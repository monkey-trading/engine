//! The mock feed: each mock market quotes until its `stop_after`, only while the engine's hours
//! have it open, and in its current share units.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use chrono::DateTime;
use engine_core::command::{AdminCommand, Command};
use engine_core::market::MarketSpec;
use engine_core::{Engine, Fixed};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::clock::now_ms;
use crate::config::{FeedProvider, FeedsSection, MockMid};
use crate::feeds::tests::fake::FakeEngine;
use crate::feeds::tests::rwa_support::listing;
use crate::feeds::{Walks, run_mock, split_view};
use crate::log::{LogEntry, read_file};
use crate::sequencer::Request;

#[tokio::test(start_paused = true)]
async fn a_mock_market_stops_quoting_at_its_stop_after_and_the_others_carry_on() {
    let cfg = feeds(&[(1, "100", Some(Duration::from_millis(250))), (2, "100", None)]);
    let (tx, mut rx) = mpsc::channel(64);
    let started = Instant::now();
    tokio::spawn(async move { run_mock(&cfg, tx).await });
    let engine = FakeEngine::new();
    let mut quoted = Vec::new();
    // The feed asks for the market view before each round of quotes; keep reading through the 1s round.
    while started.elapsed() <= Duration::from_secs(1) {
        match rx.recv().await {
            Some(Request::Query(query)) => engine.answer(query),
            Some(Request::Submit { cmd: Command::Prices { market, .. }, .. }) => {
                quoted.push((started.elapsed().as_millis(), market));
            }
            _ => panic!("expected prices or a query"),
        }
    }
    let last = |id| quoted.iter().filter(|(_, market)| *market == id).map(|(at, _)| *at).max();
    assert_eq!(last(1), Some(200));
    assert_eq!(last(2), Some(1_000));
}

#[tokio::test(start_paused = true)]
async fn a_mock_market_the_engine_has_closed_is_not_quoted() {
    let mock_mid = |market| MockMid { market, mid: Fixed::from_int(100), stop_after: None };
    let cfg = FeedsSection {
        provider: FeedProvider::Mock,
        interval: Duration::from_millis(100),
        mock_mids: vec![mock_mid(1), mock_mid(40)],
        venues: None,
    };
    // Apple is relisted shut from yesterday to tomorrow, wherever the wall clock is.
    let mut engine = FakeEngine::new();
    let today = DateTime::from_timestamp_millis(now_ms()).expect("in range").date_naive();
    let closed_dates: Vec<String> =
        [today.pred_opt(), Some(today), today.succ_opt()].into_iter().flatten().map(|day| day.to_string()).collect();
    let windows = serde_json::json!([{ "open": "Mon 00:00", "close": "Thu 00:00" }, { "open": "Thu 00:00", "close": "Mon 00:00" }]);
    let schedule =
        serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": windows, "closed_dates": closed_dates });
    let closed = MarketSpec { schedule: serde_json::from_value(schedule).expect("a schedule"), ..listing(40) };
    engine.apply(Command::Admin(AdminCommand::UpsertMarket(closed)));
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { run_mock(&cfg, tx).await });
    let mut quoted = 0;
    while quoted < 10 {
        match rx.recv().await {
            Some(Request::Query(query)) => engine.answer(query),
            Some(Request::Submit { cmd: Command::Prices { market, .. }, .. }) => {
                assert_eq!(market, 1);
                quoted += 1;
            }
            _ => panic!("expected prices or a query"),
        }
    }
}

#[test]
fn a_walk_rescales_once_for_a_quote_stamped_at_or_after_its_split_opening() {
    let mut walks = Walks::start(&feeds(&[(40, "100", None), (41, "100", None)]), &BTreeMap::new());
    let pending = || BTreeMap::from([("four".to_owned(), (40, 1_000, 4, 1))]);
    let mids = |walks: &Walks| walks.mids.iter().map(|(_, mid, _)| mid.to_string()).collect::<Vec<_>>();
    walks.follow(pending(), 999);
    assert_eq!(mids(&walks), ["100", "100"], "a quote before the opening stays in the old units");
    walks.follow(pending(), 1_000);
    assert_eq!(mids(&walks), ["25", "100"], "a quote stamped at the opening is in the new units");
    // Still pending until a command at the opening applies it, so the mock sees it again.
    walks.follow(pending(), 1_001);
    walks.follow(BTreeMap::new(), 2_000);
    assert_eq!(mids(&walks), ["25", "100"], "only the split market rescales, and only once");
}

#[test]
fn a_restarted_walk_starts_from_the_mark_of_a_market_that_has_split() {
    let marks = BTreeMap::from([(40, Fixed::from_int(25))]);
    let walks = Walks::start(&feeds(&[(40, "100", None), (41, "100", None)]), &marks);
    let mids: Vec<_> = walks.mids.iter().map(|(market, mid, _)| (*market, mid.to_string())).collect();
    assert_eq!(mids, [(40, "25".to_owned()), (41, "100".to_owned())]);
}

#[test]
fn the_split_view_reads_pending_splits_and_the_marks_of_split_markets() {
    // Applying a reference log's commands gives an engine with marked markets; one of them has split.
    let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference/v18.jsonl");
    let mut replayed = Engine::new();
    read_file(&reference, |entry| {
        if let LogEntry::Command(command) = entry {
            replayed.apply(&command.sequenced)?;
        }
        Ok(())
    })
    .unwrap();
    let marked: Vec<(u32, Fixed)> =
        replayed.markets().iter().filter_map(|(id, market)| market.mark.map(|mark| (*id, mark))).take(2).collect();
    let [(split_market, split_mark), _] = marked[..] else { panic!("two marked markets: {marked:?}") };
    let mut state = serde_json::to_value(&replayed).unwrap();
    state["markets"][split_market.to_string()]["last_split_at"] = serde_json::json!(1_000);
    let action = |id: &str, adjustment: serde_json::Value| {
        serde_json::json!({
            "instruction": { "id": id, "market": 40, "previous_close": 500, "ex_open": 1000, "adjustment": adjustment },
            "quantities": null,
            "failure": null,
        })
    };
    state["corporate_actions"] = serde_json::json!({
        "split": action("split", serde_json::json!({ "kind": "split", "new_units": 4, "old_units": 1 })),
        "cash": action("cash", serde_json::json!({ "kind": "dividend", "gross_per_unit": "1", "withholding_rate": "0" })),
    });
    let engine: Engine = serde_json::from_value(state).unwrap();
    let view = split_view(&engine);
    assert_eq!(view.pending, BTreeMap::from([("split".to_owned(), (40, 1_000, 4, 1))]), "dividends are not followed");
    assert_eq!(view.marks, BTreeMap::from([(split_market, split_mark)]), "an unsplit market keeps its walk");
}

// ── Helpers ──

/// A mock feed section quoting each `(market, mid, stop_after)`.
fn feeds(mids: &[(u32, &str, Option<Duration>)]) -> FeedsSection {
    FeedsSection {
        provider: FeedProvider::Mock,
        interval: Duration::from_millis(100),
        mock_mids: mids
            .iter()
            .map(|(market, mid, stop_after)| MockMid {
                market: *market,
                mid: mid.parse().unwrap(),
                stop_after: *stop_after,
            })
            .collect(),
        venues: None,
    }
}
