//! A member's standby key's certificate (#595): issued while the member follows, into a store of its
//! own and under the account sealed with the standby keys, so the member's turn certificate stays; a
//! promotion takes the certificate into the member's store and the account into the turn's keys, and
//! the promoted start serves it with no order and renews it under the same account.

use super::*;

use std::path::Path;
use std::sync::Arc;

use tokio::sync::watch;

use crate::clock::SystemClock;
use crate::node::member::standby::certificate;
use crate::standby_key::StandbyKeys;
use crate::tee::certificates::CertificateStore;
use crate::tee::sealing::MockSealer;
use crate::test_support::scratch_dir;
use crate::tls::acme::tests::fake_ca::FakeCa;
use crate::tls::acme::tests::sealed::{sealed_store, sealed_task, started_under, stored};

/// The release the member's keys are sealed for: its measurement and config hash.
const LAUNCH: [u8; 48] = [0x33; 48];
const CONFIG: [u8; 32] = [0xc1; 32];

#[tokio::test(start_paused = true)]
async fn a_standby_certificate_leaves_the_turns_and_serves_a_promoted_start_from_its_first_answer() {
    let ca = FakeCa::new(DOMAIN);
    let member = sealed_store("acme-standby");
    let turn = sealed_task(&ca, &member, DOMAIN);
    turn.issue(&mut Progress::default()).await.unwrap();
    let turn_chain = stored(&member, &turn).expect("the turn's certificate is sealed");

    // While it follows: the standby key's own task, its account sealed with the standby keys, into the
    // standby store.
    let standby_keys = Arc::new(StandbyKeys::sealed_for_test(&member.dir, LAUNCH, CONFIG));
    let standby_store = standby_store(&member.dir);
    let standby = standby_task(&ca, &standby_keys, standby_store.clone());
    assert!(standby.restored.is_none());
    standby.issue(&mut Progress::default()).await.unwrap();
    let standby_key = standby.key.public_key_sha256();
    let directory = standby.directory_url.clone();
    let standby_served = standby.key.served();
    let standby_chain = standby_store.chain_for(DOMAIN, &standby_key, &directory).unwrap();
    assert_eq!(standby_chain, Some(standby_served.clone()), "the standby certificate is not sealed in its store");
    assert_eq!(stored(&member, &turn), Some(turn_chain.clone()), "the standby certificate replaced the turn's");
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [2, 2], "the standby used the turn's account");

    // The promotion: the certificate goes into the member's store and the standby keys, account
    // included, are resealed as the turn's, and the start serves the certificate before any order.
    assert!(member.certificates.adopt(&standby_store, DOMAIN, &standby_key, &directory).unwrap());
    drop(standby);
    let resealed = Arc::try_unwrap(standby_keys).unwrap().reseal(8).unwrap();
    let promoted = started_under(&member, resealed);
    let task = sealed_task(&ca, &promoted, DOMAIN);
    assert!(task.restored.is_some(), "the promoted start did not restore the standby certificate");
    assert_eq!(task.key.served(), standby_served);
    assert_eq!(ca.count("new-order"), 2, "the promoted start ordered");
    let unknown = [0x11; 32];
    assert!(!member.certificates.adopt(&standby_store, DOMAIN, &unknown, &directory).unwrap(), "no certificate");

    // Its renewal orders under the standby's account.
    task.issue(&mut Progress::default()).await.unwrap();
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [2, 3], "the promoted start registered again");
}

/// A following member's restart opens the account its first run registered from the standby seal and
/// orders under it, so the CA sees one account for the standby key, which a CAA `accounturi` can name.
#[tokio::test(start_paused = true)]
async fn a_standby_restart_orders_under_the_account_sealed_with_its_keys() {
    let ca = FakeCa::new(DOMAIN);
    let dir = scratch_dir("acme-standby-restart");
    let keys = Arc::new(StandbyKeys::sealed_for_test(&dir, LAUNCH, CONFIG));
    standby_task(&ca, &keys, standby_store(&dir)).issue(&mut Progress::default()).await.unwrap();
    let sealed = keys.account().expect("the account is not held with the standby keys");
    assert_eq!(sealed.directory_url, "https://ca.test/dir");

    // The restart: the keys, and the account with them, opened from the standby seal.
    let reopened = Arc::new(StandbyKeys::reopened_for_test(&dir, LAUNCH, CONFIG));
    assert!(reopened.account() == Some(sealed), "the standby seal does not hold the account");
    let restarted = standby_task(&ca, &reopened, standby_store(&dir));
    assert!(restarted.restored.is_some(), "the restart did not restore the standby certificate");
    restarted.issue(&mut Progress::default()).await.unwrap();
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [1, 2], "the restart registered another account");
}

// ── Helpers ──

/// The standby certificate store sealed in `dir` with the mock stand-in.
fn standby_store(dir: &Path) -> Arc<CertificateStore> {
    Arc::new(CertificateStore::open_standby(dir, Box::new(MockSealer { launch_digest: LAUNCH }), CONFIG).unwrap())
}

/// The standby key's certificate task over `keys`' TLS key, keeping its account in `keys` and its
/// certificate in `certificates`, as the standby side configures it (`certificate::sealed`).
fn standby_task(ca: &FakeCa, keys: &Arc<StandbyKeys>, certificates: Arc<CertificateStore>) -> CertificateTask<FakeCa> {
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
        sealed: Some(certificate::sealed(keys, &certificates, trusted).erased()),
        restored: None,
    };
    task.restored = task.restore();
    task
}
