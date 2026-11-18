//! The standby key's certificate: served on the API's listener while the member follows, kept apart
//! from the turn's, and taken into the member's store by a promotion.

use rustls::pki_types::CertificateDer;
use sha2::{Digest as _, Sha256};
use x509_parser::prelude::{FromDer, X509Certificate};

use super::*;
use crate::metrics::Metrics;
use crate::test_support::{dev_config, scratch_dir};
use crate::tls::TlsIdentity;

const DOMAIN: &str = "api.engine.test";
const DIRECTORY: &str = "https://127.0.0.1:9/dir";
const DIGEST: [u8; 48] = [0x6d; 48];
/// The standby TLS key's hash, and the turn's.
const STANDBY: [u8; 32] = [0x5b; 32];
const TURN: [u8; 32] = [0x7e; 32];

/// A promoted start serves the certificate its standby obtained, a restart keeps whatever the member's
/// store holds for the key since, and the standby's store never held the member's turn certificate.
#[test]
fn a_promoted_start_takes_the_standby_certificate_into_the_members_store() {
    let (cfg, start) = sealed("standby-certificate-promoted");
    let turn_chain: Vec<CertificateDer<'static>> = vec![vec![0x7a; 48].into()];
    let standby_chain: Vec<CertificateDer<'static>> = vec![vec![0x5a; 48].into()];
    let main = crate::node::keys::certificates(&cfg, &start).unwrap().expect("a store with [api.tls.acme]");
    main.replace(DOMAIN, &TURN, DIRECTORY, &turn_chain).unwrap();
    let standby = store(&cfg, &start).expect("a standby store with [api.tls.acme]");
    standby.replace(DOMAIN, &STANDBY, DIRECTORY, &standby_chain).unwrap();
    let reopened = crate::node::keys::certificates(&cfg, &start).unwrap().unwrap();
    let held = reopened.chain_for(DOMAIN, &TURN, DIRECTORY).unwrap();
    assert_eq!(held, Some(turn_chain), "the standby certificate replaced the turn's");

    promoted(&cfg, &start, &[0x11; 32]);
    let unchanged = crate::node::keys::certificates(&cfg, &start).unwrap().unwrap();
    assert!(unchanged.chain_for(DOMAIN, &TURN, DIRECTORY).unwrap().is_some(), "no standby certificate for that key");

    promoted(&cfg, &start, &STANDBY);
    let adopted = crate::node::keys::certificates(&cfg, &start).unwrap().unwrap();
    assert_eq!(adopted.chain_for(DOMAIN, &STANDBY, DIRECTORY).unwrap(), Some(standby_chain));

    // A restart of the promoted member, after its task renewed the certificate, keeps the renewal.
    let renewed: Vec<CertificateDer<'static>> = vec![vec![0x6a; 48].into()];
    adopted.replace(DOMAIN, &STANDBY, DIRECTORY, &renewed).unwrap();
    promoted(&cfg, &start, &STANDBY);
    let restarted = crate::node::keys::certificates(&cfg, &start).unwrap().unwrap();
    assert_eq!(restarted.chain_for(DOMAIN, &STANDBY, DIRECTORY).unwrap(), Some(renewed), "the restart undid a renewal");
}

/// While the member follows, the API's address answers TLS under the standby key, with no API behind
/// it, so the CA's validation of the member's hostname reaches the key its certificate is for.
#[tokio::test]
async fn a_following_member_serves_its_standby_key_on_the_apis_listener() {
    let (mut cfg, start) = sealed("standby-certificate-listener");
    cfg.node.listen = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let identity = TlsIdentity::generate().unwrap();
    let standby_key = identity.public_key_sha256;
    let sealed = crate::tls::acme::Sealed {
        store: None::<Arc<crate::standby_key::StandbyKeys>>,
        certificates: store(&cfg, &start).unwrap(),
        trusted: crate::trusted_time::TrustedClock::new(
            Arc::new(crate::clock::SystemClock),
            tokio::sync::watch::channel(None).1,
            Duration::from_secs(1),
        ),
    };
    let task = CertificateTask::configured(&cfg.api, identity.key, &Metrics::new(), Some(sealed)).unwrap().unwrap();
    let running = tokio::spawn(run(cfg.node.listen, identity.acceptors, cfg.api.clone(), task));
    let client = reqwest::Client::builder().danger_accept_invalid_certs(true).tls_info(true).build().unwrap();
    let url = format!("https://{}/v1/health", cfg.node.listen);
    let mut tries = 0;
    let response = loop {
        match client.get(&url).send().await {
            Ok(response) => break response,
            Err(_) if tries < 250 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("the API's address served nothing while the member follows: {e}"),
        }
    };
    assert_eq!(response.status(), 404, "a following member serves no API");
    let info = response.extensions().get::<reqwest::tls::TlsInfo>().expect("TLS info");
    let leaf = info.peer_certificate().expect("a certificate");
    let (_, leaf) = X509Certificate::from_der(leaf).unwrap();
    let presented: [u8; 32] = Sha256::digest(leaf.public_key().raw).into();
    assert_eq!(presented, standby_key, "the API's address serves another key than the standby's");
    running.abort();
}

// ── Helpers ──

/// A dev config sealing under the mock TEE in a fresh directory, with `[api.tls.acme]`, and the start
/// flags that open its sealing.
fn sealed(name: &str) -> (NodeConfig, KeyStart) {
    let data = scratch_dir(name);
    std::fs::create_dir_all(data.join("keys")).unwrap();
    let mut cfg = dev_config();
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(toml::from_str(&format!("dir = {:?}", data.join("keys").display().to_string())).unwrap());
    let acme =
        format!("[acme]\ndomain = '{DOMAIN}'\ndirectory_url = '{DIRECTORY}'\ncontact = 'mailto:ops@engine.test'");
    cfg.api.tls = Some(toml::from_str(&acme).unwrap());
    (cfg, KeyStart { mock_launch_digest: Some(DIGEST), ..Default::default() })
}
