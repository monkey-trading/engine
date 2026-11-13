//! Spreads and margin never fall short of their exact amounts under `V10`: generated adds to one
//! position in a market with random spread and margin bands, at mids and sizes with nine decimals,
//! at the normal and the stressed level.

use std::cell::Cell;

use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::market::{MarketSpec, Tier};
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId, Side};
use proptest::arbitrary::any;
use proptest::collection::vec;
use proptest::prop_assert;
use proptest::strategy::Strategy;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

use crate::common::{Harness, acct, market, params};

/// A market whose spread and margin bands each case draws.
const BANDED: MarketId = 10;
const SEED: u64 = 0x5eed_0106;
const CASES: u32 = 512;
/// Raw units per whole unit.
const SCALE: i128 = 1_000_000_000;

#[test]
fn every_fill_pays_at_least_the_exact_spread_and_posts_at_least_the_exact_margin() {
    // Mids from 0.001 to 10,000 and adds from one unit of precision to 1,000, so three adds stay
    // inside the house caps; band edges anywhere up to 10m of notional.
    let mid = 1_000_000i128..=10_000_000_000_000;
    let adds = vec(1i128..=1_000_000_000_000, 1..=3);
    let edges = (1i128..=10_000_000_000_000_000, 1i128..=10_000_000_000_000_000);
    let spread_bps = (0i64..=50, 0i64..=50, 0i64..=50);
    let margin_bps = (1i64..=5_000, 1i64..=5_000, 1i64..=5_000);
    let cases = (mid, adds, edges, spread_bps, margin_bps, any::<bool>(), any::<bool>());
    let filled = Cell::new(0u32);
    check(cases, |(mid, adds, edges, spread_bps, margin_bps, buy, stressed)| {
        let bounds = [edges.0.min(edges.1), edges.0.max(edges.1) + 1];
        let spread = bands(bounds, [spread_bps.0, spread_bps.1, spread_bps.2]);
        let margin = bands(bounds, [margin_bps.0, margin_bps.1, margin_bps.2]);
        let mut h = Harness::v10();
        let spec = MarketSpec {
            spread_tiers: spread.iter().map(|tier| SpreadTier::bps(tier.up_to, tier.bps)).collect(),
            margin_tiers: margin.clone(),
            max_slippage_bps: 10_000,
            ..market(BANDED, "BANDED", AssetClass::Index, Schedule::AlwaysOpen, 1_000)
        };
        h.operator_admin(AdminCommand::UpsertMarket(spec));
        let level = usize::from(stressed);
        h.operator_admin(AdminCommand::SetStressFloor { market: Some(BANDED), level });
        let mid_text = Fixed::from_raw(mid).to_string();
        h.price(BANDED, &mid_text);
        let a = acct(1);
        h.deposit(a, "1000000000");
        let stress = &params().stress_levels[level];
        let side = if buy { Side::Buy } else { Side::Sell };
        let mut held = 0i128;
        for qty in adds {
            let events = h.market_order(a, BANDED, side, &Fixed::from_raw(qty).to_string(), &mid_text);
            let Some(fill) = events.iter().find_map(|e| match e {
                Event::Fill(fill) => Some(fill.clone()),
                _ => None,
            }) else {
                // A dust add whose size spread reaches the price is refused, and nothing is held.
                continue;
            };
            filled.set(filled.get() + 1);
            let paid = (fill.price - fill.index).abs().raw() * qty * 100_000_000;
            let owed = qty * mid * i128::from(stress.min_spread_bps) * 10_000
                + i128::from(stress.size_spread_multiplier_bps) * banded(&spread, held * mid, (held + qty) * mid);
            let slack = 2 * qty * 100_000_000 + 2 * (spread.len() as i128 + 1) * 100_000_000_000_000_000;
            prop_assert!(paid >= owed, "{side:?} {qty} at {mid} from {held}: paid {paid} < owed {owed}");
            prop_assert!(paid - owed < slack, "{side:?} {qty} at {mid} from {held}: paid {paid}, owed {owed}");

            held += qty;
            let posted = h.engine.equity_and_margin(&h.engine.accounts()[&a], None).1.raw() * 10_000_000_000_000;
            let required = banded(&margin, 0, held * mid);
            let slack = (margin.len() as i128 + 1) * 10_000_000_000_000;
            prop_assert!(posted >= required, "{held} at {mid}: posted {posted} < required {required}");
            prop_assert!(posted - required < slack, "{held} at {mid}: posted {posted}, required {required}");
        }
        Ok(())
    });
    // Most adds fill, so the checks above cannot pass vacuously.
    assert!(filled.get() > CASES, "only {} fills in {CASES} cases", filled.get());
}

// ── Helpers ──

/// Three bands: up to `bounds[0]` and `bounds[1]` of notional in raw units, then unbounded.
fn bands(bounds: [i128; 2], bps: [i64; 3]) -> Vec<Tier> {
    vec![
        Tier { up_to: Some(Fixed::from_raw(bounds[0])), bps: bps[0] },
        Tier { up_to: Some(Fixed::from_raw(bounds[1])), bps: bps[1] },
        Tier { up_to: None, bps: bps[2] },
    ]
}

/// The exact charge for moving from notional `from` to `to`, each given as raw qty times raw mid,
/// through `tiers`, in raw-squared units times bps: each band's bps times the slice inside it.
fn banded(tiers: &[Tier], from: i128, to: i128) -> i128 {
    let mut low = 0;
    tiers
        .iter()
        .map(|tier| {
            let high = tier.up_to.map_or(i128::MAX, |up_to| up_to.raw() * SCALE);
            let slice = (to.min(high) - from.max(low)).max(0);
            low = high;
            i128::from(tier.bps) * slice
        })
        .sum()
}

/// Runs `test` over `CASES` cases from `SEED`, so every run checks the same ones.
fn check<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let config =
        Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() };
    if let Err(failure) = TestRunner::new(config).run(&strategy, test) {
        panic!("{failure}");
    }
}
