//! MONKEY emissions for V23.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, PERIOD_MS, TokenUnits};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};

use crate::common::{ETH, Harness};

/// Activates V23 and has the operator initialise emissions from now with a budget that leaves a
/// remainder. Two accounts lose in period 0, one of them netting a win against its loss; the first
/// command at the period's end finalises it. In period 1 a leveraged account is liquidated past its
/// balance, and its weight is the loss less the debt written off; two periods then pass with no
/// command, and the next one finalises period 1 and cancels them.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 });
    let start = h.ts;
    let schedule = EmissionSchedule { start_ms: start, period_budget: TokenUnits(1_000_000_000_000_000_001) };
    h.operator_admin(AdminCommand::InitEmissions(schedule));
    let (a, b, c) = (h.signer(30), h.signer(31), h.signer(32));
    h.deposit(a, "100000");
    h.deposit(b, "100000");
    h.deposit(c, "1001");

    round_trip(h, &[(a, "1"), (b, "3")], "4000", "3900");
    round_trip(h, &[(a, "1")], "3900", "3980");
    h.advance_to(start + PERIOD_MS);
    let events = h.price(ETH, "4000");
    assert!(allocated(&events, a) && allocated(&events, b), "{events:?}");

    assert!(h.market_order(c, ETH, Side::Buy, "2", "4000").iter().any(|e| matches!(e, Event::Fill(_))));
    let events = h.price(ETH, "3400");
    assert!(events.iter().any(|e| matches!(e, Event::BadDebt { account, .. } if *account == c)), "{events:?}");
    h.advance_to(start + 4 * PERIOD_MS);
    let events = h.cmd(Command::Tick);
    assert!(allocated(&events, c), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::EmissionPeriodsCancelled { first: 2, last: 3, .. })));
}

/// Each account buys its quantity of ETH at `from` and sells it at `to`.
fn round_trip(h: &mut Harness, trades: &[(AccountId, &str)], from: &str, to: &str) {
    for (price, side) in [(from, Side::Buy), (to, Side::Sell)] {
        h.price(ETH, price);
        for (account, qty) in trades {
            let events = h.market_order(*account, ETH, side, qty, price);
            assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
        }
    }
}

/// Whether `events` allocate `account` a share of a period.
fn allocated(events: &[Event], account: AccountId) -> bool {
    events.iter().any(|e| matches!(e, Event::EmissionAllocated { account: to, .. } if *to == account))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emissions_reference_session_allocates_and_cancels_without_writing_a_fixture() {
        let mut h = Harness::v17();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
        session(&mut h);
        let emissions = h.engine.emissions().unwrap();
        assert_eq!(emissions.period, 4);
        assert_eq!(emissions.entitled.0 + emissions.cancelled.0, emissions.schedule.budgets(0, 4));
    }
}
