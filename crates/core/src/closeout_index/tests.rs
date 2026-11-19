use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde_json::{Value, json};

use super::key::Key;
use super::valuation::{Assessment, LIMIT, Valuation, Valuations, position_value};
use super::{CloseoutIndex, KEYING_ALLOWANCE, Standing};
use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::market::{MarketSpec, QuoteCurrency, floored_margin};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

const BTC: MarketId = 1;
const WTI: MarketId = 2;
const EURUSD: MarketId = 4;
const USDJPY: MarketId = 5;
const DE40: MarketId = 6;
const JP225: MarketId = 7;
/// Each market's price around which a case draws, and the FX market it converts at.
const MARKETS: [(MarketId, i64, Option<MarketId>); 6] = [
    (BTC, 50_000, None),
    (WTI, 70, None),
    (EURUSD, 1, None),
    (USDJPY, 150, Some(USDJPY)),
    (DE40, 18_000, Some(EURUSD)),
    (JP225, 38_000, Some(USDJPY)),
];
const SEED: u64 = 0x5eed_0912;
const CURRENCIES: [QuoteCurrency; 3] =
    [QuoteCurrency::Usd, QuoteCurrency::Eur { fx_market: EURUSD }, QuoteCurrency::Jpy { fx_market: USDJPY }];

#[test]
fn the_checked_valuation_is_the_checks_own_wherever_it_values() {
    let raw = prop_oneof![9 => -(1i128 << 50)..(1i128 << 50), 1 => any::<i128>()];
    let cases = (raw.clone(), raw.clone(), raw.clone(), raw, 0..3usize, 0..4usize);
    let valued = Cell::new(0u32);
    run(&cases, |(qty, entry, price, fx, currency, form)| {
        let pos = Position { qty: Fixed::from_raw(qty), entry: Fixed::from_raw(entry) };
        let (version, charge) = form_of(form);
        let currency = CURRENCIES[currency];
        let (price, fx) = (Fixed::from_raw(price), Some(Fixed::from_raw(fx)));
        let Some(checked) = position_value(version, charge, currency, &pos, price, fx) else { return Ok(()) };
        valued.set(valued.get() + 1);
        let notional = currency.to_usd(pos.qty.abs().mul_rounded(price, charge), fx, Rounding::Up);
        prop_assert_eq!(checked, (currency.to_usd(pos.unrealised(price, version), fx, Rounding::Down), notional));
        Ok(())
    });
    assert!(valued.get() >= 200, "only {} cases valued", valued.get());
}

#[test]
fn the_checked_valuation_fails_where_the_check_panics() {
    let (version, charge) = form_of(3);
    let pos = Position { qty: Fixed::ONE, entry: Fixed::ONE };
    let (euro, yen) = (CURRENCIES[1], CURRENCIES[2]);
    // No FX index for a non-zero amount, a zero yen index, and a notional past `Fixed`'s range.
    assert_eq!(position_value(version, charge, euro, &pos, Fixed::from_int(2), None), None);
    assert_eq!(position_value(version, charge, yen, &pos, Fixed::from_int(2), Some(Fixed::ZERO)), None);
    let huge = Position { qty: Fixed::from_raw(i128::MAX / 2), entry: Fixed::ONE };
    assert_eq!(position_value(version, charge, CURRENCIES[0], &huge, Fixed::from_int(3), None), None);
    let lowest = Position { qty: Fixed::from_raw(i128::MIN), entry: Fixed::ONE };
    assert_eq!(position_value(version, charge, CURRENCIES[0], &lowest, Fixed::ONE, None), None);
}

#[test]
fn the_corners_bound_the_valuation_at_every_point_inside() {
    let cases = (0..3usize, 0..4usize, -1_000_000i64..1_000_000, 1..200_000i64, (1..200_000i64, 1..200_000i64));
    let points = proptest::collection::vec((0..=1_000u64, 0..=1_000u64), 8);
    run(&(cases, (1..300i64, 1..300i64), points), |((currency, form, qty, entry, prices), fxs, points)| {
        let (version, charge) = form_of(form);
        let currency = CURRENCIES[currency];
        let pos = Position { qty: Fixed::from_raw(i128::from(qty) * 1_000_003), entry: cents(entry) };
        let (low, high) = (cents(prices.0.min(prices.1)), cents(prices.0.max(prices.1)));
        let (fx_low, fx_high) = (cents(fxs.0.min(fxs.1)), cents(fxs.0.max(fxs.1)));
        let tiers = serde_json::from_value::<MarketSpec>(listing(BTC, "BTC-USD", None, 0)).unwrap().margin_tiers;
        let corners: Vec<(Fixed, Fixed)> = [low, high]
            .into_iter()
            .flat_map(|at| [fx_low, fx_high].map(|fx| position_value(version, charge, currency, &pos, at, Some(fx))))
            .map(|value| value.expect("in range"))
            .collect();
        let worst = corners.iter().map(|(pnl, _)| *pnl).min().unwrap();
        let best = corners.iter().map(|(pnl, _)| *pnl).max().unwrap();
        let most = corners.iter().map(|(_, notional)| *notional).max().unwrap();
        for (at, fx) in points {
            let (at, fx) = (between(low, high, at), between(fx_low, fx_high, fx));
            let (pnl, notional) = position_value(version, charge, currency, &pos, at, Some(fx)).expect("in range");
            prop_assert!(worst <= pnl && pnl <= best, "{pnl} outside {worst}..{best} at {at} {fx}");
            prop_assert!(notional <= most);
            for floor in [0, 1_500] {
                prop_assert!(
                    floored_margin(&tiers, notional, floor, charge) <= floored_margin(&tiers, most, floor, charge)
                );
            }
        }
        Ok(())
    });
}

#[test]
fn a_key_shows_its_account_at_or_above_its_level_everywhere_inside_it() {
    let keyed = Cell::new(0u32);
    run(
        &(book(), 0..4usize, proptest::collection::vec(any::<u64>(), 8)),
        |((engine, acct, valuations, level_bps), form, draws)| {
            let mut engine = engine;
            engine.protocol_version = form_of(form).0;
            let Some(key) = engine.key_account(&acct, &valuations, level_bps) else { return Ok(()) };
            keyed.set(keyed.get() + 1);
            let charge = engine.charge_rounding();
            for draw in draws {
                let point = inside(&key, draw);
                let (equity, margin) = engine.equity_and_margin_at(&acct, |id, _, _| point[&id]);
                prop_assert!(
                    margin.is_zero() || equity >= margin.mul_bps_rounded(level_bps, charge),
                    "below at {point:?}"
                );
                if !key.weekend_weak() {
                    let weekend: Fixed = acct
                        .positions
                        .iter()
                        .map(|(id, pos)| {
                            let spec = &engine.markets[id].spec;
                            let (at, fx) = point[id];
                            let (_, notional) =
                                position_value(engine.protocol_version, charge, spec.quote_currency, pos, at, fx)
                                    .unwrap();
                            spec.floored_margin_for(notional, spec.weekend_margin_bps, charge)
                        })
                        .sum();
                    prop_assert!(equity >= weekend, "short of every weekend margin at {point:?}");
                }
            }
            Ok(())
        },
    );
    assert!(keyed.get() >= 64, "only {} cases keyed their account", keyed.get());
}

#[test]
fn a_moved_balance_level_or_band_gives_the_headroom_a_fresh_bound_gives() {
    run(&(book(), 1..20_000i64, 1..10_000i64), |((engine, acct, valuations, level_bps), credit, level)| {
        let mut engine = engine;
        let charge = engine.charge_rounding();
        let Some(mut key) = engine.key_account(&acct, &valuations, level_bps) else { return Ok(()) };
        if key.rebalance(acct.balance + Fixed::from_int(credit), level_bps, charge).is_some() {
            prop_assert_eq!(engine.rebound(&key, level_bps), Some(key.headroom));
        }
        if key.settle(level, charge).is_some() {
            prop_assert_eq!(engine.rebound(&key, level), Some(key.headroom));
        }
        let id = key.positions[0].market;
        let old = super::markets::MarketRules::of(&engine.markets[&id].spec);
        let mut spec = engine.markets[&id].spec.clone();
        spec.margin_tiers = vec![crate::market::Tier { up_to: None, bps: 250 }];
        spec.weekend_margin_bps = 2_000;
        let new = super::markets::MarketRules::of(&spec);
        engine.markets.insert(id, MarketState::new(spec));
        if key.reband(id, (&old, &new), level, charge).is_some() {
            prop_assert_eq!(engine.rebound(&key, level), Some(key.headroom));
        } else {
            prop_assert_eq!(engine.rebound(&key, level), None);
        }
        Ok(())
    });
}

#[test]
fn a_command_keys_no_more_than_its_allowance_across_its_refreshes() {
    // A thousand accounts of three positions each, every one far above its level, at the marks.
    let mut engine = Engine::new();
    engine.protocol_version = ProtocolVersion::LATEST;
    for (id, price, _) in MARKETS.iter().take(3) {
        let spec: MarketSpec = serde_json::from_value(listing(*id, "M", None, 0)).unwrap();
        let mut market = MarketState::new(spec);
        market.mark = Some(Fixed::from_int(*price));
        engine.markets.insert(*id, market);
    }
    let accounts: Vec<AccountId> = (0..1_000u32).map(account).collect();
    let hold = |engine: &mut Engine, entry_bp: i64| {
        for id in &accounts {
            let mut acct = Account { balance: Fixed::from_int(1_000_000), ..Account::default() };
            for (market, price, _) in MARKETS.iter().take(3) {
                let entry = Fixed::from_int(*price).mul_bps(10_000 + entry_bp);
                acct.positions.insert(*market, Position { qty: Fixed::ONE, entry });
            }
            engine.accounts.insert(*id, acct);
        }
    };
    hold(&mut engine, 0);
    let mut index = CloseoutIndex::default();
    index.build(&engine, 5_000);
    let keyed = |index: &CloseoutIndex| index.accounts.values().filter(|s| matches!(s, Standing::Keyed(_))).count();
    assert_eq!(keyed(&index), accounts.len(), "the build keys every account, with no allowance");
    // New entries unkey every account; two refreshes of one command key one allowance between them.
    engine.accounts.take_watched();
    hold(&mut engine, 1);
    engine.last_seq = Some(7);
    let changed = engine.accounts.take_watched();
    index.refresh(&engine, 5_000, changed);
    index.refresh(&engine, 5_000, BTreeSet::new());
    assert_eq!(keyed(&index) * 3, KEYING_ALLOWANCE - KEYING_ALLOWANCE % 3, "one command keys one allowance");
    assert_eq!(index.queue.len(), index.queued.len());
    // The next commands key the rest, and leave the queue empty.
    for seq in 8..10 {
        engine.last_seq = Some(seq);
        index.refresh(&engine, 5_000, BTreeSet::new());
    }
    assert_eq!(keyed(&index), accounts.len());
    assert!(index.queue.is_empty() && index.queued.is_empty());
}

#[test]
fn a_weekend_gap_values_a_keyed_holder_only_when_its_marks_leave_its_ranges() {
    let mut engine = Engine::new();
    engine.protocol_version = ProtocolVersion::LATEST;
    for (id, price, weekend) in [(BTC, 50_000, 0), (WTI, 70, 1_500)] {
        let spec: MarketSpec = serde_json::from_value(listing(id, "M", None, weekend)).unwrap();
        let mut market = MarketState::new(spec);
        market.mark = Some(Fixed::from_int(price));
        engine.markets.insert(id, market);
    }
    // One BTC-USD unit and 10 WTI at their marks, with cash well clear of the gap's requirement, so only
    // a crossing can name the account. A long's range reaches up to twice its price.
    let holder = account(1);
    let mut acct = Account { balance: Fixed::from_int(1_000_000), ..Account::default() };
    acct.positions.insert(BTC, Position { qty: Fixed::ONE, entry: Fixed::from_int(50_000) });
    acct.positions.insert(WTI, Position { qty: Fixed::from_int(10), entry: Fixed::from_int(70) });
    engine.accounts.insert(holder, acct);
    let mut index = CloseoutIndex::default();
    index.build(&engine, 5_000);
    assert!(matches!(index.accounts.get(&holder), Some(Standing::Keyed(_))), "the account is keyed");
    let closing = BTreeSet::from([WTI]);
    assert_eq!(index.weekend_candidates(&engine, &closing), Vec::<AccountId>::new(), "marks at the keys' point");
    // BTC-USD's mark lags to inside its range, then to outside it.
    engine.markets.get_mut(&BTC).unwrap().mark = Some(Fixed::from_int(90_000));
    assert_eq!(index.weekend_candidates(&engine, &closing), Vec::<AccountId>::new(), "marks inside the ranges");
    engine.markets.get_mut(&BTC).unwrap().mark = Some(Fixed::from_int(120_000));
    assert_eq!(index.weekend_candidates(&engine, &closing), vec![holder], "marks outside the ranges");
    // It holds no market in `closing`, so a gap in another market does not name it.
    let other = BTreeSet::from([EURUSD]);
    assert_eq!(index.weekend_candidates(&engine, &other), Vec::<AccountId>::new());
}

#[test]
fn a_new_grid_or_grouped_ceiling_reassesses_every_unkeyed_account() {
    // From `V81`, a hedged pair of BTC-USD long and WTI short as one group at a 1% floor share, valued at
    // their marks: between the close-out levels of its portfolio and per-position requirements, so
    // unkeyed and at or above its level.
    let mut engine = Engine::new();
    engine.protocol_version = ProtocolVersion::V81;
    let member = |market, loading: Fixed| crate::portfolio::GroupMember {
        market,
        loading,
        residual_shock_bps: 20,
        max_residual_shock_bps: 20,
    };
    let group = crate::portfolio::OffsetGroup {
        name: "pair".into(),
        floor_share_bps: 100,
        max_floor_share_bps: 10_000,
        members: vec![member(BTC, Fixed::ONE), member(WTI, Fixed::ONE)],
    };
    let grid = crate::portfolio::PortfolioGrid { groups: vec![group] };
    engine.params = Some(crate::params::ProtocolParams { portfolio_grid: grid, ..crate::params::tests::params() });
    for (id, price) in [(BTC, 50_000), (WTI, 70)] {
        let spec: MarketSpec = serde_json::from_value(listing(id, "M", None, 0)).unwrap();
        let mut market = MarketState::new(spec);
        market.mark = Some(Fixed::from_int(price));
        engine.markets.insert(id, market);
    }
    engine.close_spread_ceilings = Some(BTreeMap::from([(BTC, 20), (WTI, 20)]));
    // $500k each way: $20k of bands, matched whole, less a grid charge of 0.2% for the move and 0.4% for
    // closing at a 20bp ceiling at the 50% level, a $14k credit: $6k required. $8k of cash is above half
    // of that and below half the bands.
    let holder = account(1);
    let mut acct = Account { balance: Fixed::from_int(8_000), ..Account::default() };
    acct.positions.insert(BTC, Position { qty: Fixed::from_int(10), entry: Fixed::from_int(50_000) });
    acct.positions.insert(WTI, Position { qty: Fixed::from_int(-500_000).div_int(70), entry: Fixed::from_int(70) });
    engine.accounts.insert(holder, acct);
    let below = |index: &CloseoutIndex| match index.accounts.get(&holder) {
        Some(Standing::Unkeyed { below, .. }) => Some(*below),
        _ => None,
    };
    let mut index = CloseoutIndex::default();
    index.build(&engine, 5_000);
    assert_eq!(below(&index), Some(false), "unkeyed, at or above its level");
    // A floor share raised alone, with no price, level, band or balance moved, puts it below.
    engine.params.as_mut().unwrap().portfolio_grid.groups[0].floor_share_bps = 10_000;
    index.refresh(&engine, 5_000, BTreeSet::new());
    assert_eq!(below(&index), Some(true), "a new grid re-assesses it");
    engine.params.as_mut().unwrap().portfolio_grid.groups[0].floor_share_bps = 100;
    index.refresh(&engine, 5_000, BTreeSet::new());
    assert_eq!(below(&index), Some(false));
    // So does a grouped market's ceiling raised alone: closing costs take the credit.
    engine.close_spread_ceilings = Some(BTreeMap::from([(BTC, 20), (WTI, 5_000)]));
    index.refresh(&engine, 5_000, BTreeSet::new());
    assert_eq!(below(&index), Some(true), "a new ceiling re-assesses it");
}

#[test]
fn no_key_is_made_or_kept_where_a_point_inside_could_overflow_the_check() {
    let mut runner = TestRunner::new(Config { cases: 64, rng_seed: RngSeed::Fixed(SEED), ..Config::default() });
    let (keyed, overflowed) = (Cell::new(0u32), Cell::new(0u32));
    runner
        .run(&book(), |(engine, acct, valuations, level_bps)| {
            let charge = engine.charge_rounding();
            let Some(mut key) = engine.key_account(&acct, &valuations, level_bps) else { return Ok(()) };
            keyed.set(keyed.get() + 1);
            // A balance whose sum with the key's PnL could leave a quarter of `Fixed`'s range.
            let near_limit = Fixed::from_raw(LIMIT.raw() - 1);
            prop_assert!(key.rebalance(near_limit, level_bps, charge).is_none(), "kept a key that could overflow");
            let rich = Account { balance: near_limit, ..acct.clone() };
            prop_assert!(
                engine.key_account(&rich, &valuations, level_bps).is_none(),
                "keyed an account that could overflow"
            );
            // And an assessment whose equity fold overflows, in market order as the check folds it,
            // names the account for `close_out`.
            let overflowing = Account { balance: Fixed::from_raw(i128::MAX - 1), ..acct };
            let folded = overflowing.positions.iter().try_fold(overflowing.balance.raw(), |equity, (id, pos)| {
                let spec = &engine.markets[id].spec;
                let (price, fx) = (valuations[id].price.unwrap(), valuations[id].fx);
                let (pnl, _) =
                    position_value(engine.protocol_version, charge, spec.quote_currency, pos, price, fx).unwrap();
                equity.checked_add(pnl.raw())
            });
            if folded.is_none() {
                overflowed.set(overflowed.get() + 1);
                prop_assert_eq!(engine.assess(&overflowing, &valuations, level_bps), Assessment::Below);
            }
            Ok(())
        })
        .unwrap();
    assert!(keyed.get() >= 16, "only {} cases keyed their account", keyed.get());
    assert!(overflowed.get() >= 4, "only {} cases overflowed their equity", overflowed.get());
}

// ── Helpers ──

/// The `n`th account.
fn account(n: u32) -> AccountId {
    let mut bytes = [0x5a; 20];
    bytes[16..].copy_from_slice(&n.to_be_bytes());
    AccountId(bytes)
}

/// Runs `test` over `strategy` from the fixed seed.
fn run<S: Strategy>(strategy: &S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let config = Config { cases: 512, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() };
    TestRunner::new(config).run(strategy, test).unwrap();
}

/// The valuation and rounding forms: truncation before `V5`, rounding down from it, and the charge
/// rounding either side of `V10`.
fn form_of(form: usize) -> (ProtocolVersion, Rounding) {
    let version = [ProtocolVersion::V4, ProtocolVersion::V6, ProtocolVersion::V12, ProtocolVersion::LATEST][form];
    (version, if version >= ProtocolVersion::V10 { Rounding::Up } else { Rounding::TowardZero })
}

/// `n` hundredths.
fn cents(n: i64) -> Fixed {
    Fixed::from_raw(i128::from(n) * 10_000_000)
}

/// The point `thousandths` of the way from `low` to `high`.
fn between(low: Fixed, high: Fixed, thousandths: u64) -> Fixed {
    low + (high - low).mul_div(Fixed::from_raw(i128::from(thousandths)), Fixed::from_raw(1_000), Rounding::Down)
}

/// A point inside `key`'s ranges, each coordinate drawn from `draw`.
fn inside(key: &Key, draw: u64) -> BTreeMap<MarketId, (Fixed, Option<Fixed>)> {
    let mut draw = draw;
    let mut next = || {
        draw = draw.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (draw >> 33) % 1_001
    };
    key.positions
        .iter()
        .map(|keyed| {
            let at = between(keyed.price.low, keyed.price.high, next());
            (keyed.market, (at, keyed.fx.map(|fx| between(fx.low, fx.high, next()))))
        })
        .collect()
}

/// A listing of `id` with two margin bands, quoted in USD or at `fx`, with a weekend margin.
fn listing(id: MarketId, symbol: &str, fx: Option<Value>, weekend_bps: i64) -> Value {
    let mut spec = json!({
        "id": id, "symbol": symbol, "asset_class": "index", "schedule": { "kind": "always_open" },
        "margin_tiers": [{ "up_to": "1000000", "bps": 200 }, { "bps": 500 }], "spread_tiers": [{ "bps": 0 }],
        "price_rules": { "max_age_ms": 1000, "max_divergence_bps": 50, "min_sources": 2 },
        "max_slippage_bps": 20, "house_cap_bps": 5000, "weekend_margin_bps": weekend_bps,
        "financing": { "benchmark": "SOFR", "markup_bps": 0, "day_basis": 365 },
    });
    if let Some(fx) = fx {
        spec["quote_currency"] = fx;
    }
    spec
}

/// An engine listing `MARKETS`, an account holding some of them, their valuations around their
/// prices, and a close-out level.
fn book() -> impl Strategy<Value = (Engine, Account, Valuations, i64)> {
    let position = (any::<bool>(), 1..2_000_000i64, -2_000..2_000i64, -500..500i64, -300..300i64);
    let held = proptest::collection::btree_map(0..MARKETS.len(), position, 1..=MARKETS.len());
    (held, 1..50i64, prop_oneof![Just(5_000i64), Just(8_000), Just(10_000)]).prop_map(|(held, cover, level_bps)| {
        let mut engine = Engine::new();
        engine.protocol_version = ProtocolVersion::LATEST;
        for (id, _, fx) in MARKETS {
            let currency = fx.map(|fx| match id {
                DE40 => json!({ "currency": "eur", "fx_market": fx }),
                _ => json!({ "currency": "jpy", "fx_market": fx }),
            });
            let symbol = match id {
                EURUSD => "EUR-USD",
                USDJPY => "USD-JPY",
                _ => "M",
            };
            let weekend = if matches!(id, WTI | JP225) { 1_500 } else { 0 };
            let spec: MarketSpec = serde_json::from_value(listing(id, symbol, currency, weekend)).unwrap();
            engine.markets.insert(id, MarketState::new(spec));
        }
        let mut valuations: Valuations = BTreeMap::new();
        let mut acct = Account::default();
        let (mut notional, mut pnl) = (Fixed::ZERO, Fixed::ZERO);
        for (slot, (long, usd, entry_bp, price_bp, fx_bp)) in held {
            let (id, price, fx_market) = MARKETS[slot];
            let price = Fixed::from_int(price).mul_bps(10_000 + price_bp);
            let fx = fx_market.map(|fx| {
                let (_, base, _) = MARKETS.iter().find(|(m, ..)| *m == fx).copied().unwrap();
                if fx == id { price } else { Fixed::from_int(base).mul_bps(10_000 + fx_bp) }
            });
            let currency = engine.markets[&id].spec.quote_currency;
            let units = currency.from_usd(Fixed::from_int(usd), fx, Rounding::Down).div(price);
            let qty = if long { units } else { -units };
            let pos = Position { qty, entry: price.mul_bps(10_000 + entry_bp) };
            let (gain, held) =
                position_value(ProtocolVersion::LATEST, Rounding::Up, currency, &pos, price, fx).unwrap();
            (notional, pnl) = (notional + held, pnl + gain);
            acct.positions.insert(id, pos);
            valuations.insert(id, Valuation::of(currency, Some(price), fx));
        }
        // Cash covering the loss so far and from a fiftieth to the whole of the notional.
        acct.balance = -pnl + notional.mul_bps(cover * 200);
        (engine, acct, valuations, level_bps)
    })
}
