use std::sync::Arc;
use std::time::Duration;

use crate::alerts::{AlertLine, Severity, capture, capture_refused};
use crate::ct_watch::leaf::Certificate;
use crate::ct_watch::state::{LogProgress, MAX_ATTESTED, MAX_FINDINGS, State};
use crate::ct_watch::tracker::Tracker;
use crate::metrics::Metrics;

const GRACE_MS: u64 = 600_000;
const STALE_MS: u64 = 3_600_000;
const T0: u64 = 1_000_000_000;

fn tracker() -> Tracker {
    Tracker::new(
        State::default(),
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [("log".to_owned(), "key-log".to_owned())],
        T0,
    )
}

fn certificate(name: &str, key: u8, logged_ms: u64) -> Certificate {
    Certificate {
        names: vec![name.to_owned()],
        key_sha256: [key; 32],
        issuer: "CN=Issuer".into(),
        not_after_ms: T0 + 90 * 86_400_000,
        logged_ms,
    }
}

/// The alert lines `f` logs as `(name, severity)`; a resolve has no severity.
fn lines(f: impl FnOnce()) -> Vec<(String, Option<Severity>)> {
    capture(f).into_iter().map(|line| (line.name, line.severity)).collect()
}

fn fired(name: &str) -> (String, Option<Severity>) {
    (name.to_owned(), Some(Severity::Critical))
}

fn resolved(name: &str) -> (String, Option<Severity>) {
    (name.to_owned(), None)
}

#[test]
fn a_certificate_for_an_unknown_key_waits_out_its_grace_then_alerts_once() {
    let mut tracker = tracker();
    let mut raised: Vec<AlertLine> = Vec::new();
    raised.extend(capture(|| tracker.ingest("log", 7, &certificate("api.example.com", 1, T0), T0 + 1_000)));
    raised.extend(capture(|| tracker.settle(T0 + GRACE_MS - 1)));
    assert!(raised.is_empty(), "alerted inside the grace: {raised:?}");
    let at_grace = capture(|| tracker.settle(T0 + GRACE_MS));
    let [line] = at_grace.as_slice() else { panic!("{at_grace:?}") };
    assert_eq!((line.name.as_str(), line.severity), ("unattested_certificate", Some(Severity::Critical)));
    let field = |name: &str| line.fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
    assert_eq!(field("log"), Some("log"));
    assert_eq!(field("index"), Some("7"));
    assert_eq!(field("issuer"), Some("CN=Issuer"));
    assert_eq!(field("key_sha256"), Some(hex::encode([1u8; 32]).as_str()));
    // An open alert is asserted on every check; the router collapses the repeats into one episode.
    assert_eq!(lines(|| tracker.settle(T0 + GRACE_MS + 1_000)), [fired("unattested_certificate")]);
}

#[test]
fn a_key_attested_within_the_grace_raises_nothing() {
    let mut tracker = tracker();
    let raised = lines(|| {
        tracker.ingest("log", 0, &certificate("api.example.com", 1, T0), T0 + 1_000);
        tracker.attested(&[1; 32], T0 + 60_000);
        tracker.settle(T0 + GRACE_MS + 1);
    });
    assert!(raised.is_empty(), "{raised:?}");
    assert!(tracker.state.findings.is_empty());
}

#[test]
fn a_certificate_for_an_attested_key_is_not_held() {
    let mut tracker = tracker();
    tracker.attested(&[1; 32], T0);
    tracker.ingest("log", 0, &certificate("api.example.com", 1, T0), T0);
    assert!(tracker.state.findings.is_empty());
}

#[test]
fn certificates_for_other_names_are_ignored() {
    let mut tracker = tracker();
    let raised = lines(|| {
        tracker.ingest("log", 0, &certificate("other.example.com", 1, T0), T0 + 2 * GRACE_MS);
        tracker.settle(T0 + 2 * GRACE_MS);
    });
    assert!(raised.is_empty() && tracker.state.findings.is_empty());
}

#[test]
fn a_wildcard_for_the_domains_parent_is_held() {
    let mut tracker = tracker();
    tracker.ingest("log", 0, &certificate("*.example.com", 1, T0), T0);
    assert_eq!(tracker.state.findings.len(), 1);
}

#[test]
fn an_old_entry_read_late_alerts_when_read() {
    let mut tracker = tracker();
    let raised = lines(|| tracker.ingest("log", 3, &certificate("api.example.com", 1, T0), T0 + 5 * GRACE_MS));
    assert_eq!(raised, [fired("unattested_certificate")]);
    assert_eq!(tracker.state.findings[0].alerted_at_ms, Some(T0 + 5 * GRACE_MS));
}

#[test]
fn a_log_time_in_the_future_does_not_stretch_the_grace() {
    let mut tracker = tracker();
    tracker.ingest("log", 0, &certificate("api.example.com", 1, T0 + 365 * 86_400_000), T0);
    assert_eq!(tracker.state.findings[0].decide_at_ms, T0 + GRACE_MS);
}

#[test]
fn an_alert_is_asserted_on_every_check_and_resolved_when_a_verified_attestation_names_the_key() {
    let mut tracker = tracker();
    tracker.ingest("log", 0, &certificate("api.example.com", 1, T0), T0);
    let at = T0 + GRACE_MS;
    for check in 0..3 {
        assert_eq!(lines(|| tracker.settle(at + check * 60_000)), [fired("unattested_certificate")], "check {check}");
    }
    let resolve = lines(|| {
        tracker.attested(&[1; 32], at + 200_000);
        tracker.settle(at + 200_001);
    });
    assert_eq!(resolve, [resolved("unattested_certificate")]);
    // The finding stays one more pass, so a kill before the resolve reached the outbox is repaired.
    assert_eq!(tracker.state.findings.len(), 1);
    assert!(lines(|| tracker.settle(at + 260_000)).is_empty());
    assert!(tracker.state.findings.is_empty());
}

#[test]
fn a_resolve_not_yet_committed_when_the_watcher_was_killed_is_sent_again_on_resume() {
    let mut first = tracker();
    first.ingest("log", 0, &certificate("api.example.com", 1, T0), T0);
    let at = T0 + GRACE_MS;
    assert_eq!(lines(|| first.settle(at)), [fired("unattested_certificate")]);
    first.attested(&[1; 32], at + 1);
    assert_eq!(lines(|| first.settle(at + 2)), [resolved("unattested_certificate")]);
    assert!(first.state.findings[0].resolving, "saved with the finding");

    // Killed here: the resolve line may not have reached the outbox, and the finding is still on disk.
    let mut resumed = Tracker::new(
        std::mem::take(&mut first.state),
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [("log".to_owned(), "key-log".to_owned())],
        at + 5_000,
    );
    assert_eq!(lines(|| resumed.settle(at + 5_001)), [resolved("unattested_certificate")]);
    assert!(lines(|| resumed.settle(at + 65_000)).is_empty());
    assert!(resumed.state.findings.is_empty());
}

#[test]
fn a_batch_that_would_pass_the_bound_waits_instead_of_dropping_findings() {
    let mut tracker = tracker();
    let unknown = certificate("api.example.com", 1, T0);
    let ignored = [certificate("other.example.com", 2, T0), {
        let mut expired = certificate("api.example.com", 3, T0);
        expired.not_after_ms = T0;
        expired
    }];
    for index in 0..MAX_FINDINGS as u64 - 1 {
        tracker.ingest("log", index, &unknown, T0);
    }
    // One more fits; names that need no finding never count against the bound.
    assert!(!tracker.lacks_room_for(&[unknown.clone(), ignored[0].clone(), ignored[1].clone()], T0));
    assert!(tracker.lacks_room_for(&[unknown.clone(), unknown.clone()], T0));
    tracker.attested(&[1; 32], T0);
    assert!(!tracker.lacks_room_for(&[unknown.clone(), unknown], T0), "an attested key needs no finding");
}

#[test]
fn a_log_no_longer_configured_keeps_its_position_is_not_checked_and_its_outage_is_resolved() {
    let mut state = State::default();
    state
        .logs
        .insert("key-retired".into(), LogProgress { name: "retired".into(), next_index: Some(5), caught_up_ms: 1 });
    let mut tracker = Tracker::new(
        state,
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [("log".to_owned(), "key-log".to_owned())],
        T0,
    );
    let resolves = capture(|| tracker.settle(T0));
    let [resolve] = resolves.as_slice() else { panic!("{resolves:?}") };
    assert_eq!((resolve.name.as_str(), resolve.severity), ("ct_log_unreachable", None));
    assert!(resolve.fields.contains(&("log".to_owned(), "retired".to_owned())), "{resolve:?}");
    assert_eq!(tracker.state.logs["key-retired"].next_index, Some(5), "its position waits for it to return");
    assert_eq!(
        lines(|| tracker.check_logs(T0 + 100 * STALE_MS)),
        [fired("ct_log_unreachable")],
        "only `log` is checked"
    );
}

#[test]
fn a_log_counts_as_read_as_of_its_signed_head_so_a_replayed_head_cannot_keep_it_healthy() {
    let mut tracker = tracker();
    // Read up to a head signed at T0 + 1000, repeatedly, until that head is stale.
    tracker.advance("log", 3, Some(T0 + 1_000), T0 + 2_000);
    for polled_at in [T0 + STALE_MS / 2, T0 + STALE_MS] {
        tracker.advance("log", 3, Some(T0 + 1_000), polled_at);
        assert!(lines(|| tracker.check_logs(polled_at)).is_empty());
    }
    tracker.advance("log", 3, Some(T0 + 1_000), T0 + STALE_MS + 1_001);
    assert_eq!(lines(|| tracker.check_logs(T0 + STALE_MS + 1_001)), [fired("ct_log_unreachable")]);
    // A head stamped ahead of this clock counts as read now, no later.
    tracker.advance("log", 4, Some(T0 + 10 * STALE_MS), T0 + STALE_MS + 2_000);
    assert_eq!(tracker.state.logs["key-log"].caught_up_ms, T0 + STALE_MS + 2_000);
}

#[test]
fn the_oldest_attested_keys_are_evicted_past_the_bound() {
    let mut tracker = tracker();
    for n in 0..=MAX_ATTESTED as u64 {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&n.to_be_bytes());
        tracker.attested(&key, T0 + n);
    }
    assert_eq!(tracker.state.attested.len(), MAX_ATTESTED);
    assert!(!tracker.state.attested.contains_key(&hex::encode([0u8; 32])), "the first key stays");
}

#[test]
fn a_log_not_read_to_a_recent_tree_head_is_asserted_until_it_is() {
    let mut tracker = tracker();
    assert!(lines(|| tracker.check_logs(T0 + STALE_MS - 1)).is_empty());
    for check in 0..3 {
        assert_eq!(lines(|| tracker.check_logs(T0 + STALE_MS + check * 60_000)), [fired("ct_log_unreachable")]);
    }
    let caught_up = lines(|| tracker.advance("log", 10, Some(T0 + STALE_MS + 200_000), T0 + STALE_MS + 200_001));
    assert_eq!(caught_up, [resolved("ct_log_unreachable")]);
    assert!(lines(|| tracker.check_logs(T0 + STALE_MS + 260_000)).is_empty());
}

#[test]
fn progress_that_has_not_reached_the_tree_head_does_not_count_as_a_read() {
    let mut tracker = tracker();
    tracker.advance("log", 10, None, T0 + STALE_MS);
    assert_eq!(lines(|| tracker.check_logs(T0 + STALE_MS)), [fired("ct_log_unreachable")]);
}

#[test]
fn a_new_log_starts_at_the_configured_index_clamped_to_the_tree_or_at_its_end() {
    let mut tracker = tracker();
    assert_eq!(tracker.next_index("log", 500, None), 500);
    assert_eq!(tracker.next_index("log", 900, Some(3)), 500, "a position once fixed is kept");
    let mut tracker = Tracker::new(
        State::default(),
        "d.example",
        Duration::from_secs(1),
        Duration::from_secs(1),
        [("a".to_owned(), "key-a".to_owned()), ("b".to_owned(), "key-b".to_owned())],
        T0,
    );
    assert_eq!(tracker.next_index("a", 500, Some(3)), 3);
    assert_eq!(tracker.next_index("b", 500, Some(9_000)), 500);
}

#[test]
fn only_a_changed_state_asks_to_be_saved() {
    let mut tracker = tracker();
    assert!(tracker.take_changed(), "new log entries are state");
    assert!(!tracker.take_changed());
    tracker.settle(T0);
    tracker.check_logs(T0);
    assert!(!tracker.take_changed());
    tracker.attested(&[1; 32], T0);
    assert!(tracker.take_changed());
    tracker.attested(&[1; 32], T0 + 1);
    assert!(tracker.take_changed(), "the time of the last verification is state, so a restart keeps the outage clock");
}

#[test]
fn an_expired_certificate_is_ignored_and_an_open_alert_ends_when_its_certificate_expires() {
    let mut tracker = tracker();
    let mut expired = certificate("api.example.com", 1, T0);
    expired.not_after_ms = T0 + 1_000;
    assert!(lines(|| tracker.ingest("log", 0, &expired, T0 + 2_000)).is_empty());
    assert!(tracker.state.findings.is_empty());

    let mut short = certificate("api.example.com", 2, T0);
    short.not_after_ms = T0 + 2 * GRACE_MS;
    tracker.ingest("log", 1, &short, T0);
    assert_eq!(lines(|| tracker.settle(T0 + GRACE_MS)), [fired("unattested_certificate")]);
    assert_eq!(lines(|| tracker.settle(T0 + 2 * GRACE_MS)), [resolved("unattested_certificate")]);
    tracker.settle(T0 + 2 * GRACE_MS + 60_000);
    assert!(tracker.state.findings.is_empty());
}

#[test]
fn a_logs_position_follows_its_key_through_a_rename_and_a_new_shard_starts_afresh() {
    let mut state = State::default();
    state.logs.insert("key-log".into(), LogProgress { name: "log".into(), next_index: Some(500), caught_up_ms: 1 });
    let resume = |name: &str, key: &str| {
        let mut tracker = Tracker::new(
            State { logs: state.logs.clone(), ..State::default() },
            "api.example.com",
            Duration::from_millis(GRACE_MS),
            Duration::from_millis(STALE_MS),
            [(name.to_owned(), key.to_owned())],
            T0,
        );
        let resolves = capture(|| tracker.settle(T0));
        (tracker, resolves)
    };
    let (mut renamed, resolves) = resume("renamed", "key-log");
    assert_eq!(renamed.next_index("renamed", 900, None), 500, "the same key keeps its position");
    assert_eq!(renamed.state.logs["key-log"].name, "renamed");
    let [resolve] = resolves.as_slice() else { panic!("{resolves:?}") };
    assert!(
        resolve.fields.contains(&("log".to_owned(), "log".to_owned())),
        "the outage under the old name is resolved: {resolve:?}"
    );
    let (mut new_shard, _) = resume("log", "key-new-shard");
    assert_eq!(new_shard.next_index("log", 7, None), 7, "another key starts at its own tree's end");
}

#[test]
fn an_alert_open_before_a_restart_is_asserted_again_by_the_resumed_watcher() {
    let mut first = tracker();
    first.ingest("log", 0, &certificate("api.example.com", 1, T0), T0);
    assert_eq!(lines(|| first.settle(T0 + GRACE_MS)), [fired("unattested_certificate")]);
    assert!(first.state.findings[0].alerted_at_ms.is_some());

    let mut resumed = Tracker::new(
        std::mem::take(&mut first.state),
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [("log".to_owned(), "key-log".to_owned())],
        T0 + GRACE_MS + 1_000,
    );
    assert_eq!(lines(|| resumed.settle(T0 + GRACE_MS + 1_000)), [fired("unattested_certificate")]);
}

#[test]
fn going_a_grace_without_a_verified_attestation_is_asserted_until_one_verifies() {
    let mut tracker = tracker();
    assert!(lines(|| tracker.check_attestation(T0 + GRACE_MS - 1)).is_empty());
    for check in 0..3 {
        assert_eq!(
            lines(|| tracker.check_attestation(T0 + GRACE_MS + check * 60_000)),
            [fired("attestation_unverified")]
        );
    }
    tracker.attested(&[1; 32], T0 + GRACE_MS + 200_000);
    assert!(lines(|| tracker.check_attestation(T0 + GRACE_MS + 250_000)).is_empty());
}

#[test]
fn the_last_verification_survives_a_restart_so_an_outage_is_not_given_a_fresh_grace() {
    let mut first = tracker();
    first.attested(&[1; 32], T0 + 1_000);
    let mut resumed = Tracker::new(
        std::mem::take(&mut first.state),
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [("log".to_owned(), "key-log".to_owned())],
        T0 + GRACE_MS,
    );
    assert!(lines(|| resumed.check_attestation(T0 + GRACE_MS)).is_empty());
    assert_eq!(lines(|| resumed.check_attestation(T0 + 1_000 + GRACE_MS)), [fired("attestation_unverified")]);
}

#[test]
fn the_longest_alert_episode_key_fits_the_outbox_bound() {
    let log = "l".repeat(64);
    let mut tracker = Tracker::new(
        State::default(),
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [(log.clone(), "key-log".to_owned())],
        T0,
    );
    let mut cert = certificate("api.example.com", 0xff, T0);
    cert.issuer = "I".repeat(48);
    let raised = capture(|| tracker.ingest(&log, u64::MAX, &cert, T0 + 10 * GRACE_MS));
    let [line] = raised.as_slice() else { panic!("{raised:?}") };
    assert!(line.key().len() <= 256, "{} bytes: {}", line.key().len(), line.key());
}

/// Runs `settle` at `now` with the alert queue full, as a router backlog leaves it.
fn settle_refused(tracker: &mut Tracker, metrics: &Arc<Metrics>, now: u64) {
    capture_refused(metrics, || tracker.settle(now));
}

#[test]
fn a_resolve_the_alert_queue_refused_is_sent_again_before_the_finding_is_forgotten() {
    let metrics = Arc::new(Metrics::new());
    let mut tracker = tracker();
    tracker.count_drops(metrics.alerts_dropped.clone());
    tracker.ingest("log", 0, &certificate("api.example.com", 1, T0), T0);
    let at = T0 + GRACE_MS;
    assert_eq!(lines(|| tracker.settle(at)), [fired("unattested_certificate")]);
    tracker.attested(&[1; 32], at + 1);
    // The queue is full: the resolve is lost, and the finding must stay to resolve it later.
    settle_refused(&mut tracker, &metrics, at + 2);
    assert_eq!(metrics.alerts_dropped.get(), 1);
    assert!(!tracker.state.findings[0].resolving, "the resolve was not accepted");
    settle_refused(&mut tracker, &metrics, at + 3);
    assert_eq!(tracker.state.findings.len(), 1, "still waiting for a resolve the queue takes");
    // The queue has room again.
    assert_eq!(lines(|| tracker.settle(at + 4)), [resolved("unattested_certificate")]);
    assert!(tracker.state.findings[0].resolving);
    assert!(lines(|| tracker.settle(at + 5)).is_empty());
    assert!(tracker.state.findings.is_empty());
}

#[test]
fn a_renamed_logs_old_label_is_resolved_until_the_alert_queue_takes_it() {
    let mut state = State::default();
    state.logs.insert("key-log".into(), LogProgress { name: "old".into(), next_index: Some(5), caught_up_ms: 1 });
    let metrics = Arc::new(Metrics::new());
    let mut tracker = Tracker::new(
        state,
        "api.example.com",
        Duration::from_millis(GRACE_MS),
        Duration::from_millis(STALE_MS),
        [("new".to_owned(), "key-log".to_owned())],
        T0,
    );
    tracker.count_drops(metrics.alerts_dropped.clone());
    settle_refused(&mut tracker, &metrics, T0);
    assert_eq!(tracker.state.logs["key-log"].name, "old", "the old label is kept while its resolve is owed");
    let resolves = capture(|| tracker.settle(T0 + 1));
    let [resolve] = resolves.as_slice() else { panic!("{resolves:?}") };
    assert!(resolve.fields.contains(&("log".to_owned(), "old".to_owned())), "{resolve:?}");
    assert_eq!(tracker.state.logs["key-log"].name, "new");
    assert!(capture(|| tracker.settle(T0 + 2)).is_empty());
}
