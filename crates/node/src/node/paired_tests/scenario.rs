//! Whole restart/history scenario through real storage, production startup and public HTTP pages.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use engine_core::types::AccountId;
use serde_json::Value;
use tokio::sync::watch;

use crate::clock::test_clock::TestClock;
use crate::history::{KINDS, account_of};
use crate::log::{LogEntry, Manifest, Offset, SegmentedLog};
use crate::node::paired_tests::harness::{Node, configuration};
use crate::node::paired_tests::http::Http;
use crate::test_support::{TRADER, deposit, logged_history, withdraw};

#[tokio::test]
async fn dev_scenario_authenticated_history_and_paired_restart_preserve_all_public_pages() {
    let cfg = configuration("whole-37-public-history");
    let reference = crate::sequencer::fixture_tests::signed_fixture(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/reference/v2.jsonl"
    )));
    let mut writer = SegmentedLog::open(&cfg.node.log_dir, |_| Ok(()))
        .unwrap()
        .into_writer(4, watch::channel(Manifest::default()).0)
        .unwrap();
    let mut accounts = BTreeSet::from([TRADER]);
    let mut last_ts = 0;
    for entry in &reference {
        writer.append(entry).unwrap();
        if let LogEntry::Command(command) = entry {
            last_ts = command.sequenced.ts;
            accounts.extend(command.events.iter().flatten().filter_map(account_of));
        }
    }
    writer.sync().unwrap();
    drop(writer);
    let clock = Arc::new(TestClock::at(last_ts + 1_000));
    let genesis = crate::sequencer::fixture_tests::finished_genesis(&reference);
    let node = Node::start_resuming(cfg, clock.clone(), genesis.clone()).await.unwrap();
    for n in 0..9 {
        node.submit(deposit(100, &format!("whole37-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    let http = Http::start(reader.clone()).await;
    for account in &accounts {
        let expected = encoded(&logged_history(&cfg.node.log_dir, *account));
        assert_public_pages(&http, *account, &expected).await;
    }
    http.stop().await;
    drop(reader);
    // Restart before any withdrawal approval: the paired cache resumes, and the sweep below must
    // pass against the history recovered from it, not only after the full-replay fallback.
    let node = Node::start_resuming(cfg, clock.clone(), genesis.clone()).await.unwrap();
    assert!(
        node.snapshot_seq.is_some(),
        "before any withdrawal approval the paired cache must resume, so the sweep below covers it"
    );
    node.submit(withdraw(40)).await;
    // The withdrawal is approved under V22; every cache retained so far predates it. Waiting for
    // the history owner after each deposit lands every checkpoint's capture, so five checkpoints
    // age the four retained caches past the approval and the next restart refuses every candidate
    // deterministically, not depending on which captures completed first.
    for n in 0..15 {
        node.submit(deposit(1, &format!("whole37-settle-{n}"))).await;
        node.history_caught_up().await;
    }
    let (cfg, reader) = node.stop().await;
    let http = Http::start(reader.clone()).await;
    let mut kinds = BTreeSet::new();
    for account in &accounts {
        let expected = encoded(&logged_history(&cfg.node.log_dir, *account));
        kinds.extend(expected.iter().map(|event| event["type"].as_str().unwrap().to_owned()));
        assert_public_pages(&http, *account, &expected).await;
    }
    http.stop().await;
    drop(reader);
    let segments = Manifest::load(&cfg.node.log_dir).unwrap().segments.len();
    let node = Node::start_resuming(cfg, clock, genesis).await.unwrap();
    assert!(
        node.snapshot_seq.is_none(),
        "a V22 withdrawal approval refuses snapshot recovery: payout delivery order is not cached, so the full log replays (#213)"
    );
    for n in 0..5 {
        node.submit(deposit(1, &format!("whole37-rotate-{n}"))).await;
    }
    let (cfg, reader) = node.stop().await;
    assert!(Manifest::load(&cfg.node.log_dir).unwrap().segments.len() > segments);
    let http = Http::start(reader).await;
    for account in accounts {
        let expected = encoded(&logged_history(&cfg.node.log_dir, account));
        kinds.extend(expected.iter().map(|event| event["type"].as_str().unwrap().to_owned()));
        assert_public_pages(&http, account, &expected).await;
    }
    for kind in ["deposited", "fill", "financing", "close_out", "withdrawal_approved"] {
        assert!(kinds.contains(kind), "{kind} must be exercised by the whole scenario");
    }
    http.stop().await;
}

/// Compares every real HTTP result against independent full-log event vectors.
async fn assert_public_pages(http: &Http, account: AccountId, expected: &[Value]) {
    let base = format!("/v1/accounts/{account}/history");
    for kind in std::iter::once(None).chain(KINDS.iter().map(|kind| Some(*kind))) {
        let selected: Vec<_> =
            expected.iter().filter(|event| kind.is_none_or(|kind| event["type"] == kind)).cloned().collect();
        for limit in [None, Some(1), Some(200)] {
            let mut actual = Vec::new();
            let mut before: Option<String> = None;
            loop {
                let mut params = Vec::new();
                if let Some(kind) = kind {
                    params.push(format!("kind={kind}"));
                }
                if let Some(limit) = limit {
                    params.push(format!("limit={limit}"));
                }
                if let Some(before) = &before {
                    params.push(format!("before={before}"));
                }
                let (status, body) = http.get(&format!("{base}?{}", params.join("&"))).await.unwrap();
                assert_eq!(status, 200, "{body}");
                let events = body["events"].as_array().unwrap();
                assert!(events.len() <= limit.unwrap_or(50));
                actual.extend(events.iter().cloned());
                let next = body["next"].as_str().map(str::to_owned);
                if next.is_none() {
                    break;
                }
                assert_ne!(next, before, "pagination must advance");
                assert_eq!(next.as_deref(), events.last().unwrap()["cursor"].as_str(), "exact lookahead cursor");
                before = next;
                assert!(actual.len() <= selected.len(), "no repeated or invented events");
            }
            assert_eq!(actual, selected, "{account} kind={kind:?} limit={limit:?}");
        }
    }
    if expected.len() >= 3 {
        let upper: Offset = expected[0]["cursor"].as_str().unwrap().parse().unwrap();
        let lower: Offset = expected[expected.len() - 1]["cursor"].as_str().unwrap().parse().unwrap();
        let (status, body) = http.get(&format!("{base}?before={upper}&after={lower}&limit=200")).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body["events"], Value::Array(expected[1..expected.len() - 1].to_vec()), "both bounds exclusive");
    }
    for suffix in ["limit=0", "limit=201", "kind=unknown", "before=bad"] {
        assert_eq!(http.get(&format!("{base}?{suffix}")).await.unwrap().0, 400);
    }
}

fn encoded(events: &[crate::history::HistoryEvent]) -> Vec<Value> {
    events.iter().map(|event| serde_json::to_value(event).unwrap()).collect()
}
