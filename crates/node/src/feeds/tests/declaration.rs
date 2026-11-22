//! The test account's deposit declaration, stamped after a market's session boundary: the
//! command's events then carry the boundary's before the attestation.

use std::sync::Arc;
use std::time::Duration;

use engine_core::event::Event;
use engine_core::market::MarketSpec;

use crate::clock::now_ms;
use crate::clock::test_clock::TestClock;
use crate::feeds::tests::hours::genesis;
use crate::feeds::tests::rwa_support::{around_the_clock, events_of, listing};
use crate::feeds::tests::scenario::Node;

const AAPL: u32 = 40;

#[tokio::test]
async fn a_declaration_stamped_after_a_market_opens_is_attested_behind_the_open() {
    let start = now_ms();
    let clock = Arc::new(TestClock::at(start));
    let closed = MarketSpec { schedule: around_the_clock(Some((start - 60_000, start + 1_000))), ..listing(AAPL) };
    let node = Node::boot_clocked("open-before-declaration", &genesis(vec![closed]), clock.clone()).await;
    let applied = node.record();

    // No command runs while the clock passes Apple's open, so the declaration's stamp is the first after it.
    clock.advance(Duration::from_secs(2));
    node.deposit().await;

    let events = events_of(&applied);
    let opened = events.iter().position(|event| *event == Event::MarketOpened { market: AAPL });
    let attested = events.iter().position(|event| matches!(event, Event::EligibilityAttested { .. }));
    assert!(opened < attested && opened.is_some(), "the open must come first: {events:?}");
}
