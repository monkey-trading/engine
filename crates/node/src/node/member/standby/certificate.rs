//! The standby key's public certificate (#595), obtained while the member follows. With
//! `[api.tls.acme]`, the member serves its standby TLS key on the API's listener, with no API behind
//! it, so the CA's TLS-ALPN-01 validation of the member's hostname reaches it, and keeps a CA
//! certificate for that key in the standby certificate store, renewed as the API's is
//! (`tls::acme`), under the ACME account sealed with the standby keys. A promotion copies it into the
//! member's certificate store before the promoted member serves, so browsers and following members,
//! which verify the operator's certificate against public roots, reach it from its first answer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::config::{ApiSection, NodeConfig};
use crate::limits::{ApiLimits, run_retain};
use crate::node::KeyStart;
use crate::standby_key::StandbyKeys;
use crate::tee::certificates::CertificateStore;
use crate::tls::acme::{CertificateTask, Sealed};
use crate::tls::{Acceptors, serve};
use crate::trusted_time::TrustedClock;

/// How long a member waits before binding the API's listener again after it failed.
const BIND_RETRY: Duration = Duration::from_secs(5);

/// This release's standby certificate store, with `[tee.sealing]` and `[api.tls.acme]`; `None`
/// without either, or, with Warning `standby_certificates_unreadable`, when it does not open: the
/// member stays promotable, and a promotion serves the self-signed certificate until the CA issues one.
pub(crate) fn store(cfg: &NodeConfig, start: &KeyStart) -> Option<Arc<CertificateStore>> {
    let (Some(sealing), Some(_)) = (&cfg.tee.sealing, &cfg.api.tls) else { return None };
    let opened = crate::node::keys::sealer(cfg.tee.provider, start)
        .and_then(|sealer| CertificateStore::open_standby(&sealing.dir, sealer, cfg.file_sha256));
    opened
        .inspect_err(|e| {
            warn!(
                alert = "standby_certificates_unreadable",
                "the standby certificate store does not open: no standby certificate is obtained: {e:#}"
            )
        })
        .ok()
        .map(Arc::new)
}

/// Where the standby key's certificate task keeps its ACME account, the standby `keys`, and its
/// certificate, the standby store `certificates`, with the member's `trusted` time.
pub(crate) fn sealed(
    keys: &Arc<StandbyKeys>,
    certificates: &Arc<CertificateStore>,
    trusted: TrustedClock,
) -> Sealed<StandbyKeys> {
    Sealed { store: Some(keys.clone()), certificates: certificates.clone(), trusted }
}

/// Serves the standby TLS key, through `acceptors`, on `listen` with no API, under `api`'s limits, and
/// runs `task`, which obtains and renews its certificate; until dropped. A listener that does not
/// bind is tried again every `BIND_RETRY`.
pub(super) async fn run(listen: SocketAddr, acceptors: Acceptors, api: ApiSection, task: CertificateTask) {
    let listener = loop {
        match TcpListener::bind(listen).await {
            Ok(listener) => break listener,
            Err(e) => {
                warn!(%listen, "binding the API's listener for the standby certificate failed, trying again: {e}");
                tokio::time::sleep(BIND_RETRY).await;
            }
        }
    };
    info!(%listen, "answering the CA's validation for the standby key's certificate");
    let limits = Arc::new(ApiLimits::new(&api));
    let retained = limits.clone();
    let ended = tokio::select! {
        served = serve(listener, acceptors, Router::new(), limits, api) => served,
        retained = run_retain(retained) => retained,
        issued = task.run() => issued,
    };
    warn!("the standby certificate stopped: {ended:?}");
}

/// Copies the certificate the standby obtained for its TLS key, whose DER SubjectPublicKeyInfo
/// hashes to `tls_key_sha256`, into the member's certificate store, for a promoted start, unless that
/// store holds one for the key already (a restart's, or a renewal's since): its certificate task then
/// serves it from the first answer. A failure leaves the self-signed certificate until the CA issues
/// one (`certificate_pending`).
pub(crate) fn promoted(cfg: &NodeConfig, start: &KeyStart, tls_key_sha256: &[u8; 32]) {
    let Some(tls) = &cfg.api.tls else { return };
    let (domain, directory) = (&tls.acme.domain, &tls.acme.directory_url);
    let adopted = crate::node::keys::certificates(cfg, start).and_then(|main| {
        let Some(main) = main else { return Ok(None) };
        if main.chain_for(domain, tls_key_sha256, directory)?.is_some() {
            return Ok(Some(false));
        }
        let Some(standby) = store(cfg, start) else { return Ok(None) };
        main.adopt(&standby, domain, tls_key_sha256, directory).map(|adopted| adopted.then_some(true))
    });
    match adopted {
        Ok(Some(true)) => info!(domain = %domain, "the promoted member serves its standby key's certificate"),
        Ok(Some(false)) => info!(domain = %domain, "the promoted member's store holds a certificate for its key"),
        Ok(None) => warn!(
            domain = %domain,
            "the standby obtained no certificate for its key: the promoted member orders one"
        ),
        Err(e) => warn!("copying the standby key's certificate failed: the promoted member orders one: {e:#}"),
    }
}

#[cfg(test)]
mod tests;
