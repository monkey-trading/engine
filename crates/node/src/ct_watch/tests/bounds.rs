//! Bounds of the poll flow: the log's and the clock's untrusted values, the state file's binding, and
//! the time budgets.

use super::*;

use crate::alerts::capture_until_dropped;
use crate::ct_watch::Watcher;
use crate::ct_watch::attestation::{Poll, classify};
use crate::ct_watch::config::WatchConfig;
use crate::ct_watch::state::{Finding, MAX_FINDINGS};

use super::harness::*;

#[tokio::test]
async fn a_replayed_old_tree_head_does_not_end_an_outage() {
    let (_guard, mut router) = capture_until_dropped();
    let mut h = harness("ct-flow-stale-head", Vec::new(), Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    h.log.set_fault(Fault::ServerError);
    h.pass(T0 + 3_600_000).await;
    assert_eq!(alerts(&mut router), [fired("ct_log_unreachable", None)]);
    // The log answers again, but only with the head it signed at T0.
    h.log.set_fault(Fault::None);
    h.log.shared.lock().unwrap().sth_timestamp = T0;
    h.logs_at(T0 + 3_660_000).await;
    h.settle_at(T0 + 3_660_000);
    assert!(
        router.take().iter().all(|line| line.name != "ct_log_unreachable" || line.severity.is_some()),
        "a replayed head resolved the outage"
    );
    h.pass(T0 + 3_720_000).await;
    assert!(router.take().iter().any(|line| line.name == "ct_log_unreachable" && line.severity.is_none()));
}

#[tokio::test]
async fn a_tree_size_near_the_integer_limit_does_not_stop_the_watcher() {
    let mut h = harness("ct-flow-huge-tree", Vec::new(), None, true).await;
    h.node.attest([5; 32]);
    h.log.shared.lock().unwrap().tree_size = Some(u64::MAX - 1);
    h.pass(T0).await;
    assert_eq!(h.next_index(), Some(u64::MAX - 1));
    h.log.shared.lock().unwrap().tree_size = Some(u64::MAX);
    h.pass(T0 + 1_000).await;
    assert_eq!(h.next_index(), Some(u64::MAX - 1), "the unreadable batch is left unread");
}

/// The config with HTTPS URLs, which a strict (non-mock) run accepts; nothing connects in
/// `Watcher::new`.
fn strict(config: &str) -> WatchConfig {
    toml::from_str(&config.replace("http://", "https://")).unwrap()
}

#[tokio::test]
async fn a_state_file_keeps_the_trust_mode_it_was_created_in() {
    // Created by a mock run: a strict run refuses it.
    let h = harness("ct-flow-trust-mock", Vec::new(), Some(0), true).await;
    let Harness { mut watcher, log, node, config, .. } = h;
    watcher.settle().unwrap();
    drop(watcher);
    let err =
        Watcher::new(&strict(&config), false, fixed(T0)).err().expect("mock keys are not trusted by a strict run");
    assert!(format!("{err:#}").contains("holds keys from mock attestations"), "{err:#}");
    assert!(Watcher::new(&toml::from_str(&config).unwrap(), true, fixed(T0)).is_ok());

    // Created by a strict run: a mock run refuses it.
    let h = harness("ct-flow-trust-strict", Vec::new(), Some(0), true).await;
    let Harness { watcher, config: other, .. } = h;
    drop(watcher);
    let mut watcher = Watcher::new(&strict(&other), false, fixed(T0)).unwrap();
    watcher.settle().unwrap();
    drop(watcher);
    let err =
        Watcher::new(&toml::from_str(&other).unwrap(), true, fixed(T0)).err().expect("a strict state is not mock");
    assert!(format!("{err:#}").contains("holds keys from verified attestations"), "{err:#}");
    drop((log, node));
}

#[tokio::test]
async fn a_state_file_belongs_to_the_domain_it_was_created_for() {
    let h = harness("ct-flow-domain", Vec::new(), Some(0), true).await;
    let Harness { mut watcher, config, .. } = h;
    watcher.settle().unwrap();
    drop(watcher);
    let other = config.replace(&format!("domain = \"{DOMAIN}\""), "domain = \"other.example.com\"");
    let err = Watcher::new(&toml::from_str(&other).unwrap(), true, fixed(T0)).err().expect("another domain");
    assert!(format!("{err:#}").contains("was kept for api.example.com"), "{err:#}");
    assert!(Watcher::new(&toml::from_str(&config).unwrap(), true, fixed(T0)).is_ok());
}

#[tokio::test]
async fn a_tree_head_dated_ahead_of_the_clock_never_counts_as_a_read() {
    let (_guard, mut router) = capture_until_dropped();
    let mut h = harness("ct-flow-future-head", Vec::new(), Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    // The log signs a head a day ahead and serves it unchanged from then on.
    let ahead = T0 + 86_400_000;
    for polled_at in [T0 + 60_000, T0 + 1_800_000, T0 + 3_600_000] {
        h.log.shared.lock().unwrap().sth_timestamp = ahead;
        h.attestation_at(polled_at).await;
        h.logs_at(polled_at).await;
        h.settle_at(polled_at);
    }
    assert_eq!(alerts(&mut router), [fired("ct_log_unreachable", None)], "the future head hid the outage");
}

#[tokio::test]
async fn a_findings_bound_that_is_full_leaves_the_log_unread_until_room_returns() {
    let mut h = harness("ct-flow-full", Vec::new(), Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    let filler = Finding {
        log: "log".into(),
        index: 0,
        issuer: "CN=x".into(),
        key_sha256: "aa".repeat(32),
        not_after_ms: u64::MAX,
        decide_at_ms: u64::MAX,
        alerted_at_ms: None,
        resolving: false,
    };
    h.watcher.store.tracker.state.findings = vec![filler; MAX_FINDINGS];
    let foreign = cert(&[DOMAIN], "Issuer");
    h.log.push(x509_leaf(T0, &foreign.der));
    h.pass(T0 + 1_000).await;
    assert_eq!(h.next_index(), Some(0), "the log moved past a certificate it could not hold");
    assert_eq!(h.watcher.store.tracker.state.findings.len(), MAX_FINDINGS);

    h.watcher.store.tracker.state.findings.clear();
    h.pass(T0 + 2_000).await;
    assert_eq!(h.next_index(), Some(1));
    assert_eq!(h.watcher.store.tracker.state.findings.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_log_read_that_uses_its_budget_is_cut_off_and_left_behind() {
    let mut h = harness("ct-flow-log-budget", Vec::new(), Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    h.log.set_fault(Fault::SlowHead);
    h.watcher.log_budget = std::time::Duration::from_secs(2);
    let started = tokio::time::Instant::now();
    assert!(h.logs_at(T0 + 1_000).await, "a cut-off log is still behind");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
}

#[tokio::test(start_paused = true)]
async fn an_attestation_poll_is_cut_off_at_half_its_interval() {
    let h = harness("ct-flow-attest-budget", Vec::new(), Some(0), true).await;
    let Harness { watcher, log, node, config, .. } = h;
    drop(watcher);
    // A node that accepts connections and never answers.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_url = format!("http://{}", silent.local_addr().unwrap());
    let holder = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok(connection) = silent.accept().await {
            held.push(connection);
        }
    });
    let config = config
        .replace(&node.url, &silent_url)
        .replace("attestation_interval = \"1m\"", "attestation_interval = \"10s\"");
    let mut watcher = open(&config, true, fixed(T0));
    let started = tokio::time::Instant::now();
    watcher.poll_attestation().await;
    assert!(started.elapsed() <= std::time::Duration::from_secs(6), "{:?}", started.elapsed());
    assert!(watcher.store.tracker.state.attested.is_empty());
    holder.abort();
    drop((log, node));
}

#[test]
fn a_vendor_outage_is_unavailable_and_any_other_failure_is_a_rejection() {
    use crate::verify::CollateralUnavailable;
    let outage = anyhow::Error::new(CollateralUnavailable("https://kds answered 503".into()))
        .context("fetching the issuer chain");
    assert!(matches!(classify(Err(outage)), Poll::Unreachable(_)), "a context-wrapped outage stays an outage");
    assert!(matches!(classify(Err(anyhow::anyhow!("the quote's signature does not verify"))), Poll::Rejected(_)));
    assert!(matches!(classify(Ok([3; 32])), Poll::Verified(key) if key == [3; 32]));
}
