//! The ladder of a hedged account whose hedge cannot price, under the latest protocol's portfolio margin.

use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};

use super::ladder::largest;
use super::ladder_tests::{BTC, ETH, apply, deposit, fills, order, price, read};
use super::tests::{T0, prices};
use crate::sequencer::genesis_commands;
use crate::test_support::dev_config;

const HEDGED: AccountId = AccountId([13; 20]);

/// The dev engine with the crypto group's floor share at 1%, so a matched hedge keeps nearly all its credit.
fn engine() -> Engine {
    let mut cfg = dev_config();
    for group in &mut cfg.genesis.params.portfolio_grid.groups {
        group.floor_share_bps = 100;
    }
    let activation = [
        AdminCommand::SetClientAuth(cfg.client_auth().unwrap()),
        AdminCommand::SetOperator(cfg.operator().unwrap()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 },
    ];
    let commands =
        activation.into_iter().map(Command::Admin).chain(genesis_commands(&cfg.genesis)).chain([prices(100_000)]);
    let mut engine = Engine::new();
    for (seq, cmd) in commands.enumerate() {
        engine.apply(&Sequenced { seq: seq as u64, ts: T0, cmd }).unwrap();
    }
    engine
}

/// BTC long 10 against ETH short 250 on about $55k, placed under `V23`, then the latest protocol
/// activated, and ETH's quotes gone stale while BTC prices.
fn hedge_with_a_shut_member() -> Engine {
    let mut engine = engine();
    deposit(&mut engine, HEDGED, Fixed::from_int(55_000));
    price(&mut engine, ETH, Fixed::from_int(4_000), T0);
    for (market, side, qty, mid) in [(BTC, Side::Buy, 10, 100_000), (ETH, Side::Sell, 250, 4_000)] {
        let events =
            apply(&mut engine, T0, order(HEDGED, market, side, Fixed::from_int(qty), Fixed::from_int(mid), 10_000));
        assert!(fills(&events), "{events:?}");
    }
    apply(&mut engine, T0, Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }));
    price(&mut engine, BTC, Fixed::from_int(100_000), T0 + 2_000);
    engine
}

/// A declined partial close below a whole close that fills gets its own step, in size order, and each
/// step's verdict is its own size's.
#[test]
fn a_declined_partial_close_below_a_whole_close_that_fills_keeps_the_steps_in_size_order() {
    let engine = hedge_with_a_shut_member();
    let quote = read(&engine, HEDGED, BTC);
    let steps = &quote.bid;
    assert!(steps.windows(2).all(|pair| pair[0].qty < pair[1].qty), "{steps:?}");
    assert!(steps.iter().any(|step| step.declined.is_some()), "a declined partial close: {steps:?}");
    assert_eq!(steps[0].qty, Fixed::from_int(5), "{steps:?}");
    assert!(steps[0].declined.is_some() && steps[1].declined.is_none(), "{steps:?}");
    for step in steps {
        let own = engine.quote(HEDGED, BTC, Side::Sell, step.qty).unwrap();
        assert_eq!(own.declined.is_some(), step.declined.is_some(), "{step:?}");
    }
}

#[test]
fn the_largest_size_search_does_not_run_from_a_size_that_fills_down_to_a_declined_one() {
    let fits = |qty: Fixed| Some(qty);
    let (ten, five) = (Fixed::from_int(10), Fixed::from_int(5));
    assert_eq!(largest(&fits, Fixed::from_int(1), ten, five), None);
    assert_eq!(largest(&fits, Fixed::from_int(1), five, five), None);
}
