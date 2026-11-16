//! The venue task's silent and stale symbols on paused time: when a stale run alerts and resolves,
//! when a resubscription is due, and how a failed reconnect of an outage is logged.

use engine_core::types::Millis;

use super::*;
use crate::clock::now_ms;
use crate::feeds::tests::fake::WAIT;
use crate::feeds::venue::confirm::Confirmation;
use crate::feeds::venue::tests::kraken_feed;
use crate::test_support::{capture_alerts, capture_alerts_and_resolutions, capture_field};
use crate::throttle::REALERT_AFTER;

/// Each failed attempt of an outage after its alert says why at info, once a window with a count.
#[tokio::test(start_paused = true)]
async fn failed_attempts_in_an_outage_are_logged_at_info_once_a_window() {
    let (_guard, mut lines) = capture_field("message");
    let mut feed = kraken_feed(&[(1, "BTC/USD")], WAIT);

    let retry = Duration::from_secs(1);
    ["refused", "dns failed", "tls failed"].into_iter().for_each(|error| feed.dropped(&anyhow::anyhow!(error), retry));
    tokio::time::advance(REALERT_AFTER).await;
    feed.dropped(&anyhow::anyhow!("connect timed out"), retry);

    let info: Vec<String> = std::iter::from_fn(|| lines.try_recv().ok())
        .filter(|line| line["level"] == "INFO")
        .map(|line| line["message"].clone())
        .collect();
    assert_eq!(
        info,
        [
            "venue still down (0 more failed attempts since the last): dns failed",
            "venue still down (1 more failed attempts since the last): connect timed out",
        ]
    );
}

/// After a resubscription a stale symbol is judged on what the new subscription sends. The old
/// source's updates in flight go on reading stale; the new snapshot, read seconds after the newest
/// answered ping went out because its transfer held the pongs back, was sent current and ends the
/// run; and reads from a source still behind start a new one, which sets off the next attempt once
/// it spans the grace.
#[tokio::test(start_paused = true)]
async fn a_resubscribed_stale_symbol_is_judged_on_what_the_new_subscription_sends() {
    let (_guard, _alerts) = capture_alerts();
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], Duration::from_secs(10));
    let read = |feed: &mut VenueFeed, age_ms: Millis| feed.note_age("ETH/USD", now_ms() - age_ms, now_ms());
    let answered = |feed: &mut VenueFeed| {
        let ping = feed.delay.sent(Instant::now(), now_ms());
        feed.delay.answered(&ping);
    };
    answered(&mut feed);
    read(&mut feed, 600_000);
    tokio::time::advance(STALE_GRACE).await;
    answered(&mut feed);
    read(&mut feed, 600_000);
    assert_eq!(feed.check_quoting(), ["ETH/USD"]);

    // An update in flight from the old source, then the new snapshot 2.1 s on, 2 s old.
    tokio::time::advance(Duration::from_millis(20)).await;
    read(&mut feed, 600_000);
    tokio::time::advance(Duration::from_millis(2_100)).await;
    read(&mut feed, 2_000);
    assert!(feed.check_quoting().is_empty());
    // Ten minutes old again, with pings answered at once: once that spans the grace, ETH again.
    answered(&mut feed);
    read(&mut feed, 600_000);
    tokio::time::advance(STALE_GRACE).await;
    answered(&mut feed);
    read(&mut feed, 600_000);
    assert_eq!(feed.check_quoting(), ["ETH/USD"]);
}

/// A silent symbol is tried again only after a whole window of its own count, so one whose count an
/// outage's end restarted waits it out.
#[tokio::test(start_paused = true)]
async fn a_silent_symbol_is_retried_only_after_a_window_of_its_own_count() {
    let (_guard, _alerts) = capture_alerts();
    let window = Duration::from_millis(300);
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], window);
    tokio::time::advance(window).await;
    feed.delivered("BTC/USD");
    assert_eq!(feed.check_quoting(), ["ETH/USD"]);

    // An outage, whose end restarts every count, ETH's included, while ETH stays silent.
    feed.down = true;
    assert!(feed.check_quoting().is_empty());
    tokio::time::advance(window * 2).await;
    feed.delivered("BTC/USD");
    tokio::time::advance(window - Duration::from_millis(1)).await;
    feed.delivered("BTC/USD");
    assert!(feed.check_quoting().is_empty());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(feed.check_quoting(), ["ETH/USD"]);
}

/// A market whose `max_age_ms` is shorter than `stale_after` holds its symbol's reads to it: a book
/// sent 600 ms old reads stale there, and one at its 400 ms ends the run.
#[tokio::test(start_paused = true)]
async fn a_market_age_limit_under_stale_after_bounds_a_stale_read() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], Duration::from_secs(10));
    assert_eq!(feed.timing.stale_after, Duration::from_secs(1));
    feed.max_age.insert(2, 400);
    let read = |feed: &mut VenueFeed, age_ms: Millis| feed.note_age("ETH/USD", now_ms() - age_ms, now_ms());
    read(&mut feed, 600);
    tokio::time::advance(STALE_GRACE).await;
    read(&mut feed, 600);
    assert_eq!(feed.check_quoting(), ["ETH/USD"]);
    let alert = alerts.try_recv().expect("an alert");
    assert_eq!((alert["alert"].as_str(), alert["symbol"].as_str()), ("venue_stale", "ETH/USD"));
    read(&mut feed, 400);
    let resolution = resolved.try_recv().expect("a resolution");
    assert_eq!((resolution["alert_resolved"].as_str(), resolution["symbol"].as_str()), ("venue_stale", "ETH/USD"));
}

/// A symbol alerted stale that the engine stops taking quotes for is no longer expected to quote,
/// so its alert resolves under the same key.
#[tokio::test(start_paused = true)]
async fn a_stale_symbol_held_back_resolves_its_alert() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], Duration::from_secs(10));
    let read = |feed: &mut VenueFeed| feed.note_age("ETH/USD", now_ms() - 600_000, now_ms());
    read(&mut feed);
    tokio::time::advance(STALE_GRACE).await;
    read(&mut feed);
    assert_eq!(feed.check_quoting(), ["ETH/USD"]);
    let alert = alerts.try_recv().expect("an alert");

    feed.confirmed(Confirmation { idle: vec!["ETH/USD".into()], ..Confirmation::default() });
    assert!(feed.check_quoting().is_empty());
    let resolution = resolved.try_recv().expect("a resolution");
    assert_eq!(resolution["alert_resolved"], "venue_stale");
    for field in ["venue", "feed", "symbol", "market"] {
        assert_eq!(resolution[field], alert[field], "{field}");
    }
    assert!(resolved.try_recv().is_err(), "resolved twice");
}
