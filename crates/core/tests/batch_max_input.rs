//! A close-all closes a position larger than the quantity input limit, a piece of at most
//! `Fixed::MAX_INPUT` per entry, from `V45`; under `V44` it refuses such a close whole.

mod common;

use common::batch::{close_all, closes, market_order};
use common::events::{fills, rejection};
use common::session_keys::funded_at;
use common::{BTC, Client, Harness, f};
use engine_core::Fixed;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

/// BTC at five hundredths of a cent, so 1.2 billion units are a $600,000 position: inside the
/// funded account's margin and the house's caps, and under the first size-spread tier per fill.
const PRICE: &str = "0.0005";

/// An account that traded `each` units of BTC on `side` `orders` times.
fn holding(version: ProtocolVersion, side: Side, each: &str, orders: usize) -> (Harness, Client) {
    let (mut h, client, _) = funded_at(version);
    h.price(BTC, PRICE);
    for _ in 0..orders {
        assert_eq!(fills(&market_order(&mut h, &client, BTC, side, each, PRICE)).len(), 1);
    }
    (h, client)
}

fn position(h: &Harness, client: &Client) -> Fixed {
    h.engine.accounts()[&client.account].position(BTC).qty
}

fn close(h: &mut Harness, client: &Client) -> Vec<Event> {
    let nonce = h.engine.next_request_nonce(client.account);
    let cmd = close_all(h, client, client.account, nonce, closes(&[BTC], PRICE), None);
    h.cmd(cmd)
}

#[test]
fn a_position_past_the_input_limit_closes_in_pieces_of_the_limit_from_v45() {
    for side in [Side::Buy, Side::Sell] {
        let (mut h, client) = holding(ProtocolVersion::V45, side, "600000000", 2);
        assert_eq!(position(&h, &client), side.signed(f("1200000000")));
        let events = close(&mut h, &client);
        let [fill] = fills(&events)[..] else { panic!("one fill: {events:?}") };
        assert_eq!(fill.qty, Fixed::MAX_INPUT, "{side:?}: up to the limit per entry");
        assert_eq!(fill.position, side.signed(f("200000000")), "{side:?}: the rest stays in the fill's position");
        let events = close(&mut h, &client);
        let [fill] = fills(&events)[..] else { panic!("one fill: {events:?}") };
        assert_eq!((fill.qty, fill.position), (f("200000000"), Fixed::ZERO), "{side:?}");
        assert!(position(&h, &client).is_zero());
        h.assert_conserved(Fixed::ZERO);
    }
}

#[test]
fn a_position_at_the_input_limit_closes_whole_and_one_unit_past_it_leaves_one_unit() {
    let (mut h, client) = holding(ProtocolVersion::V45, Side::Buy, "500000000", 2);
    assert_eq!(position(&h, &client), Fixed::MAX_INPUT);
    let events = close(&mut h, &client);
    assert_eq!(fills(&events)[0].position, Fixed::ZERO);

    let (mut h, client) = holding(ProtocolVersion::V45, Side::Buy, "500000000", 2);
    assert_eq!(fills(&market_order(&mut h, &client, BTC, Side::Buy, "0.000000001", PRICE)).len(), 1);
    let events = close(&mut h, &client);
    let [fill] = fills(&events)[..] else { panic!("one fill: {events:?}") };
    assert_eq!((fill.qty, fill.position), (Fixed::MAX_INPUT, Fixed::from_raw(1)));
}

#[test]
fn under_v44_a_close_all_of_a_position_past_the_limit_is_refused_and_leaves_it_open() {
    let (mut h, client) = holding(ProtocolVersion::V44, Side::Buy, "600000000", 2);
    let events = close(&mut h, &client);
    assert!(
        matches!(rejection(&events), Reject::InvalidOrder { reason } if reason == "qty out of range"),
        "{events:?}"
    );
    assert_eq!(position(&h, &client), f("1200000000"));
}
