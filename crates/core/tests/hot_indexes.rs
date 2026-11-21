//! Derived indexes survive recovery and preserve trigger priority and close-out membership.

mod common;

use common::events::fills;
use common::{BTC, ETH, Harness, acct, f};
use engine_core::Engine;
use engine_core::command::{Command, OrderKind};
use engine_core::types::Side;

#[test]
fn crossing_triggers_preserve_time_priority_across_prices_and_order_kinds() {
    let mut h = Harness::v17();
    let account = acct(1);
    h.deposit(account, "1000000");
    h.price(BTC, "100");
    for (side, kind) in [
        (Side::Buy, OrderKind::Stop { price: f("110") }),
        (Side::Sell, OrderKind::Limit { price: f("105") }),
        (Side::Buy, OrderKind::Stop { price: f("105") }),
        (Side::Buy, OrderKind::Limit { price: f("90") }),
        (Side::Sell, OrderKind::Stop { price: f("90") }),
    ] {
        h.order(account, BTC, side, "1", kind);
    }
    let ids: Vec<_> = h.engine.accounts()[&account].orders.keys().copied().collect();
    assert_eq!(ids.len(), 5);
    let events = h.price(BTC, "110");
    assert_eq!(fills(&events).iter().map(|fill| fill.order_id).collect::<Vec<_>>(), ids[..3]);
    let events = h.price(BTC, "90");
    assert_eq!(fills(&events).iter().map(|fill| fill.order_id).collect::<Vec<_>>(), ids[4..]);
    // The buy limit remains eligible after the spread prevents an equality-price fill.
    assert!(h.engine.accounts()[&account].orders.contains_key(&ids[3]));
    let events = h.price(BTC, "89");
    assert_eq!(fills(&events).iter().map(|fill| fill.order_id).collect::<Vec<_>>(), ids[3..4]);
    assert!(h.engine.accounts()[&account].orders.is_empty());
    assert!(h.engine.markets()[&BTC].resting.is_empty());
    assert_eq!(fills(&h.price(BTC, "110")).len(), 0);
}

#[test]
fn rebuilding_indexes_after_every_command_preserves_events_serialization_and_roots() {
    let mut h = Harness::v17();
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "100000");
    h.deposit(b, "100000");
    h.price(BTC, "100");
    h.price(ETH, "100");
    h.market_order(a, BTC, Side::Buy, "10", "100");
    h.market_order(b, ETH, Side::Buy, "10", "100");
    h.order(a, BTC, Side::Buy, "1", OrderKind::Stop { price: f("110") });
    h.order(b, BTC, Side::Sell, "1", OrderKind::Stop { price: f("90") });
    let cancelled = *h.engine.accounts()[&b].orders.keys().next().unwrap();
    h.cmd(Command::CancelOrder { account: b, order_id: cancelled, auth: None });
    h.price(BTC, "110");
    h.market_order(a, BTC, Side::Sell, "11", "110");
    h.market_order(a, ETH, Side::Buy, "5", "100");
    h.price(BTC, "1");
    h.price(ETH, "1");

    let mut cached = Engine::new();
    let mut recovered = Engine::new();
    for command in &h.log {
        recovered = serde_json::from_slice(&serde_json::to_vec(&recovered).unwrap()).unwrap();
        assert_eq!(cached.apply(command).unwrap(), recovered.apply(command).unwrap());
        assert_eq!(serde_json::to_vec(&cached).unwrap(), serde_json::to_vec(&recovered).unwrap());
        assert_eq!(cached.state_root(), recovered.state_root());
    }
    assert!(cached.accounts()[&a].orders.is_empty());
    assert!(!cached.accounts()[&a].positions.contains_key(&BTC));
}

/// A state decoded with an order missing from its market's index of resting orders still hashes to the
/// same root, since the market leaf leaves the index out and the order's own leaf holds the order:
/// rebuilding the index from the accounts' orders gives the state back, and the order fills as before.
#[test]
fn an_index_of_resting_orders_a_decoded_state_lost_is_rebuilt_from_its_orders() {
    let mut h = Harness::v17();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100");
    h.order(a, BTC, Side::Buy, "1", OrderKind::Stop { price: f("110") });
    h.order(a, ETH, Side::Buy, "1", OrderKind::Limit { price: f("90") });
    assert!(h.engine.resting_matches_orders());
    let order_id = *h.engine.markets()[&BTC].resting.keys().next().unwrap();
    let root = h.engine.state_root();
    let mut encoded = serde_json::to_value(&h.engine).unwrap();
    let resting = encoded["markets"][BTC.to_string()]["resting"].as_object_mut().unwrap();
    assert!(resting.remove(&order_id.to_string()).is_some(), "the index held the order");
    let mut decoded: Engine = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.state_root(), root, "the root does not bind the index");
    assert!(!decoded.resting_matches_orders());

    decoded.rebuild_resting();
    assert!(decoded.resting_matches_orders());
    decoded.restore_indexes();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), serde_json::to_vec(&h.engine).unwrap());
    let events = h.price(BTC, "110");
    assert_eq!(fills(&events).iter().map(|fill| fill.order_id).collect::<Vec<_>>(), [order_id]);
    assert_eq!(decoded.apply(h.log.last().unwrap()).unwrap(), events);
}

/// An order indexed in BTC-USD, the earlier market, as well as its own fails the check, as `rebuild_resting` would drop it
/// from the earlier market: each market's index is compared on its own.
#[test]
fn an_order_indexed_in_an_earlier_market_as_well_as_its_own_fails_the_check() {
    let mut h = Harness::v17();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100");
    h.order(a, BTC, Side::Buy, "1", OrderKind::Stop { price: f("110") });
    h.order(a, ETH, Side::Buy, "1", OrderKind::Limit { price: f("90") });
    assert!(h.engine.resting_matches_orders());
    let (order_id, account) =
        h.engine.markets()[&ETH].resting.iter().map(|(id, account)| (*id, *account)).next().unwrap();
    let mut encoded = serde_json::to_value(&h.engine).unwrap();
    let earlier = encoded["markets"][BTC.to_string()]["resting"].as_object_mut().unwrap();
    earlier.insert(order_id.to_string(), serde_json::to_value(account).unwrap());
    let mut doubled: Engine = serde_json::from_value(encoded).unwrap();
    assert!(doubled.markets()[&BTC].resting.contains_key(&order_id), "the earlier market indexes it too");
    assert!(!doubled.resting_matches_orders());
    doubled.rebuild_resting();
    assert!(doubled.resting_matches_orders());
    assert!(!doubled.markets()[&BTC].resting.contains_key(&order_id), "rebuilding drops it from the earlier market");
}
