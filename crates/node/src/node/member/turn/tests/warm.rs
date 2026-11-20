//! A member's venue feeds for its own turn: opened within the lead of its boundary, kept while it
//! takes the turn, closed when it follows on or its start is no replica's; and the wait after a failed
//! follow stint.

use super::*;
use crate::config::FeedProvider;
use crate::config::{SecretName, SecretValue};
use crate::feeds::status::FeedStatus;
use crate::feeds::tests::fake::{accept, listen};
use crate::node::member::turn::tries::retry_after;
use crate::node::price_feeds::{self, WARM_LEAD};
use crate::node::secrets::Delivered;
use tokio::time::timeout;

/// Opened once trusted time is within the lead of the member's own next turn and kept while it takes
/// the turn, the feeds connect to their venue; following on, or a start that is no replica's, closes
/// them.
#[tokio::test]
async fn a_member_holds_its_venue_feeds_only_around_its_own_turn() {
    let (listener, url) = listen().await;
    let mut fixture = Fixture::start("member-warm-feeds", true).await;
    fixture.cfg.feeds.provider = FeedProvider::Venues;
    fixture.cfg.feeds.venues = Some(
        toml::from_str(&format!(
            "timeout = '5s'\nreconnect_min = '40ms'\nreconnect_max = '200ms'\nstale_after = '3s'\n\
             min_quote_spacing = '10ms'\nping_interval = '1s'\nmax_delay = '3s'\n[[connections]]\nvenue = 'kraken'\n\
             url = {url:?}\nsymbols = [{{ market = 1, symbol = 'BTC/USD' }}]"
        ))
        .unwrap(),
    );
    let delivered = Delivered::default();
    let mut member = fixture.member().with_feeds(&delivered);
    let lead = Millis::try_from(WARM_LEAD.as_millis()).unwrap();
    let boundary = FIRST + 10 * lead;

    fixture.time.set(boundary - lead - 1_000);
    member.warm_for(false, Some(boundary), None).await;
    assert!(member.warm.is_none(), "the feeds opened before the lead");
    fixture.time.set(boundary - lead + 1_000);
    member.warm_for(false, Some(boundary), None).await;
    assert!(member.warm.is_some(), "the feeds did not open within the lead");
    accept(&listener).await;
    member.warm_for(true, None, None).await;
    assert!(member.warm.is_some(), "the feeds closed while the member took its turn");
    member.warm_for(false, None, None).await;
    assert!(member.warm.is_none(), "the feeds stayed open with the member following on");

    member.warm_for(true, None, None).await;
    let mut turn = Ok(Turn::Hold);
    member.hand_feeds(&mut turn);
    assert!(member.warm.is_none(), "a whole-log start kept the feeds the member opened");
}

/// A connection that carries a key does not open in the warm window, since the vendor allows one live
/// connection per account and the incumbent holds it; it opens when the member's turn starts, while a
/// keyless one opens in the window.
#[tokio::test]
async fn a_keyed_feed_opens_at_the_turn_and_a_keyless_one_in_the_warm_window() {
    let (keyless, keyless_url) = listen().await;
    let (keyed, keyed_url) = listen().await;
    let mut fixture = Fixture::start("member-warm-keyed-feeds", true).await;
    fixture.cfg.feeds.provider = FeedProvider::Venues;
    fixture.cfg.feeds.venues = Some(
        toml::from_str(&format!(
            "timeout = '5s'\nreconnect_min = '40ms'\nreconnect_max = '200ms'\nstale_after = '3s'\n\
             min_quote_spacing = '10ms'\nping_interval = '1s'\nmax_delay = '3s'\n\
             [[connections]]\nvenue = 'kraken'\nurl = {keyless_url:?}\n\
             symbols = [{{ market = 1, symbol = 'BTC/USD' }}]\n\
             [[connections]]\nvenue = 'massive'\nurl = {keyed_url:?}\napi_key_secret = 'massive_key'\n\
             symbols = [{{ market = 2, symbol = 'C:EURUSD' }}]"
        ))
        .unwrap(),
    );
    let mut delivered = Delivered::default();
    delivered
        .values
        .insert(SecretName::try_from("massive_key".to_owned()).unwrap(), SecretValue::test("FAKE-massive-key"));
    let mut member = fixture.member().with_feeds(&delivered);
    let lead = Millis::try_from(WARM_LEAD.as_millis()).unwrap();
    let boundary = FIRST + 10 * lead;

    fixture.time.set(boundary - lead + 1_000);
    member.warm_for(false, Some(boundary), None).await;
    assert!(member.warm.is_some(), "the feeds did not open within the lead");
    let _keyless_connection = accept(&keyless).await;
    assert!(
        timeout(Duration::from_millis(500), keyed.accept()).await.is_err(),
        "the keyed feed connected in the warm window"
    );

    let mut status = FeedStatus::default();
    let ((tx, _rx), live) = price_feeds::channel(member.warm.take(), &mut status);
    let mut tasks = tokio::task::JoinSet::new();
    price_feeds::start(&mut tasks, live, (fixture.cfg.feeds.clone(), &delivered), &tx, &status).unwrap();
    let _keyed_connection = accept(&keyed).await;
    assert!(
        timeout(Duration::from_millis(500), keyless.accept()).await.is_err(),
        "the keyless feed was opened a second time at the turn"
    );
}

/// After a failed stint a member tries again a second later, then twice as long each time, up to a
/// stint.
#[test]
fn a_failed_stint_is_retried_soon_and_then_less_often_up_to_a_stint() {
    let waits: Vec<Duration> =
        std::iter::successors(Some(RETRY_PAUSE), |wait| Some(retry_after(*wait))).take(8).collect();
    assert_eq!(waits[0], RETRY_PAUSE);
    assert!(waits.windows(2).all(|pair| pair[1] == (pair[0] * 2).min(STINT)), "{waits:?}");
    assert_eq!(*waits.last().unwrap(), STINT, "{waits:?}");
}
