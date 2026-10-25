//! The mock feed: each mock market quotes until its `stop_after`, and only while the engine's hours
//! have it open.

use std::time::Duration;

use chrono::DateTime;
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::market::MarketSpec;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::clock::now_ms;
use crate::config::{FeedProvider, FeedsSection, MockMid};
use crate::feeds::run_mock;
use crate::feeds::tests::fake::FakeEngine;
use crate::feeds::tests::rwa_support::listing;
use crate::sequencer::Request;

#[tokio::test(start_paused = true)]
async fn a_mock_market_stops_quoting_at_its_stop_after_and_the_others_carry_on() {
    let mock_mid = |market, stop_after| MockMid { market, mid: Fixed::from_int(100), stop_after };
    let cfg = FeedsSection {
        provider: FeedProvider::Mock,
        interval: Duration::from_millis(100),
        mock_mids: vec![mock_mid(1, Some(Duration::from_millis(250))), mock_mid(2, None)],
        venues: None,
    };
    let (tx, mut rx) = mpsc::channel(64);
    let started = Instant::now();
    tokio::spawn(async move { run_mock(&cfg, tx).await });
    let engine = FakeEngine::new();
    let mut quoted = Vec::new();
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
