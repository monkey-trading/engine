//! A node whose key the vault does not name — new keys before governance registers them, or keys
//! the vault has since retired — serves only `/v1/attestation` and waits. Governance verifies that
//! attestation and has the timelock register the key; the node then starts as usual. It signs
//! nothing while it waits, so a retired key restored by the host never signs.

use std::sync::Arc;

use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::{error, info};

use crate::checkpoints::setup::{Setup, Unregistered, connect};
use crate::config::NodeConfig;
use crate::limits::{ApiLimits, run_retain};
use crate::tee::{Attestation, EnclaveKey};
use crate::tls::{Acceptors, serve};

/// The checkpoint setup, once the vault names this enclave's key; `None` without `[checkpoints]`.
/// Until then it serves the attestation on `node.listen` under `signer_unregistered`, and checks
/// the vault again every `checkpoints.retry_interval`.
/// Error cases:
/// - checkpoint setup fails for any other reason (see `checkpoints::setup::connect`)
/// - the API's listener cannot be bound while it waits
pub(super) async fn registered(
    cfg: &NodeConfig,
    key: &Arc<EnclaveKey>,
    acceptors: &Acceptors,
    attestation: &Arc<Attestation>,
) -> anyhow::Result<Option<Setup>> {
    wait(cfg, &key.address_hex(), acceptors, attestation, || connect(cfg, key)).await
}

/// `registered`, setting the checkpoints up with `connect`.
/// Error cases:
/// - see `registered`
async fn wait<F: Future<Output = anyhow::Result<Option<Setup>>>>(
    cfg: &NodeConfig,
    enclave: &str,
    acceptors: &Acceptors,
    attestation: &Arc<Attestation>,
    mut connect: impl FnMut() -> F,
) -> anyhow::Result<Option<Setup>> {
    let mut serving: Option<JoinHandle<anyhow::Result<()>>> = None;
    let outcome = loop {
        let unregistered = match connect().await {
            Err(e) => match e.downcast::<Unregistered>() {
                Ok(unregistered) => unregistered,
                Err(e) => break Err(e),
            },
            Ok(setup) => break Ok(setup),
        };
        if serving.is_none() {
            error!(
                alert = "signer_unregistered",
                enclave = %enclave,
                "{unregistered}: serving only /v1/attestation until governance registers this key"
            );
            serving = Some(serve_attestation(cfg, acceptors, attestation).await?);
        }
        let retry = cfg.checkpoints.as_ref().map_or(std::time::Duration::from_secs(1), |c| c.retry_interval);
        sleep(retry).await;
    };
    if let Some(server) = serving {
        // Frees the listener for the API.
        server.abort();
        let _ = server.await;
        if outcome.is_ok() {
            info!(alert_resolved = "signer_unregistered", enclave = %enclave, "the vault names this enclave's key");
        }
    }
    outcome
}

/// Serves only the attestation on `node.listen`, under the API's limits, pruned as the API's are,
/// until aborted.
/// Error cases:
/// - the listener cannot be bound
async fn serve_attestation(
    cfg: &NodeConfig,
    acceptors: &Acceptors,
    attestation: &Arc<Attestation>,
) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let listener =
        TcpListener::bind(cfg.node.listen).await.map_err(|e| anyhow::anyhow!("binding {}: {e}", cfg.node.listen))?;
    let served = attestation.clone();
    let app =
        Router::new().route("/v1/attestation", get(move || std::future::ready(Json(Attestation::clone(&served)))));
    let limits = Arc::new(ApiLimits::new(&cfg.api));
    info!(listen = %listener.local_addr()?, "serving the attestation while the vault names another signer");
    // The limiter is pruned for as long as it serves, which may be days.
    let (acceptors, api, retained) = (acceptors.clone(), cfg.api.clone(), limits.clone());
    Ok(tokio::spawn(async move {
        tokio::select! {
            served = serve(listener, acceptors, app, limits, api) => served,
            retained = run_retain(retained) => retained,
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use engine_core::types::AccountId;
    use tokio::time::timeout;

    use super::*;
    use crate::tee::{AttestationBundle, MockAttestor, attest_bundle};
    use crate::test_support::dev_config;
    use crate::tls::TlsIdentity;
    use crate::trusted_time::tests::logs::Logs;

    #[tokio::test]
    async fn an_unregistered_key_serves_only_its_attestation_until_the_vault_names_it() {
        let (logs, _guard) = Logs::capture();
        let listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut cfg = dev_config();
        cfg.node.listen = listen;
        cfg.checkpoints = Some(
            toml::from_str(
                "rpc_url = 'http://127.0.0.1:9'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
                 retry_interval = '20ms'\nrpc_timeout = '1s'",
            )
            .unwrap(),
        );
        let tls = TlsIdentity::generate().unwrap();
        let key = EnclaveKey::generate();
        let bundle = AttestationBundle {
            enclave_address: key.address(),
            config_sha256: [1; 32],
            tls_public_key_sha256: tls.public_key_sha256,
        };
        let attestation = Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap());
        let named = Arc::new(AtomicBool::new(false));
        let vault = named.clone();
        let connect = move || {
            let named = vault.load(Ordering::SeqCst);
            async move { if named { Ok(None) } else { Err(Unregistered { vault_signer: AccountId([0xbb; 20]) }.into()) } }
        };
        let waiting = {
            let (cfg, acceptors, attestation, enclave) =
                (cfg, tls.acceptors.clone(), attestation.clone(), key.address_hex());
            tokio::spawn(async move {
                wait(&cfg, &enclave, &acceptors, &attestation, connect).await.map(|setup| setup.is_none())
            })
        };

        let client = reqwest::Client::builder().tls_danger_accept_invalid_certs(true).build().unwrap();
        let get = |path: &str| client.get(format!("https://{listen}{path}")).send();
        let served = timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(response) = get("/v1/attestation").await {
                    break response;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the attestation was never served");
        assert_eq!(served.status(), 200);
        let served: Attestation = served.json().await.unwrap();
        assert_eq!(served.enclave_address, key.address_hex());
        assert_eq!(get("/v1/markets").await.unwrap().status(), 404, "a route besides the attestation answered");
        assert!(!waiting.is_finished(), "startup went on before the vault named the key");
        assert_eq!(logs.critical("signer_unregistered"), 1);

        named.store(true, Ordering::SeqCst);
        assert!(timeout(Duration::from_secs(10), waiting).await.unwrap().unwrap().unwrap(), "no setup expected");
        assert_eq!(logs.count("alert_resolved=\"signer_unregistered\""), 1);
        assert_eq!(logs.critical("signer_unregistered"), 1, "raised once for the whole wait");
        std::net::TcpListener::bind(listen).expect("the API's listener was not freed");
    }

    #[tokio::test]
    async fn any_other_setup_failure_ends_the_wait_without_serving() {
        let mut cfg = dev_config();
        cfg.node.listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let tls = TlsIdentity::generate().unwrap();
        let attestation = Arc::new(
            attest_bundle(
                &MockAttestor,
                &AttestationBundle {
                    enclave_address: [1; 20],
                    config_sha256: [1; 32],
                    tls_public_key_sha256: tls.public_key_sha256,
                },
            )
            .unwrap(),
        );
        let failed = || async { Err(anyhow::anyhow!("the vault holds another token")) };
        let err =
            wait(&cfg, "0x01", &tls.acceptors, &attestation, failed).await.err().expect("the failure was swallowed");
        assert!(err.to_string().contains("another token"), "{err:#}");
        std::net::TcpListener::bind(cfg.node.listen).expect("a failed setup bound the API's listener");
    }
}
