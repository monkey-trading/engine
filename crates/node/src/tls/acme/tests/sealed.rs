//! The account and certificate kept in the sealed keys: a restart serves the certificate it had,
//! orders nothing until renewal, and renews under the same account.

use super::*;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use crate::clock::test_clock::TestClock;
use crate::clock::{Clock, SystemClock};
use crate::tee::keys::KeySet;
use crate::tee::sealing::{KeyStore, MockSealer, file_for};
use crate::test_support::scratch_dir;
use crate::tls::acme::tests::fake_ca::FakeCa;
use crate::trusted_time::Sample;

#[tokio::test(start_paused = true)]
async fn a_restart_serves_the_sealed_certificate_and_renews_under_the_sealed_account() {
    let ca = FakeCa::new(DOMAIN);
    let store = sealed_store("acme-sealed-restart");
    let first = sealed_task(&ca, &store, DOMAIN);
    assert!(first.restored.is_none(), "a fresh store restored a certificate");
    let issued = first.issue(&mut Progress::default()).await.unwrap();
    let sealed = store.keys();
    assert_eq!(sealed.acme_account.as_ref().map(|account| account.directory_url.as_str()), Some("https://ca.test/dir"));
    assert_eq!(
        sealed.certificate_chain("https://ca.test/dir").unwrap(),
        Some(first.key.served()),
        "the served chain is not sealed"
    );

    // The restart: the same TLS key from the store, serving the sealed chain before any order.
    let restarted = sealed_task(&ca, &store, DOMAIN);
    let restored = restarted.restored.as_ref().expect("the sealed certificate was not restored");
    assert_eq!(restored.lifetime, issued.lifetime);
    assert_eq!(restarted.key.served(), first.key.served());
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [1, 1], "the restart ordered");

    // Its renewal uses the sealed account: no second registration.
    restarted.issue(&mut Progress::default()).await.unwrap();
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [1, 2]);
    assert_eq!(
        store.keys().certificate_chain("https://ca.test/dir").unwrap(),
        Some(restarted.key.served()),
        "the renewal is not sealed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_sealed_account_the_ca_forgot_is_dropped_and_registered_again() {
    let ca = FakeCa::new(DOMAIN);
    let store = sealed_store("acme-sealed-forgotten");
    sealed_task(&ca, &store, DOMAIN).issue(&mut Progress::default()).await.unwrap();
    let forgotten = store.keys().acme_account.expect("the account is sealed").credentials;

    ca.state().forget_account = true;
    let restarted = sealed_task(&ca, &store, DOMAIN);
    let mut progress = Progress::default();
    let err = restarted.issue(&mut progress).await.unwrap_err();
    assert!(format!("{err:#}").contains("accountDoesNotExist"), "{err:#}");
    assert!(store.keys().acme_account.is_none(), "the account the CA forgot stayed sealed");
    restarted.issue(&mut progress).await.unwrap();
    assert_eq!(ca.count("new-account"), 2, "registered again rather than retrying the forgotten account");
    assert_ne!(store.keys().acme_account.unwrap().credentials, forgotten);
}

#[tokio::test(start_paused = true)]
async fn a_forgotten_account_is_not_restored_again_while_its_removal_cannot_be_sealed() {
    let ca = FakeCa::new(DOMAIN);
    let (store, dir) = sealed_store_in("acme-sealed-forgotten-unwritable");
    sealed_task(&ca, &store, DOMAIN).issue(&mut Progress::default()).await.unwrap();
    // A directory where the sealed file's replacement goes makes every write fail.
    std::fs::create_dir(file_for(&dir, &store.release(), store.generation()).with_extension("partial")).unwrap();

    ca.state().forget_account = true;
    let restarted = sealed_task(&ca, &store, DOMAIN);
    let mut progress = Progress::default();
    restarted.issue(&mut progress).await.unwrap_err();
    assert!(store.keys().acme_account.is_some(), "the removal was sealed, though writes fail");
    restarted.issue(&mut progress).await.unwrap();
    assert_eq!(ca.count("new-account"), 2, "the account the CA forgot was restored again");
}

#[tokio::test(start_paused = true)]
async fn a_sealed_certificate_for_another_domain_is_not_served() {
    let ca = FakeCa::new(DOMAIN);
    let store = sealed_store("acme-sealed-other-domain");
    sealed_task(&ca, &store, DOMAIN).issue(&mut Progress::default()).await.unwrap();

    let renamed = sealed_task(&ca, &store, "other.engine.test");
    assert!(renamed.restored.is_none(), "a certificate for api.engine.test serves other.engine.test");
    assert_eq!(renamed.key.served().len(), 1, "not the self-signed certificate");
}

#[tokio::test(start_paused = true)]
async fn a_certificate_another_directory_issued_is_not_served_and_a_new_one_is_ordered() {
    let ca = FakeCa::new(DOMAIN);
    let store = sealed_store("acme-sealed-other-directory");
    sealed_task(&ca, &store, DOMAIN).issue(&mut Progress::default()).await.unwrap();

    // The staging directory's certificate, then the production directory in the config.
    let production = sealed_task_at(&ca, &store, DOMAIN, "https://ca.test/production");
    assert!(production.restored.is_none(), "the staging CA's certificate serves the production config");
    assert_eq!(production.key.served().len(), 1, "not the self-signed certificate");

    // It registers an account of its own there and orders, sealing both for that directory.
    production.issue(&mut Progress::default()).await.unwrap();
    assert_eq!([ca.count("new-account"), ca.count("new-order")], [2, 2], "the staging account was reused");
    let sealed = store.keys();
    let account = sealed.acme_account.as_ref().map(|account| account.directory_url.as_str());
    assert_eq!(account, Some("https://ca.test/production"));
    assert_eq!(sealed.certificate_chain("https://ca.test/production").unwrap(), Some(production.key.served()));
    assert_eq!(sealed.certificate_chain("https://ca.test/dir").unwrap(), None, "the staging chain stayed sealed");
}

#[tokio::test(start_paused = true)]
async fn a_restored_certificate_renews_on_its_age_by_trusted_time() {
    let ca = FakeCa::new(DOMAIN);
    let store = sealed_store("acme-sealed-age");
    sealed_task(&ca, &store, DOMAIN).issue(&mut Progress::default()).await.unwrap();
    let restored = sealed_task(&ca, &store, DOMAIN).restored.unwrap();
    let not_before_ms = (restored.not_after_seconds - restored.lifetime.as_secs() as i64) * 1000;

    // Ten days into its life by trusted time, whatever the monotonic clock says.
    let now_ms = not_before_ms + 10 * DAY.as_millis() as i64;
    let clock = TestClock::at(now_ms);
    let sample = Sample { at: clock.instant(), earliest_ms: now_ms, latest_ms: now_ms };
    let trusted = TrustedClock::new(Arc::new(clock), watch::channel(Some(sample)).1, Duration::from_secs(1));
    assert_eq!(age_at(&trusted, &restored).await, 10 * DAY);

    let (logs, _guard) = Logs::capture();
    let pending = Gauge::default();
    pending.set(1);
    let (schedule, wait) =
        Schedule::restored(DOMAIN.into(), None, pending.clone(), restored.lifetime, 10 * DAY, Instant::now());
    assert_eq!(wait, restored.lifetime * 2 / 3 - 10 * DAY, "renewal at a third of its life left");
    assert_eq!(pending.get(), 0, "a restored certificate is not pending");
    // A pending episode an earlier process left open ends.
    assert_eq!(logs.count("alert_resolved=\"certificate_pending\""), 1);
    drop(schedule);
}

#[tokio::test(start_paused = true)]
async fn a_restored_certificate_past_its_renewal_point_renews_at_once_and_alerts_as_it_expires() {
    let (logs, _guard) = Logs::capture();
    let now = Instant::now();
    let (mut schedule, wait) = Schedule::restored(DOMAIN.into(), None, Gauge::default(), 90 * DAY, 80 * DAY, now);
    assert_eq!(wait, Duration::ZERO);
    assert_eq!(logs.warnings("certificate_pending"), 0, "a restored certificate raised certificate_pending");
    schedule.failed(&anyhow!("refused"), now);
    assert_eq!(logs.warnings("certificate_expiring"), 1, "ten days left and renewal failing");
    // A life that ran out while the node was down is renewed at once too.
    let (_, wait) = Schedule::restored(DOMAIN.into(), None, Gauge::default(), 90 * DAY, 100 * DAY, now);
    assert_eq!(wait, Duration::ZERO);
}

// ── Helpers ──

/// Fresh keys sealed with the mock stand-in.
fn sealed_store(name: &str) -> Arc<KeyStore> {
    sealed_store_in(name).0
}

/// `sealed_store`, and the directory it seals in.
fn sealed_store_in(name: &str) -> (Arc<KeyStore>, PathBuf) {
    let (dir, sealer) = (scratch_dir(name), MockSealer { launch_digest: [0x33; 48] });
    (Arc::new(KeyStore::create(&dir, &sealer, [0xc1; 32], 1, KeySet::generate().unwrap()).unwrap()), dir)
}

/// A certificate task for `domain` over the store's TLS key, keeping its account and certificate
/// in the store, as a start configures it.
fn sealed_task(ca: &FakeCa, store: &Arc<KeyStore>, domain: &str) -> CertificateTask<FakeCa> {
    sealed_task_at(ca, store, domain, "https://ca.test/dir")
}

/// `sealed_task` with `directory_url` configured.
fn sealed_task_at(ca: &FakeCa, store: &Arc<KeyStore>, domain: &str, directory_url: &str) -> CertificateTask<FakeCa> {
    let tls = TlsIdentity::new(store.keys().tls_key().unwrap()).unwrap();
    let trusted = TrustedClock::new(Arc::new(SystemClock), watch::channel(None).1, Duration::from_secs(1));
    let mut task = CertificateTask {
        domain: domain.into(),
        directory_url: directory_url.into(),
        contact: "mailto:ops@engine.test".into(),
        renew_before: None,
        http: ca.clone(),
        key: tls.key,
        not_after: FloatGauge::default(),
        pending: Gauge::default(),
        sealed: Some(Sealed { store: store.clone(), trusted }),
        restored: None,
    };
    task.restored = task.restore();
    task
}
