//! `V66`'s weekend close-out on a market with no index, through `Engine::apply`: it fills at the market's
//! mark at the higher of the market's stress level and the level the house's loss sets then, though no
//! index re-reads that level; it changes no level or cooldown; and its spread still clamps to the
//! market's close-spread ceiling. `V62` fills at the level the market last set.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::house_loss::{level, level_two, loss};
use common::toxicity::v54_params;
use common::weekend::{OIL, OIL_WEEKEND_BPS, chicago, client, genesis_with, globex};
use common::{BTC, Harness, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Fill;
use engine_core::params::{ProtocolParams, StressLevel, StressTrigger};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

/// Oil's close-spread ceiling in the weekend harness.
const OIL_CEILING_BPS: i64 = 25;

#[test]
fn a_close_out_on_a_market_with_no_index_pays_the_level_the_house_loss_sets_now() {
    let scenario = Scenario { version: ProtocolVersion::V66, ..Scenario::default() };
    let (before, after) = (Scenario { version: ProtocolVersion::V62, ..scenario }.run(), scenario.run());
    // Level 0 charges 2 bp on the first $500k of the $700k and 4 bp on the rest, 2.57 bp. Level 2 adds
    // its 6 bp minimum to four times that.
    assert_eq!((before.fill.spread_bps, after.fill.spread_bps), (2, 16), "{before:?} {after:?}");
    assert_eq!((before.fill.price, after.fill.price), (f("69.982"), f("69.886")));
    for run in [&before, &after] {
        assert!(run.fill.liquidation && run.fill.market == OIL && run.fill.index == f("70"), "{:?}", run.fill);
        // Oil had no index when the house fell behind, so it stayed at level 0 and BTC rose to level 2.
        assert!(run.loss.is_positive());
        assert_eq!((run.oil_level_before, run.btc_level_before), (0, 2));
    }
    // The close-out changes no level, automatic level or cooldown start.
    assert_eq!(after.oil_after, after.oil_before);
}

#[test]
fn a_close_out_never_pays_less_than_the_level_the_market_holds() {
    // The house is not behind, so its loss sets level 0, and the operator's floor holds oil at level 1.
    let flat = Scenario { btc_rises: false, ..Scenario::default() };
    let (floored, unfloored) = (Scenario { floor: true, ..flat }.run(), flat.run());
    assert_eq!((floored.oil_level_before, floored.loss.is_positive()), (1, false));
    assert!(floored.fill.spread_bps >= params().stress_levels[1].min_spread_bps, "{:?}", floored.fill);
    assert!(floored.fill.spread_bps > unfloored.fill.spread_bps, "{:?} {:?}", floored.fill, unfloored.fill);
}

#[test]
fn the_close_out_spread_stays_within_the_markets_ceiling() {
    // $20m of oil pays about 5.9 bp over its bands (2 bp on the first $500k, 4 bp to $10m, 8 bp above), so
    // level 2's 6 bp minimum and four times that is 30 bp, over oil's 25 bp ceiling.
    let large = Scenario { units: "285000", funds: "1200000", ..Scenario::default() };
    let (before, capped) = (Scenario { version: ProtocolVersion::V62, ..large }.run(), large.run());
    assert_eq!((before.fill.spread_bps, capped.fill.spread_bps), (5, OIL_CEILING_BPS), "{before:?} {capped:?}");
}

// ── Helpers ──

/// Friday 23 October 2026's 16:00 Globex close in Chicago, which starts that weekend's gap.
fn friday_close() -> Millis {
    chicago(2026, 10, 23, 16, 0)
}

/// What a run of the scenario leaves.
#[derive(Debug)]
struct Run {
    fill: Fill,
    loss: Fixed,
    oil_level_before: usize,
    btc_level_before: usize,
    /// Oil's effective level, automatic level and the time it began.
    oil_before: (usize, usize, Millis),
    oil_after: (usize, usize, Millis),
}

/// Client 1 holds $700k of oil it cannot hold over the weekend, and client 2 holds five bitcoin. Five
/// seconds before Friday's close oil is priced flat, and two seconds before it, with oil's quote stale,
/// bitcoin rises 4% if `btc_rises`, which puts the house behind. With `floor`, the operator holds oil at
/// level 1.
#[derive(Clone, Copy)]
struct Scenario {
    version: ProtocolVersion,
    floor: bool,
    btc_rises: bool,
    /// Client 1's oil, and the funds it opens it with.
    units: &'static str,
    funds: &'static str,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario { version: ProtocolVersion::V66, floor: false, btc_rises: true, units: "10000", funds: "10000" }
    }
}

impl Scenario {
    /// The first command after the close, which closes out client 1.
    fn run(self) -> Run {
        let params = params();
        let mut h = genesis_with(self.version, globex(vec![]), OIL_WEEKEND_BPS, 0, params);
        let (a, b) = (client(&mut h, 1, self.funds), client(&mut h, 2, "1000000"));
        h.price(OIL, "70");
        h.price(BTC, "100000");
        only_fill(&trade(&mut h, &a, OIL, Side::Buy, self.units, "70"));
        only_fill(&trade(&mut h, &b, BTC, Side::Buy, "5", "100000"));
        let close = friday_close();
        h.advance_to(close - 5_000);
        h.price(OIL, "70");
        h.advance_to(close - 2_000);
        h.price(BTC, if self.btc_rises { "104000" } else { "100000" });
        if self.floor {
            h.operator_admin(AdminCommand::SetStressFloor { market: Some(OIL), level: 1 });
        }
        let (oil_level_before, btc_level_before) = (level(&h, OIL), level(&h, BTC));
        let oil_before = oil_stress(&h);
        let loss = loss(&h);
        h.advance_to(close + 60 * 60 * 1000);
        let fill = only_fill(&h.cmd(Command::Tick)).clone();
        assert_eq!(fill.account, a.account);
        let oil_after = oil_stress(&h);
        Run { fill, loss, oil_level_before, btc_level_before, oil_before, oil_after }
    }
}

/// The weekend harness's parameters with a third level whose house loss threshold is zero.
fn params() -> ProtocolParams {
    let mut params = v54_params();
    let trigger = Some(StressTrigger { house_loss: Some(Fixed::ZERO), ..StressTrigger::default() });
    params.stress_levels.push(StressLevel { trigger, ..level_two() });
    params
}

/// Oil's effective level, automatic level and the time it began.
fn oil_stress(h: &Harness) -> (usize, usize, Millis) {
    let stress = &h.engine.markets()[&OIL].stress;
    (stress.level, stress.auto, stress.auto_since)
}
