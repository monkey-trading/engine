//! The session's `V58` part: a voted ceiling adoption binds a listing that names no markout floor,
//! `V58`'s activation gives it a crypto listing's floor, a later adoption binding one that names none
//! is refused, and a change across an index that quote expiry alone ends and brings back between two
//! grid points counts nothing.

use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::GRID_MS;
use engine_core::types::{MarketId, Millis};

use crate::common::{BTC, Harness, f, quote};
use crate::v56::pass;

/// A market no part lists, whose adoption binds a listing that names no floor before `V58`.
const APT: MarketId = 60;
/// A market no part lists, whose adoption `V58` refuses.
const SUI: MarketId = 61;

/// With rotation on, a vote passes a ceiling adoption for APT binding BTC's listing as APT's with no
/// markout floor, `V58`'s activation, and an adoption for SUI binding the same, all applied seven days
/// on in that order: APT's listing takes a crypto listing's 1 bp floor at the activation, and SUI's
/// adoption is refused. Then BTC's five venues quote an index that is valid at a grid point, divergent
/// once two of its quotes age out and valid again once a third does, before a price at another level:
/// the change to the next grid point counts nothing, so its markout spread stays at its floor.
pub(crate) fn session(h: &mut Harness) {
    let unfloored = |id: MarketId, symbol: &str| MarketSpec {
        id,
        symbol: symbol.into(),
        markout_floor_bps: None,
        ..h.engine.baseline_spec(BTC).expect("listed")
    };
    let applies_at = h.ts + NOTICE_MS;
    let adopt = |market, listing| AdminCommand::AdoptCloseSpreadCeiling {
        market,
        max_close_spread_bps: 500,
        effective_at: applies_at,
        listing: Some(listing),
    };
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V58 };
    let proposals =
        [(11, adopt(APT, unfloored(APT, "APT-USD"))), (12, activate), (13, adopt(SUI, unfloored(SUI, "SUI-USD")))];
    for (proposal, command) in &proposals {
        let events = pass(h, *proposal, false, Some(command.clone()));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    h.advance_to(applies_at);
    let outcomes: Vec<Option<Event>> = proposals
        .into_iter()
        .map(|(proposal, command)| {
            h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command }))).last().cloned()
        })
        .collect();
    let refused = Event::AdminRejected { reason: "market 61 names no markout floor".into() };
    assert_eq!(outcomes, [Some(Event::AdminApplied), Some(Event::AdminApplied), Some(refused)]);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V58);
    assert_eq!(h.engine.operator_bounds().proposed_listings[&APT].markout_floor_bps, Some(f("1")));

    expiry_lapse(h);
}

/// BTC's quotes, fresh for a second within a 50 bps bound, give an index at the grid point that two
/// expiries end and a third brings back; BTC is then priced at another level to the next grid point.
fn expiry_lapse(h: &mut Harness) {
    let rules = &h.engine.markets()[&BTC].spec.price_rules;
    assert_eq!((rules.max_age_ms, rules.max_divergence_bps), (1_000, 50));
    let floor = h.engine.markets()[&BTC].spec.markout_floor_bps;
    let grid = h.ts.div_euclid(GRID_MS) * GRID_MS + 2 * GRID_MS;
    // Two venues at 100,000 and three at 100,511, 50.8 bps apart about the median: one high ages out
    // 200 ms past the grid point, another at 300 ms leaves the median at 100,000 and the spread at
    // 51.1 bps, and a low at 400 ms brings it back to 50.9 bps about their mean.
    let (low, high) = ("100000", "100511");
    let expires = |at: Millis| at - 1_001;
    h.advance_to(grid - 100);
    let quotes = vec![
        quote("venue-a", low, expires(grid + 400)),
        quote("venue-b", low, h.ts),
        quote("venue-c", high, expires(grid + 200)),
        quote("venue-d", high, h.ts),
        quote("venue-e", high, expires(grid + 300)),
    ];
    h.cmd(Command::Prices { market: BTC, quotes });
    for offset in [800, 1_700, 2_600, 3_500, 4_400] {
        h.advance_to(grid + offset);
        let quotes = vec![quote("venue-b", low, h.ts), quote("venue-d", low, h.ts)];
        h.cmd(Command::Prices { market: BTC, quotes });
    }
    h.advance_to(grid + GRID_MS + 100);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markout_spread_bps(BTC), floor);
}
