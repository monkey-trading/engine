//! From `V52` every fill pays at least the exact size spread on the slice it opens and on the slice it
//! closes, in tenths of a bp, and no more than the rounding units: generated opens, partial closes and
//! reversals of one position in a market with random bands, at the normal and the stressed level.

use std::cell::Cell;

use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId, Side};
use proptest::arbitrary::any;
use proptest::collection::vec;
use proptest::prop_assert;
use proptest::strategy::Strategy;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

use crate::common::close_spread::{attest, ceilings, fund, install_expect, order, source_key};
use crate::common::{CHAIN_ID, Client, Harness, VAULT, market, params};

/// A market whose spread bands each case draws.
const BANDED: MarketId = 10;
const SEED: u64 = 0x5eed_0152;
const CASES: u32 = 256;
/// Raw units per whole unit.
const SCALE: i128 = 1_000_000_000;

#[test]
fn every_open_and_close_pays_at_least_the_exact_spread_from_v52() {
    // Mids from 0.001 to 10,000, and up to four trades of one unit of precision to 500 either way, so
    // the position grows, closes in part, and reverses through bands anywhere up to 10m, the house's
    // exposure staying under the level-1 utilisation trigger.
    let mid = 1_000_000i128..=10_000_000_000_000;
    let trades = vec((any::<bool>(), 1i128..=500_000_000_000), 1..=4);
    let edges = (1i128..=10_000_000_000_000_000, 1i128..=10_000_000_000_000_000);
    let rates = (1i64..=500, 1i64..=500, 1i64..=500);
    let cases = (mid, trades, edges, rates, any::<bool>());
    let (filled, closed) = (Cell::new(0u32), Cell::new(0u32));
    check(cases, |(mid, trades, edges, rates, stressed)| {
        let bounds = [edges.0.min(edges.1), edges.0.max(edges.1) + 1];
        let bands = [(Some(bounds[0]), rates.0), (Some(bounds[1]), rates.1), (None, rates.2)];
        let (mut h, client) = harness(&bands, usize::from(stressed));
        let mid_text = Fixed::from_raw(mid).to_string();
        h.price(BANDED, &mid_text);
        let stress = &params().stress_levels[usize::from(stressed)];
        let mut held = 0i128;
        for (buy, qty) in trades {
            let side = if buy { Side::Buy } else { Side::Sell };
            let events = order(&mut h, &client, BANDED, side, &Fixed::from_raw(qty).to_string(), &mid_text, false);
            let Some(fill) = events.iter().find_map(|e| match e {
                Event::Fill(fill) => Some(fill.clone()),
                _ => None,
            }) else {
                // A dust open whose size spread reaches the price is refused, and nothing is held.
                continue;
            };
            filled.set(filled.get() + 1);
            let delta = if buy { qty } else { -qty };
            // The slices in raw qty times raw mid: what grows |position|, and what closes it.
            let opposite = held != 0 && held.signum() != delta.signum();
            let closed_qty = if opposite { qty.min(held.abs()) } else { 0 };
            let grown = if opposite { (qty - closed_qty, 0) } else { (qty, held.abs()) };
            closed.set(closed.get() + u32::from(closed_qty > 0));
            let size = banded(&bands, grown.1 * mid, (grown.1 + grown.0) * mid)
                + banded(&bands, (held.abs() - closed_qty) * mid, held.abs() * mid);
            // Both sides in raw-squared units times 10^9: a bp of the notional is notional * 10^5.
            let paid = (fill.price - fill.index).abs().raw() * qty * SCALE;
            let owed = qty * mid * i128::from(stress.min_spread_bps) * 100_000
                + i128::from(stress.size_spread_multiplier_bps) * size;
            let slack = 4 * qty * SCALE + 8 * (bands.len() as i128 + 2) * SCALE * SCALE;
            prop_assert!(paid >= owed, "{side:?} {qty} at {mid} from {held}: paid {paid} < owed {owed}");
            prop_assert!(paid - owed < slack, "{side:?} {qty} at {mid} from {held}: paid {paid}, owed {owed}");
            held += delta;
        }
        Ok(())
    });
    // Most trades fill, and many close, so the checks above cannot pass vacuously.
    assert!(filled.get() > CASES, "only {} fills in {CASES} cases", filled.get());
    assert!(closed.get() > CASES / 2, "only {} closing fills in {CASES} cases", closed.get());
}

// ── Helpers ──

/// The signed `V8` genesis with the banded index market listed in tenths of a bp, every ceiling at
/// 9999bps so none clamps, access screening, `V52`, BANDED's stress floor at `level`, and a declared
/// client holding a billion.
fn harness(bands: &[(Option<i128>, i64); 3], level: usize) -> (Harness, Client) {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    let spread_tiers =
        bands.iter().map(|&(up_to, rate)| SpreadTier::tenths(up_to.map(Fixed::from_raw), rate)).collect();
    let spec = MarketSpec {
        spread_tiers,
        max_slippage_bps: 10_000,
        ..market(BANDED, "BANDED", AssetClass::Index, Schedule::AlwaysOpen, 1_000)
    };
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let mut entries: Vec<CloseSpreadCeiling> = ceilings();
    entries.iter_mut().for_each(|entry| entry.max_close_spread_bps = 9_999);
    entries.push(CloseSpreadCeiling { market: BANDED, max_close_spread_bps: 9_999 });
    install_expect(&mut h, entries);
    h.admin(AdminCommand::SetAccessPolicy(engine_core::access::AccessPolicy {
        source: source_key(),
        max_age_ms: 60_000,
    }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V52 });
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BANDED), level });
    let client = Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client, "1000000000");
    (h, client)
}

/// The exact charge for moving from notional `from` to `to`, each raw qty times raw mid, through
/// `bands`, in raw-squared units times tenths of a bp: each band's rate times the slice inside it.
fn banded(bands: &[(Option<i128>, i64); 3], from: i128, to: i128) -> i128 {
    let mut low = 0;
    bands
        .iter()
        .map(|&(up_to, rate)| {
            let high = up_to.map_or(i128::MAX, |up_to| up_to * SCALE);
            let slice = (to.min(high) - from.max(low)).max(0);
            low = high;
            i128::from(rate) * slice
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
