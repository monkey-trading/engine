//! MONKEY emissions for V19.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, PERIOD_MS, TokenUnits};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

use crate::common::{Harness, market};

/// A market the part lists, so its trades start from no positions or quotes.
const LTC: MarketId = 22;

/// Activates V19, lists a market, and has the operator initialise emissions from now with a budget
/// that leaves a remainder. Two accounts lose in period 0, one of them netting a win against its loss; the first
/// command at the period's end finalises it. In period 1 a leveraged account is liquidated past its
/// balance, and its weight is the loss less the debt written off; two periods then pass with no
/// command, and the next one finalises period 1 and cancels them.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V19 });
    h.operator_admin(AdminCommand::UpsertMarket(market(
        LTC,
        "LTC-USD",
        AssetClass::Crypto,
        Schedule::AlwaysOpen,
        1_000,
    )));
    let start = h.ts;
    let schedule = EmissionSchedule { start_ms: start, period_budget: TokenUnits(1_000_000_000_000_000_001) };
    h.operator_admin(AdminCommand::InitEmissions(schedule));
    // Seeds no earlier part uses, so the accounts start empty.
    let (a, b, c) = (h.signer(40), h.signer(41), h.signer(42));
    h.deposit(a, "100000");
    h.deposit(b, "100000");
    h.deposit(c, "1001");

    round_trip(h, &[(a, "1"), (b, "3")], "4000", "3900");
    round_trip(h, &[(a, "1")], "3900", "3980");
    h.advance_to(start + PERIOD_MS);
    let events = h.price(LTC, "4000");
    assert!(allocated(&events, a) && allocated(&events, b), "{events:?}");

    assert!(h.market_order(c, LTC, Side::Buy, "2", "4000").iter().any(|e| matches!(e, Event::Fill(_))));
    let events = h.price(LTC, "3400");
    assert!(events.iter().any(|e| matches!(e, Event::BadDebt { account, .. } if *account == c)), "{events:?}");
    h.advance_to(start + 4 * PERIOD_MS);
    let events = h.cmd(Command::Tick);
    assert!(allocated(&events, c), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::EmissionPeriodsCancelled { first: 2, last: 3, .. })));
}

/// Each account buys its quantity of LTC at `from` and sells it at `to`.
fn round_trip(h: &mut Harness, trades: &[(AccountId, &str)], from: &str, to: &str) {
    for (price, side) in [(from, Side::Buy), (to, Side::Sell)] {
        h.price(LTC, price);
        for (account, qty) in trades {
            let events = h.market_order(*account, LTC, side, qty, price);
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
    use crate::common::operator;

    #[test]
    fn emissions_reference_session_allocates_and_cancels_without_writing_a_fixture() {
        // Signed requests, as the reference session sends them, under the version before `V19`.
        let mut h = Harness::signed();
        h.admin(AdminCommand::SetOperator(operator()));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
        session(&mut h);
        let emissions = h.engine.emissions().unwrap();
        assert_eq!(emissions.period, 4);
        assert_eq!(emissions.entitled.0 + emissions.cancelled.0, emissions.schedule.budgets(0, 4));
    }
}
