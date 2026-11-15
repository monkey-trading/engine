//! The session's `V63` part: an activation ends each market's open markout-spread interval, so a
//! change across it counts nothing.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::GRID_MS;
use engine_core::types::Millis;

use crate::common::{BTC, Harness};
use crate::v56::pass;

/// A vote activates `V63`. BTC is priced at 100,000 just before a grid point and kept fresh through
/// the activation, which samples that grid point under the earlier rules and then ends its open
/// interval; BTC is priced at 100,200 before the next grid point, so the 20 bps change to it counts
/// nothing and BTC's markout spread stays at its floor.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V63 };
    let events = pass(h, 40, false, Some(activate.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let rules = &h.engine.markets()[&BTC].spec.price_rules;
    // A quote stays fresh for a second, so the prices below, 800 ms apart, leave no gap.
    assert_eq!(rules.max_age_ms, 1_000);
    let floor = h.engine.markets()[&BTC].spec.markout_floor_bps.expect("V54 lists a floor");
    let grid: Millis = h.ts.div_euclid(GRID_MS) * GRID_MS + 2 * GRID_MS;
    h.advance_to(grid - 100);
    h.price(BTC, "100000");
    h.advance_to(grid + 200);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 40, command: activate })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V63);
    for offset in [700, 1_500, 2_300, 3_100, 3_900] {
        h.advance_to(grid + offset);
        h.price(BTC, "100000");
    }
    h.advance_to(grid + 4_700);
    h.price(BTC, "100200");
    h.advance_to(grid + GRID_MS + 100);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markout_spread_bps(BTC), Some(floor));
}
