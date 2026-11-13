use super::*;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::{Venue, VenueConnection, VenueSymbol};
use crate::feeds::tests::cme_support::{CmeStandIn, StandIn};
use crate::feeds::tests::fake::fast;
use crate::feeds::tests::rwa_support::TlsFront;
use crate::feeds::venue::{Credentials, OAuthClient, Timing};

#[tokio::test]
async fn a_replacement_slower_than_the_delay_bound_hands_over_a_ping_answered_since_it_was_ready() {
    let tls = TlsFront::new();
    let stand_in = quoting_stand_in(&tls, |state| {
        (state.login_delay, state.delay_every_login) = (Some(Duration::from_secs(3)), true)
    })
    .await;
    let timing = Timing { ping_interval: PING_INTERVAL, max_delay: MAX_DELAY, ..fast(Duration::from_secs(5)) };
    let (feed, token) = cme_feed(&stand_in, &tls, timing).await;

    let replacement = feed.replace(&token).await.expect("the replacement opens");
    // Its first ping was answered three seconds after it went out, past the two-second bound; what it
    // hands over is bounded by a ping answered since it was subscribed, within a restatement's 500 ms.
    let behind = replacement.delay.behind(Instant::now()).expect("a ping answered");
    assert!(behind < Duration::from_secs(1), "{behind:?} behind");
    stand_in.stop();
}

#[tokio::test]
async fn a_replacement_not_ready_before_its_token_expires_is_dropped() {
    let tls = TlsFront::new();
    // Two-second tokens, every login answered three seconds late, within the five-second timeout,
    // and sessions kept open past their tokens' expiry.
    let stand_in = quoting_stand_in_granting(&tls, 2, |state| {
        (state.login_delay, state.delay_every_login, state.keep_expired) = (Some(Duration::from_secs(3)), true, true)
    })
    .await;
    let timing = Timing { ping_interval: PING_INTERVAL, max_delay: MAX_DELAY, ..fast(Duration::from_secs(5)) };
    let client = stand_in.client(&tls, timing.timeout);
    let token = client.fetch(Duration::from_secs(1)).await.expect("a token");
    let feed = feed_on(&stand_in, &tls, timing, client);

    let started = Instant::now();
    assert!(feed.replace(&token).await.is_err(), "a replacement outlived its token");
    assert!(started.elapsed() < Duration::from_millis(2_500), "dropped only {:?} later", started.elapsed());
    stand_in.stop();
}

#[tokio::test]
async fn a_binary_message_a_replacement_reads_is_kept_to_be_refused_at_the_switch() {
    let tls = TlsFront::new();
    let stand_in = quoting_stand_in(&tls, |state| state.binary_before_subscribing = true).await;
    let timing = Timing { ping_interval: PING_INTERVAL, max_delay: MAX_DELAY, ..fast(Duration::from_secs(5)) };
    let (feed, token) = cme_feed(&stand_in, &tls, timing).await;

    let replacement = feed.replace(&token).await.expect("the replacement opens");
    assert_eq!(replacement.refused, [("binary message".to_owned(), String::new())]);
    stand_in.stop();
}

#[tokio::test]
async fn a_replacement_whose_ready_ping_is_answered_past_the_delay_bound_is_pinged_again_before_a_book() {
    let replacement = ready_past_a_late_answer(None).await;
    assert!(!replacement.books.is_empty(), "no book handed over");
}

#[tokio::test]
async fn a_replacement_whose_ready_ping_is_answered_past_the_delay_bound_is_pinged_again_through_a_closure() {
    // Only a heartbeat, as through a closure: ready a ping interval after an answer, with no book.
    let replacement = ready_past_a_late_answer(Some(Duration::from_secs(10))).await;
    assert!(replacement.books.is_empty(), "a book handed over");
}

// ── Helpers ──

const PING_INTERVAL: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(2);

/// A CME feed on `stand_in`'s stream quoting ES:202612, and a token for its replacement.
async fn cme_feed(stand_in: &CmeStandIn, tls: &TlsFront, timing: Timing) -> (VenueFeed, Token) {
    let client = stand_in.client(tls, timing.timeout);
    let token = client.fetch(timing.timeout).await.expect("a token");
    (feed_on(stand_in, tls, timing, client), token)
}

/// A CME feed on `stand_in`'s stream quoting ES:202612, logging in with `client`.
fn feed_on(stand_in: &CmeStandIn, tls: &TlsFront, timing: Timing, client: OAuthClient) -> VenueFeed {
    let symbols = vec![VenueSymbol { market: 1, symbol: "ES:202612".into(), contract: None }];
    let conn = VenueConnection {
        venue: Venue::Cme,
        url: stand_in.stream_url.clone(),
        api_key_env: None,
        api_key_secret: None,
        oauth: None,
        symbols,
    };
    VenueFeed::new(conn, Credentials::OAuth(client), timing, tls.client.clone(), mpsc::channel(1).0)
}

/// A stand-in quoting ES:202612, with `set` applied to what it does.
async fn quoting_stand_in(tls: &TlsFront, set: impl FnOnce(&mut StandIn)) -> CmeStandIn {
    quoting_stand_in_granting(tls, 60, set).await
}

/// A stand-in quoting ES:202612, granting tokens living `life_secs`, with `set` applied.
async fn quoting_stand_in_granting(tls: &TlsFront, life_secs: u64, set: impl FnOnce(&mut StandIn)) -> CmeStandIn {
    let quote = |symbol: &str, _| (symbol == "ES:202612").then(|| ("6000".parse().unwrap(), "6001".parse().unwrap()));
    let stand_in = CmeStandIn::start(tls, life_secs, Arc::new(quote)).await;
    set(&mut stand_in.state.lock().expect("stand-in"));
    stand_in
}

/// A replacement made ready on a stand-in that answers the ping sent once it is subscribed two and a
/// half seconds late, past the bound, sending a heartbeat `heartbeat_every` in place of books if
/// given: what it hands over is bounded by a ping answered within the bound.
async fn ready_past_a_late_answer(heartbeat_every: Option<Duration>) -> Replacement {
    let tls = TlsFront::new();
    // A ping interval longer than the stand-in's 500 ms between books, so a book always comes within it.
    let ping_interval = Duration::from_secs(1);
    let timing = Timing { ping_interval, max_delay: MAX_DELAY, ..fast(Duration::from_secs(8)) };
    let stall = Some(Duration::from_millis(2_500));
    let stand_in = quoting_stand_in(&tls, |state| {
        (state.stall_after_subscribing, state.heartbeat_every) = (stall, heartbeat_every)
    })
    .await;
    let (feed, token) = cme_feed(&stand_in, &tls, timing).await;
    let replacement = feed.replace(&token).await.expect("the replacement opens");
    let behind = replacement.delay.behind(Instant::now()).expect("a ping answered");
    assert!(behind < MAX_DELAY, "{behind:?} behind");
    stand_in.stop();
    replacement
}
