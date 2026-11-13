//! Fills at one price never realise a profit: generated sequences of adds, reductions, and flips by
//! one account, all at a constant mid in a market with no spread, under `V5`.

use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId, Side};
use proptest::arbitrary::any;
use proptest::collection::vec;
use proptest::prop_oneof;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

use crate::common::{Harness, acct, market};

/// A market with no size spread, so every fill is at the mid.
const FLAT: MarketId = 10;
const SEED: u64 = 0x5eed_0004;
const CASES: u32 = 256;

#[test]
fn fills_at_one_price_never_realise_a_profit() {
    // Prices from 0.1 to 1,000 and sizes from one unit of precision to 1,000, kept small enough that
    // 40 fills stay inside margin and the house caps.
    let price = 100_000_000i128..=1_000_000_000_000;
    let qty = prop_oneof![1i128..=1_000, 1i128..=1_000_000_000_000];
    let orders = vec((any::<bool>(), qty), 1..40);
    let config =
        Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() };
    let result = TestRunner::new(config).run(&(price, orders), |(price, orders)| {
        let mut h = Harness::v5();
        let spec = MarketSpec {
            spread_tiers: vec![SpreadTier::bps(None, 0)],
            ..market(FLAT, "FLAT", AssetClass::Index, Schedule::AlwaysOpen, 1_000)
        };
        h.admin(AdminCommand::UpsertMarket(spec));
        let mid = Fixed::from_raw(price).to_string();
        h.price(FLAT, &mid);
        let a = acct(1);
        h.deposit(a, "10000000");
        for (buy, qty) in orders {
            let side = if buy { Side::Buy } else { Side::Sell };
            let qty = Fixed::from_raw(qty).to_string();
            for event in h.market_order(a, FLAT, side, &qty, &mid) {
                if let Event::Fill(fill) = event
                    && (fill.price != fill.index || fill.realised_pnl.is_positive())
                {
                    return Err(TestCaseError::fail(format!("{side:?} {qty} at {mid}: {fill:?}")));
                }
            }
        }
        if h.balance(a) > Fixed::from_int(10_000_000) {
            return Err(TestCaseError::fail(format!("balance rose to {} at a constant price", h.balance(a))));
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
}
