//! Emission eligibility during corporate-action catch-up: from `V33` it is judged at the enclosing
//! command's stamp, while the boundary time the clock rewinds to keeps ordering the work.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::*;
use crate::support::*;

/// The fixture at `version`, with emissions starting one millisecond after the opening and the
/// stamp of the catch-up command a second later.
fn crossing(version: ProtocolVersion) -> (Harness, i64) {
    let mut h = held("2", "4");
    if version >= ProtocolVersion::V33 {
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    let start_ms = open() + 1;
    h.operator_admin(AdminCommand::InitEmissions(EmissionSchedule { start_ms, period_budget: TokenUnits(1_000_000) }));
    (h, start_ms + 1_000)
}

#[test]
fn cash_in_lieu_at_a_boundary_before_the_start_counts_when_the_command_is_after_it() {
    for (version, counted) in [(ProtocolVersion::V23, false), (ProtocolVersion::V33, true)] {
        let (mut h, at) = crossing(version);
        h.price(AAPL, "80.5");
        accepted(&schedule(&mut h, split("reverse", 1, 3)));
        let events = tick(&mut h, at);
        assert!(events.iter().any(|e| matches!(e, Event::CorporateCashInLieu { .. })), "{events:?}");
        let realised: Vec<_> =
            h.engine.period_results().iter().map(|(account, result)| (*account, result.realised)).collect();
        let expected = [(acct(1), f("0.000000001")), (acct(2), f("-0.000000001"))];
        assert_eq!(realised, if counted { expected.to_vec() } else { vec![] }, "{version:?}");
    }
}

#[test]
fn cash_in_lieu_with_the_command_before_the_start_still_counts_nothing() {
    let (mut h, _) = crossing(ProtocolVersion::V33);
    h.price(AAPL, "80.5");
    accepted(&schedule(&mut h, split("reverse", 1, 3)));
    // The opening and the command that applies it both precede the start.
    let events = tick(&mut h, open());
    assert!(events.iter().any(|e| matches!(e, Event::CorporateCashInLieu { .. })), "{events:?}");
    assert!(h.engine.period_results().is_empty());
}

#[test]
fn a_liquidation_at_a_rollover_boundary_before_the_start_counts_when_the_command_is_after_it() {
    for (version, counted) in [(ProtocolVersion::V23, false), (ProtocolVersion::V33, true)] {
        let (mut h, trader, command_ms) = liquidated_by_a_missed_rollover(version);
        let events = h.cmd(Command::Tick);
        assert_eq!(h.ts, command_ms);
        let liquidation = events
            .iter()
            .find_map(|e| match e {
                Event::Fill(fill) if fill.liquidation && fill.account == trader => Some(fill.realised_pnl),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{version:?}: the rollover's financing liquidates the trader: {events:?}"));
        assert!(liquidation.is_negative());
        let recorded = h.engine.period_results().get(&trader).map(|result| result.realised);
        assert_eq!(recorded, counted.then_some(liquidation), "{version:?}");
        h.assert_conserved(Fixed::ZERO);
    }
}

/// A trader long one BTC whose equity sits just above its close-out level, a rollover due before the
/// emissions start, and a catch-up command after it, not yet applied. Returns the harness set to the
/// command's stamp, the trader, and that stamp.
fn liquidated_by_a_missed_rollover(version: ProtocolVersion) -> (Harness, engine_core::types::AccountId, i64) {
    let mut h = held("0", "0");
    let trader = acct(3);
    h.deposit(trader, "10000");
    h.price(BTC, "100000");
    assert!(h.market_order(trader, BTC, Side::Buy, "1", "100000").iter().any(|e| matches!(e, Event::Fill(_))));
    if version >= ProtocolVersion::V33 {
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    let rollover = h.engine.next_rollover().expect("a position schedules the rollover");
    let mut spec = h.engine.markets()[&BTC].spec.clone();
    spec.financing.markup_bps = 10_000;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let start_ms = rollover + 1;
    h.operator_admin(AdminCommand::InitEmissions(EmissionSchedule { start_ms, period_budget: TokenUnits(1_000_000) }));
    // 94,900 leaves 4,900 of equity above the 4,745 close-out level; one day's financing breaks it.
    h.advance_to(rollover - 500);
    let events = h.price(BTC, "94900");
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    h.advance_to(start_ms + 1_000);
    let ts = h.ts;
    (h, trader, ts)
}
