//! The poll flow against loopback stand-ins for a log and a node, with the time passed in.

use super::*;

use std::sync::atomic::Ordering;

use crate::alerts::capture_until_dropped;
use crate::ct_watch::attestation::{AttestationSource, Poll};
use crate::ct_watch::config::WatchConfig;
use crate::tee::Attestation;

use super::harness::*;

#[tokio::test]
async fn only_the_certificate_with_a_foreign_key_alerts_and_only_after_its_grace() {
    let (_guard, mut router) = capture_until_dropped();
    let attested = cert(&[DOMAIN], "Issuer");
    let unrelated = cert(&["other.test"], "Issuer");
    let foreign = cert(&[DOMAIN], "Issuer");
    let leaves = vec![x509_leaf(T0, &attested.der), x509_leaf(T0, &unrelated.der), precert_leaf(T0, &foreign.der)];
    let mut h = harness("ct-flow-foreign", leaves, Some(0), true).await;
    h.node.attest(attested.key_sha256);

    h.pass(T0).await;
    h.pass(T0 + GRACE_MS - 1).await;
    assert_eq!(alerts(&mut router), [], "alerted inside the grace");
    h.pass(T0 + GRACE_MS).await;
    assert_eq!(alerts(&mut router), [fired("unattested_certificate", Some(foreign.key_sha256))]);
    // Asserted again on every pass: the router collapses the repeats into the one episode.
    h.pass(T0 + GRACE_MS + 60_000).await;
    assert_eq!(alerts(&mut router), [fired("unattested_certificate", Some(foreign.key_sha256))]);
    assert_eq!(h.next_index(), Some(3));
    assert_eq!(get_entries(&h).len(), 2, "{:?}: two entries a call", get_entries(&h));
}

#[tokio::test]
async fn a_restart_that_rotates_the_key_before_the_watcher_polls_is_accepted() {
    let (_guard, mut router) = capture_until_dropped();
    let before = cert(&[DOMAIN], "Issuer");
    let after = cert(&[DOMAIN], "Issuer");
    let mut h = harness("ct-flow-rotation", vec![x509_leaf(T0, &before.der)], Some(0), true).await;
    h.node.attest(before.key_sha256);
    h.pass(T0).await;

    // The node restarts and gets its certificate; the watcher reads the entry before it polls the
    // node's new attestation.
    h.log.push(x509_leaf(T0 + 10_000, &after.der));
    h.logs_at(T0 + 20_000).await;
    assert_eq!(h.watcher.store.tracker.state.findings.len(), 1, "held for its grace");
    h.node.attest(after.key_sha256);
    h.pass(T0 + 70_000).await;
    h.pass(T0 + 10_000 + GRACE_MS + 1).await;
    assert_eq!(alerts(&mut router), []);
    assert!(h.watcher.store.tracker.state.findings.is_empty());
    assert_eq!(h.watcher.store.tracker.state.attested.len(), 2);
}

#[tokio::test]
async fn a_restarted_watcher_resumes_its_position_and_its_open_finding() {
    let (_guard, mut router) = capture_until_dropped();
    let attested = cert(&[DOMAIN], "Issuer");
    let foreign = cert(&[DOMAIN], "Issuer");
    let leaves = vec![x509_leaf(T0, &attested.der), x509_leaf(T0, &foreign.der)];
    let mut h = harness("ct-flow-restart", leaves, Some(0), true).await;
    h.node.attest(attested.key_sha256);
    h.pass(T0).await;
    h.pass(T0 + GRACE_MS).await;
    assert_eq!(alerts(&mut router).len(), 1);
    let read = get_entries(&h).len();

    let Harness { watcher, log, node, config, time } = h;
    drop(watcher);
    time.store(T0 + GRACE_MS + 1, Ordering::SeqCst);
    let mut h = Harness { watcher: open(&config, true, clock(&time)), log, node, config, time };
    h.pass(T0 + GRACE_MS + 1_000).await;
    assert_eq!(get_entries(&h).len(), read, "an entry read before the restart is read again");
    assert_eq!(h.next_index(), Some(2));
    assert_eq!(h.watcher.store.tracker.state.findings.len(), 1, "the open finding survived");
    // Raised again, in case the page was lost; the router's resumed episode keeps a sent one from
    // paging twice.
    assert_eq!(alerts(&mut router), [fired("unattested_certificate", Some(foreign.key_sha256))]);
}

#[tokio::test]
async fn without_a_start_index_a_new_log_is_read_from_its_end() {
    let (_guard, mut router) = capture_until_dropped();
    let old = cert(&[DOMAIN], "Issuer");
    let mut h = harness("ct-flow-tip", vec![x509_leaf(T0, &old.der)], None, true).await;
    h.node.attest([5; 32]);
    h.pass(T0 + GRACE_MS * 2).await;
    assert!(get_entries(&h).is_empty());
    assert_eq!(h.next_index(), Some(1));
    let new = cert(&[DOMAIN], "Issuer");
    h.log.push(x509_leaf(T0 + GRACE_MS * 2, &new.der));
    h.pass(T0 + GRACE_MS * 4).await;
    assert_eq!(alerts(&mut router), [fired("unattested_certificate", Some(new.key_sha256))]);
}

#[tokio::test]
async fn a_log_that_fails_keeps_its_position_and_alerts_once_it_has_been_unread_too_long() {
    let (_guard, mut router) = capture_until_dropped();
    let mut h = harness("ct-flow-unreachable", Vec::new(), Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    h.log.set_fault(Fault::ServerError);
    h.pass(T0 + 3_599_000).await;
    assert_eq!(alerts(&mut router), []);
    h.pass(T0 + 3_600_000).await;
    assert_eq!(alerts(&mut router), [fired("ct_log_unreachable", None)]);
    h.log.set_fault(Fault::None);
    h.pass(T0 + 3_660_000).await;
    let resolved: Vec<_> = router.take().into_iter().map(|line| (line.name, line.severity)).collect();
    assert!(resolved.contains(&("ct_log_unreachable".to_owned(), None)), "{resolved:?}");
}

#[tokio::test]
async fn an_entry_that_does_not_parse_neither_blinds_the_log_nor_hides_a_certificate_for_the_domain() {
    let (_guard, mut router) = capture_until_dropped();
    let foreign = cert(&[DOMAIN], "Issuer");
    let mut broken_for_domain = x509_leaf(T0, &cert(&[DOMAIN], "Issuer").der);
    broken_for_domain.truncate(broken_for_domain.len() - 5);
    let leaves = vec![b"garbage".to_vec(), x509_leaf(T0, &foreign.der), broken_for_domain];
    let mut h = harness("ct-flow-malformed", leaves, Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    assert_eq!(h.next_index(), Some(3), "a malformed entry stopped the log");
    h.pass(T0 + GRACE_MS).await;
    // Asserted after each log and each pass; a set of the alerts is what the router sees as episodes.
    let raised = alerts(&mut router);
    let mut distinct = Vec::new();
    for alert in raised {
        if !distinct.contains(&alert) {
            distinct.push(alert);
        }
    }
    let raised = distinct;
    assert_eq!(raised.len(), 2, "{raised:?}");
    assert!(raised.contains(&fired("unattested_certificate", Some(foreign.key_sha256))));
}

#[tokio::test]
async fn a_tree_that_shrinks_below_the_position_is_not_trusted() {
    let (_guard, mut router) = capture_until_dropped();
    let leaves: Vec<_> = (0..3).map(|_| x509_leaf(T0, &cert(&["other.test"], "I").der)).collect();
    let mut h = harness("ct-flow-shrink", leaves, Some(0), true).await;
    h.node.attest([5; 32]);
    h.pass(T0).await;
    assert_eq!(h.next_index(), Some(3));
    h.log.shared.lock().unwrap().leaves.truncate(1);
    h.pass(T0 + 3_600_000).await;
    assert_eq!(h.next_index(), Some(3));
    assert_eq!(alerts(&mut router), [fired("ct_log_unreachable", None)]);
}

#[tokio::test]
async fn a_log_signing_with_another_key_is_never_read() {
    let (_guard, mut router) = capture_until_dropped();
    let foreign = cert(&[DOMAIN], "Issuer");
    let mut h = harness("ct-flow-wrong-key", vec![x509_leaf(T0, &foreign.der)], Some(0), true).await;
    h.node.attest([5; 32]);
    h.log.set_fault(Fault::WrongKey);
    h.pass(T0 + 3_600_000).await;
    assert!(get_entries(&h).is_empty());
    assert_eq!(alerts(&mut router), [fired("ct_log_unreachable", None)]);
}

#[tokio::test]
async fn a_mock_attestation_is_refused_unless_the_watcher_allows_it() {
    let h = harness("ct-flow-refuse-mock", vec![], Some(0), true).await;
    h.node.attest([5; 32]);
    let cfg: WatchConfig = toml::from_str(&h.config).unwrap();
    assert!(
        matches!(AttestationSource::new(&cfg.node, true).unwrap().poll().await, Poll::Verified(key) if key == [5; 32])
    );
    let refusing = AttestationSource::new(&cfg.node, false).unwrap().poll().await;
    let Poll::Rejected(err) = refusing else { panic!("{refusing:?}") };
    assert!(format!("{err:#}").contains("mock quote never verifies"), "{err:#}");
}

#[tokio::test]
async fn an_attestation_that_does_not_verify_alerts_and_names_no_key() {
    let (_guard, mut router) = capture_until_dropped();
    let mut h = harness("ct-flow-tampered", vec![], Some(0), true).await;
    h.node.attest([5; 32]);
    let served: Attestation = serde_json::from_str(&h.node.answer.lock().unwrap().1).unwrap();
    let mut wrong_key = served.clone();
    wrong_key.tls_public_key_sha256 = format!("0x{}", hex::encode([6; 32]));
    let mut wrong_quote = served;
    wrong_quote.quote = format!("0x{}", hex::encode(b"MOCK-ATTESTATION-NOT-A-TEE:short"));
    for body in [
        serde_json::to_string(&wrong_key).unwrap(),
        serde_json::to_string(&wrong_quote).unwrap(),
        r#"{"quote": 1}"#.to_owned(),
    ] {
        h.node.serve(StatusCode::OK, &body);
        h.attestation_at(T0).await;
        assert_eq!(alerts(&mut router), [fired("attestation_unverified", None)], "{body}");
        assert!(h.watcher.store.tracker.state.attested.is_empty());
    }
    h.node.attest([5; 32]);
    h.attestation_at(T0 + 60_000).await;
    let lines = router.take();
    assert!(lines.iter().any(|line| line.name == "attestation_unverified" && line.severity.is_none()));
    assert!(h.watcher.store.tracker.state.attested.contains_key(&hex::encode([5u8; 32])));
}

#[tokio::test]
async fn a_node_that_does_not_answer_names_no_key_and_raises_no_alert() {
    let (_guard, mut router) = capture_until_dropped();
    let mut h = harness("ct-flow-down", vec![], Some(0), true).await;
    h.node.serve(StatusCode::SERVICE_UNAVAILABLE, "");
    h.attestation_at(T0).await;
    assert_eq!(alerts(&mut router), []);
    assert!(h.watcher.store.tracker.state.attested.is_empty());
    h.node.attest([5; 32]);
    h.attestation_at(T0 + 60_000).await;
    assert!(h.watcher.store.tracker.state.attested.contains_key(&hex::encode([5u8; 32])));
}

#[tokio::test]
async fn an_attestation_longer_than_the_bound_is_not_read() {
    let h = harness("ct-flow-huge", vec![], Some(0), true).await;
    h.node.serve(StatusCode::OK, &"A".repeat(2 * 1024 * 1024));
    let cfg: WatchConfig = toml::from_str(&h.config).unwrap();
    let polled = AttestationSource::new(&cfg.node, true).unwrap().poll().await;
    let Poll::Unreachable(err) = polled else { panic!("{polled:?}") };
    assert!(format!("{err:#}").contains("too long"), "{err:#}");
}
