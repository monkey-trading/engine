//! Helpers for the emission tests: funded accounts, losing and winning round trips, and the
//! emission events a command returns.

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::emissions::EmissionSchedule;
use engine_core::event::Event;
use engine_core::types::{AccountId, Side};

use crate::common::events::fills;
use crate::common::{BTC, Harness, acct};

/// Accounts 1 to `N`, each with 1m deposited.
pub fn funded<const N: usize>(h: &mut Harness) -> [AccountId; N] {
    std::array::from_fn(|i| {
        let account = acct(i as u8 + 1);
        h.deposit(account, "1000000");
        account
    })
}

/// Each account buys its quantity of BTC at `from` and sells it at `to`, realising `to - from` a
/// unit; normal sizes fill at the index.
pub fn round_trip(h: &mut Harness, trades: &[(AccountId, &str)], from: &str, to: &str) {
    for (price, side) in [(from, Side::Buy), (to, Side::Sell)] {
        h.price(BTC, price);
        for (account, qty) in trades {
            let events = h.market_order(*account, BTC, side, qty, price);
            assert_eq!(fills(&events).len(), 1, "{events:?}");
        }
    }
}

/// The schedule the log recorded.
pub fn schedule(h: &Harness) -> EmissionSchedule {
    h.engine.emissions().expect("emissions are initialised").schedule
}

/// Ticks at the end of the open period, finalising it, and returns the tick's events.
pub fn next_period(h: &mut Harness) -> Vec<Event> {
    let open = h.engine.emissions().expect("emissions are initialised").period;
    h.advance_to(schedule(h).period_end(open));
    h.cmd(Command::Tick)
}

/// The allocations among `events`: account, period, weight, and base units.
pub fn allocations(events: &[Event]) -> Vec<(AccountId, u64, Fixed, u128)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::EmissionAllocated { account, period, weight, amount } => {
                Some((*account, *period, *weight, amount.0))
            }
            _ => None,
        })
        .collect()
}

/// The finalised periods among `events`: period, budget, allocated, and cancelled.
pub fn closed(events: &[Event]) -> Vec<(u64, u128, u128, u128)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::EmissionPeriodClosed { period, budget, allocated, cancelled } => {
                Some((*period, budget.0, allocated.0, cancelled.0))
            }
            _ => None,
        })
        .collect()
}

/// The periods cancelled with no command among `events`: first, last, and base units.
pub fn cancelled_ranges(events: &[Event]) -> Vec<(u64, u64, u128)> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::EmissionPeriodsCancelled { first, last, cancelled } => Some((*first, *last, cancelled.0)),
            _ => None,
        })
        .collect()
}
