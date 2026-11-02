//! An account's close-out price against the engine: moving the market with `Engine::apply` to just
//! short of the price closes nothing out, and to just past it closes the account out.

use super::*;
use crate::stats::closeout::{PositionCloseout, ReachKind, account_closeouts};
use crate::test_support::dev_config;

const EUR_USD: MarketId = 10;
const ETH: MarketId = 2;
const DE40: MarketId = 23;
const USD_JPY: MarketId = 12;

/// How far from the price the engine's own threshold may lie, as a share of the mark: one part in a
/// million, which is the walk's resolution.
const TOLERANCE: Fixed = Fixed::from_raw(1_000);

/// The close-out of `account`'s position in `market`, as the account read serves it.
fn closeout_of(rig: &Rig, account: AccountId, market: MarketId) -> PositionCloseout {
    let acct = &rig.engine.accounts()[&account];
    let standing = rig.engine.equity_and_margin(acct, None);
    account_closeouts(&rig.engine, acct, standing).into_iter().find(|c| c.market == market).expect("a position")
}

fn price_of(rig: &Rig, market: MarketId) -> Fixed {
    let found = closeout_of(rig, TRADER, market);
    assert_eq!(found.reach, ReachKind::At, "{found:?}");
    found.price.expect("an `at` close-out has a price")
}

fn mark(rig: &Rig, market: MarketId) -> Fixed {
    rig.engine.markets()[&market].mark.expect("a priced market")
}

/// Moves `market` from its mark to `target` in steps of 0.2% of the mark, two seconds apart, keeping
/// each `(market, mid)` of `keep` quoted so conversions stay priced, and says whether an account was
/// closed out on the way.
fn closes_out(mut rig: Rig, market: MarketId, target: Fixed, keep: &[(MarketId, Fixed)], from: Millis) -> bool {
    let mut price = mark(&rig, market);
    let step = price.mul_bps(20);
    let mut at = from;
    while price != target {
        price += if target > price { (target - price).min(step) } else { -(price - target).min(step) };
        at += 2_000;
        let mut events = Vec::new();
        for (fx, mid) in keep {
            events.extend(rig.price_in(*fx, at, *mid));
        }
        events.extend(rig.price_in(market, at, price));
        if events.iter().any(|event| matches!(event, Event::CloseOut { .. })) {
            return true;
        }
    }
    assert_eq!(mark(&rig, market), target, "the walk priced the market at its target");
    false
}

/// A position under test: how to build its engine, and what the walk keeps quoted.
struct Case {
    build: Box<dyn Fn() -> Rig>,
    market: MarketId,
    keep: Vec<(MarketId, Fixed)>,
    from: Millis,
}

impl Case {
    fn new(market: MarketId, build: impl Fn() -> Rig + 'static) -> Self {
        Self { build: Box::new(build), market, keep: Vec::new(), from: T0 + 10_000 }
    }

    fn closes_at(&self, target: Fixed) -> bool {
        closes_out((self.build)(), self.market, target, &self.keep, self.from)
    }

    /// The estimate, once the price where the engine closes the account out, found by halving a window
    /// of 0.5% of the price around it, is within the tolerance of it.
    fn agrees(&self) -> Fixed {
        self.agrees_within(TOLERANCE)
    }

    fn agrees_within(&self, tolerance: Fixed) -> Fixed {
        let rig = (self.build)();
        let estimate = price_of(&rig, self.market);
        let mark = mark(&rig, self.market);
        let long = estimate < mark;
        let window = estimate.mul_bps(50);
        let (mut safe, mut gone) =
            if long { (estimate + window, estimate - window) } else { (estimate - window, estimate + window) };
        assert!(!self.closes_at(safe), "no close-out at {safe}, short of the estimate {estimate}");
        assert!(self.closes_at(gone), "a close-out at {gone}, past the estimate {estimate}");
        for _ in 0..14 {
            let mid = Fixed::from_raw((safe.raw() + gone.raw()) / 2);
            if self.closes_at(mid) { gone = mid } else { safe = mid }
        }
        let threshold = Fixed::from_raw((safe.raw() + gone.raw()) / 2);
        let off = (estimate - threshold).abs();
        assert!(off <= mark.mul(tolerance), "the estimate {estimate} is {off} from the engine's {threshold}");
        estimate
    }
}

fn with_balance(mut rig: Rig, balance: i64) -> Rig {
    let mut state = serde_json::to_value(&rig.engine).unwrap();
    state["accounts"][TRADER.to_string()]["balance"] = serde_json::json!(Fixed::from_int(balance));
    rig.engine = serde_json::from_value(state).unwrap();
    rig
}

fn deposit(rig: &mut Rig, amount: i64, reference: &str) {
    let amount = Fixed::from_int(amount);
    rig.apply(T0, Command::Deposit { account: TRADER, amount, chain_ref: reference.into() });
}

fn btc(side: Side, qty: i64, extra: i64) -> Rig {
    let mut rig = Rig::new();
    if extra > 0 {
        deposit(&mut rig, extra, "extra");
    }
    rig.trade(T0 + 1_000, TRADER, side, qty);
    rig
}

fn quote(rig: &mut Rig, market: MarketId, mid: &str, ts: Millis) {
    rig.price_in(market, ts, mid.parse().unwrap());
}

/// One DE40 contract, quoted in euros, on `balance` of equity.
fn de40(balance: i64) -> Rig {
    let mut rig = Rig::new();
    quote(&mut rig, EUR_USD, "1.08", T0 + 500);
    quote(&mut rig, DE40, "23500", T0 + 500);
    let events = rig.trade_in(DE40, T0 + 600, TRADER, Side::Buy, 1);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    with_balance(rig, balance)
}

fn usd_jpy(side: Side) -> Rig {
    let production = include_str!("../../../../../config/production-genesis.toml");
    let genesis: toml::Table = toml::from_str(production).unwrap();
    let listing = genesis["genesis"]["markets"].as_array().unwrap().iter().find(|m| m["id"].as_integer() == Some(12));
    let spec: engine_core::market::MarketSpec = listing.unwrap().clone().try_into().unwrap();
    let mut rig = Rig::new();
    let operator = dev_config().operator().unwrap();
    let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).unwrap();
    let nonce = rig.engine.next_request_nonce(operator.address);
    let listed = crate::admin::sign(&key, &operator, AdminCommand::UpsertMarket(spec), nonce, T0 + 60_000);
    let events = rig.apply(T0 + 100, Command::SignedAdmin(Box::new(listed)));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    rig.price_in(USD_JPY, T0 + 200, Fixed::from_int(150));
    let events = rig.trade_in(USD_JPY, T0 + 300, TRADER, side, 100_000);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    with_balance(rig, 1_500)
}

/// A long and a short of 5 BTC, on 100000 of equity behind each.
#[test]
fn a_usd_long_and_short_are_priced_where_the_engine_closes_them_out() {
    let price = Case::new(1, || btc(Side::Buy, 5, 0)).agrees();
    assert!(price < Fixed::from_int(100_000), "{price}");
    let price = Case::new(1, || btc(Side::Sell, 5, 0)).agrees();
    assert!(price > Fixed::from_int(100_000), "{price}");
}

/// 20 BTC is 2M of notional with a 10% band to a million and 20% above; the account closes out only
/// once the fall takes it under a million.
#[test]
fn a_close_out_across_a_margin_band_is_priced_where_the_engine_closes_it_out() {
    let price = Case::new(1, || btc(Side::Buy, 20, 1_400_000)).agrees();
    assert!(price < Fixed::from_int(60_000), "the fall crosses the band at 50000: {price}");
}

#[test]
fn a_market_quoted_in_euros_is_priced_where_the_engine_closes_it_out() {
    let mut case = Case::new(DE40, || de40(1_800));
    case.keep = vec![(EUR_USD, "1.08".parse().unwrap())];
    let price = case.agrees();
    assert!(price < Fixed::from_int(23_500) && price > Fixed::from_int(20_000), "{price}");
}

#[test]
fn usd_jpy_is_priced_where_the_engine_closes_it_out() {
    let price = Case::new(USD_JPY, || usd_jpy(Side::Buy)).agrees();
    assert!(price < Fixed::from_int(150), "{price}");
    let price = Case::new(USD_JPY, || usd_jpy(Side::Sell)).agrees();
    assert!(price > Fixed::from_int(150), "{price}");
}

/// A long in BTC and a long in ETH share the account's margin: each is priced with the other held at
/// its mark, and opening the second moves the first's price.
#[test]
fn an_account_with_several_positions_prices_each_with_the_others_at_their_marks() {
    let alone = btc(Side::Buy, 2, 0);
    let alone_price = price_of(&alone, 1);
    let both = || {
        let mut rig = btc(Side::Buy, 2, 0);
        quote(&mut rig, ETH, "4000", T0 + 2_000);
        let events = rig.trade_in(ETH, T0 + 2_100, TRADER, Side::Buy, 40);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
        rig
    };
    let rig = both();
    assert!(
        price_of(&rig, 1) > alone_price,
        "an ETH long uses equity BTC needs: {alone_price} to {}",
        price_of(&rig, 1)
    );
    Case::new(1, both).agrees();
    Case::new(ETH, both).agrees();
}

/// A short in another market adds margin the account must carry, as a long does, so it brings the
/// price of a BTC long toward its mark.
#[test]
fn a_short_in_another_market_is_priced_with_its_margin_where_the_engine_closes_the_account_out() {
    let alone = price_of(&btc(Side::Buy, 4, 0), 1);
    let hedged = || {
        let mut rig = btc(Side::Buy, 4, 0);
        quote(&mut rig, ETH, "4000", T0 + 2_000);
        let events = rig.trade_in(ETH, T0 + 2_100, TRADER, Side::Sell, 20);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
        rig
    };
    assert!(price_of(&hedged(), 1) > alone, "the ETH short's margin leaves BTC less room: {alone}");
    Case::new(1, hedged).agrees();
    Case::new(ETH, hedged).agrees();
}

/// 8 BTC short is 800k of notional, in the 10% band; with 500k of equity it closes out near 1.2M of
/// notional, past the million where the rate is 20%, so the walk up from the mark crosses the band.
#[test]
fn a_short_that_crosses_a_margin_band_is_priced_where_the_engine_closes_it_out() {
    let price = Case::new(1, || btc(Side::Sell, 8, 400_000)).agrees();
    assert!(price > Fixed::from_int(125_000), "past the million of notional, at 125000: {price}");
}

/// With the DE40 closed, a close-out cannot trade it and counts it at its last mark, as the estimate
/// for the BTC long beside it does.
#[test]
fn a_position_beside_a_market_outside_its_session_is_priced_with_that_market_at_its_mark() {
    let after_close = T0 + 6 * 3_600_000 + 30 * 60_000;
    let build = move || {
        let mut rig = de40(100_000);
        rig.trade(T0 + 1_000, TRADER, Side::Buy, 3);
        rig.price(after_close, 100_000);
        assert!(!rig.engine.markets()[&DE40].spec.schedule.is_open(rig.engine.now()), "the DE40 is closed");
        rig
    };
    let mut case = Case::new(1, build);
    case.from = after_close;
    case.agrees();
}

#[test]
fn an_account_below_the_level_at_the_marks_is_due_and_closed_out_by_the_next_price() {
    let rig = with_balance(btc(Side::Buy, 5, 0), 20_000);
    let found = closeout_of(&rig, TRADER, 1);
    assert_eq!((found.reach, found.price), (ReachKind::Due, None));
    let mut rig = rig;
    let events = rig.price(T0 + 3_000, 100_000);
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    // The same account on enough equity is above the level, and the same price closes nothing out.
    let mut rig = with_balance(btc(Side::Buy, 5, 0), 100_000);
    assert_eq!(closeout_of(&rig, TRADER, 1).reach, ReachKind::At);
    let events = rig.price(T0 + 3_000, 100_000);
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
}

/// A long loses at most its notional, so one the equity covers has no price above zero, and a short
/// has none from ten times its mark when the equity covers the loss there.
#[test]
fn a_position_the_range_cannot_close_out_has_no_price() {
    let long = btc(Side::Buy, 1, 0);
    let found = closeout_of(&long, TRADER, 1);
    assert_eq!((found.reach, found.price), (ReachKind::Beyond, None));
    assert!(!closes_out(btc(Side::Buy, 1, 0), 1, Fixed::from_int(1_000), &[], T0 + 10_000));
    let short = btc(Side::Sell, 1, 10_000_000);
    let found = closeout_of(&short, TRADER, 1);
    assert_eq!((found.reach, found.price), (ReachKind::Beyond, None));
    assert!(!closes_out(btc(Side::Sell, 1, 10_000_000), 1, Fixed::from_int(200_000), &[], T0 + 10_000));
}

/// A 1 BTC short at 100000 stands 900000 of loss from ten times its mark, with 100000 of margin there
/// at 10% and half of it the level: 950000 of equity is the tie. Under it the price is below ten marks,
/// over it there is none, so the range ends at ten marks and no sooner or later.
#[test]
fn a_short_is_priced_up_to_ten_times_its_mark_and_none_past_it() {
    let ten_marks = Fixed::from_int(1_000_000);
    for (balance, priced) in [(940_000, true), (960_000, false)] {
        let rig = with_balance(btc(Side::Sell, 1, 0), balance);
        let found = closeout_of(&rig, TRADER, 1);
        match (priced, found.reach, found.price) {
            (true, ReachKind::At, Some(price)) => {
                assert!(price > Fixed::from_int(900_000) && price < ten_marks, "{price}")
            }
            (false, ReachKind::Beyond, None) => {}
            other => panic!("{balance} of equity: {other:?}"),
        }
    }
}

/// A 1 BTC long at 100000 on 95250 of equity falls to 5000 before the slack is gone, a twentieth of its
/// mark, so the range reaches at least that far down and the engine agrees.
#[test]
fn a_long_is_priced_down_to_a_twentieth_of_its_mark() {
    let price = Case::new(1, || with_balance(btc(Side::Buy, 1, 0), 95_250)).agrees();
    assert!(price > Fixed::from_int(4_000) && price < Fixed::from_int(6_000), "{price}");
}

#[test]
fn the_answer_names_each_position_and_leaves_the_price_out_unless_the_reach_is_at() {
    let mut rig = btc(Side::Buy, 5, 0);
    quote(&mut rig, ETH, "4000", T0 + 2_000);
    rig.trade_in(ETH, T0 + 2_100, TRADER, Side::Sell, 1);
    let acct = &rig.engine.accounts()[&TRADER];
    let standing = rig.engine.equity_and_margin(acct, None);
    let found = account_closeouts(&rig.engine, acct, standing);
    let json = serde_json::to_value(&found).unwrap();
    assert_eq!(json[0]["market"], 1);
    assert_eq!(json[0]["reach"], "at");
    assert!(json[0]["price"].as_str().unwrap().parse::<Fixed>().unwrap() < Fixed::from_int(100_000));
    assert_eq!((json[1]["market"].as_i64(), json[1]["reach"].as_str()), (Some(2), Some("beyond")));
    assert!(json[1]["price"].is_null(), "{json}");
}

/// Moving an FX market also moves the dollar value of the account's positions quoted in its currency,
/// which the estimate holds at their marks. With 250k of DE40 beside a 200k EUR-USD long, the price
/// is 13 bps of the mark from the engine's: the one case the estimate is not exact to the tick.
#[test]
fn an_fx_market_is_priced_within_15_bps_with_positions_quoted_in_its_currency_held() {
    let build = || {
        let mut rig = Rig::new();
        quote(&mut rig, EUR_USD, "1.08", T0 + 500);
        quote(&mut rig, DE40, "23500", T0 + 500);
        rig.trade_in(DE40, T0 + 600, TRADER, Side::Buy, 10);
        let events = rig.trade_in(EUR_USD, T0 + 700, TRADER, Side::Buy, 200_000);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
        rig
    };
    Case::new(EUR_USD, build).agrees_within(Fixed::from_raw(1_500_000));
}
