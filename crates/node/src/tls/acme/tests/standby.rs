//! A member's standby key's certificate (#595): issued while the member follows, into a store of its
//! own, so the member's turn certificate stays; a promotion takes it into the member's store, and the
//! promoted start serves it with no order.

use super::*;

use std::sync::Arc;

use tokio::sync::watch;

use crate::clock::SystemClock;
use crate::tee::certificates::CertificateStore;
use crate::tee::keys::KeySet;
use crate::tee::sealing::MockSealer;
use crate::tls::acme::tests::fake_ca::FakeCa;
use crate::tls::acme::tests::sealed::{next_turn, sealed_store, sealed_task, stored};

#[tokio::test(start_paused = true)]
async fn a_standby_certificate_leaves_the_turns_and_serves_a_promoted_start_from_its_first_answer() {
    let ca = FakeCa::new(DOMAIN);
    let member = sealed_store("acme-standby");
    let turn = sealed_task(&ca, &member, DOMAIN);
    turn.issue(&mut Progress::default()).await.unwrap();
    let turn_chain = stored(&member, &turn).expect("the turn's certificate is sealed");

    // While it follows: the standby key's own task, its account unsealed, into the standby store.
    let standby_keys = KeySet::generate().unwrap();
    let standby_store = Arc::new(
        CertificateStore::open_standby(&member.dir, Box::new(MockSealer { launch_digest: [0x33; 48] }), [0xc1; 32])
            .unwrap(),
    );
    let standby = standby_task(&ca, &standby_keys, standby_store.clone());
    assert!(standby.restored.is_none());
    standby.issue(&mut Progress::default()).await.unwrap();
    let standby_key = standby.key.public_key_sha256();
    let directory = standby.directory_url.clone();
    let standby_chain = standby_store.chain_for(DOMAIN, &standby_key, &directory).unwrap();
    assert_eq!(standby_chain, Some(standby.key.served()), "the standby certificate is not sealed in its store");
    assert_eq!(stored(&member, &turn), Some(turn_chain.clone()), "the standby certificate replaced the turn's");
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [2, 2], "the standby keeps no sealed account");

    // The promotion: the certificate goes into the member's store, and the start under the resealed
    // standby keys serves it before any order.
    assert!(member.certificates.adopt(&standby_store, DOMAIN, &standby_key, &directory).unwrap());
    let promoted = next_turn(&member, standby_keys);
    let task = sealed_task(&ca, &promoted, DOMAIN);
    assert!(task.restored.is_some(), "the promoted start did not restore the standby certificate");
    assert_eq!(task.key.served(), standby.key.served());
    assert_eq!(ca.count("new-order"), 2, "the promoted start ordered");
    let unknown = [0x11; 32];
    assert!(!member.certificates.adopt(&standby_store, DOMAIN, &unknown, &directory).unwrap(), "no certificate");
}

// ── Helpers ──

/// The standby key's certificate task over `keys`' TLS key, keeping its certificate in `certificates`
/// and no account, as the standby side configures it.
fn standby_task(ca: &FakeCa, keys: &KeySet, certificates: Arc<CertificateStore>) -> CertificateTask<FakeCa> {
    let tls = TlsIdentity::new(keys.tls_key().unwrap()).unwrap();
    let trusted = TrustedClock::new(Arc::new(SystemClock), watch::channel(None).1, Duration::from_secs(1));
    let mut task = CertificateTask {
        domain: DOMAIN.into(),
        directory_url: "https://ca.test/dir".into(),
        contact: "mailto:ops@engine.test".into(),
        renew_before: None,
        http: ca.clone(),
        key: tls.key,
        not_after: FloatGauge::default(),
        pending: Gauge::default(),
        sealed: Some(Sealed { store: None, certificates, trusted }),
        restored: None,
    };
    task.restored = task.restore();
    task
}
