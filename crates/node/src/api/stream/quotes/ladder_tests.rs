//! The ladder against the engine it is read from: every step is checked with `Engine::quote` and,
//! where it matters, by placing the order.

use engine_core::command::{Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::{Event, Reject};
use engine_core::price::SourceQuote;
use engine_core::types::{AccountId, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};

use super::ladder::{MarketQuote, Step, largest, market_quote};
use super::tests::{T0, TRADER, engine};

const BTC: MarketId = 1;
const ETH: MarketId = 2;
const EUR_USD: MarketId = 10;
const DE40: MarketId = 23;
/// An account with 3 million USD, which runs out of margin before the house runs out of capacity.
const MARGIN: AccountId = AccountId([10; 20]);
/// An account with five cents.
const TINY: AccountId = AccountId([11; 20]);
/// An account that is short ETH in `with_an_eth_position`.
const SHORT: AccountId = AccountId([12; 20]);

fn fixed(text: &str) -> Fixed {
    text.parse().unwrap()
}

fn apply(engine: &mut Engine, ts: Millis, cmd: Command) -> Vec<Event> {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    engine.apply(&Sequenced { seq, ts, cmd }).unwrap()
}

fn price(engine: &mut Engine, market: MarketId, mid: Fixed, ts: Millis) {
    let quotes = ["a", "b"].map(|source| SourceQuote { source: source.into(), mid, ts, contract: None }).to_vec();
    apply(engine, ts, Command::Prices { market, quotes });
}

/// The dev engine with `capital` more USD of house capital.
fn funded(capital: i64) -> Engine {
    let mut engine = engine();
    let amount = Fixed::from_int(capital);
    apply(&mut engine, T0, Command::HouseCapital { amount, chain_ref: "0xh".into() });
    engine
}

fn deposit(engine: &mut Engine, account: AccountId, amount: Fixed) {
    apply(engine, T0, Command::Deposit { account, amount, chain_ref: format!("0x{account}") });
}

fn read(engine: &Engine, account: AccountId, market: MarketId) -> MarketQuote {
    market_quote(engine, &engine.quoter(), account, market).expect("a listed market")
}

fn order(account: AccountId, market: MarketId, side: Side, qty: Fixed, quoted: Fixed, bps: i64) -> Command {
    Command::PlaceOrder(OrderRequest {
        account,
        client_id: 1,
        market,
        side,
        qty,
        kind: OrderKind::Market { quoted, max_slippage_bps: Some(bps) },
        reduce_only: false,
        auth: None,
    })
}

fn fills(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::Fill(_)))
}

fn kind(reject: &Option<Reject>) -> String {
    serde_json::to_value(reject).unwrap()["kind"].as_str().unwrap().to_owned()
}

fn sides(quote: &MarketQuote) -> [(Side, &Vec<Step>); 2] {
    [(Side::Buy, &quote.ask), (Side::Sell, &quote.bid)]
}

/// The account's quote of `qty` on `side`, which must fill.
fn fitting(engine: &Engine, account: AccountId, market: MarketId, side: Side, qty: Fixed) -> Fixed {
    let quote = engine.quote(account, market, side, qty).unwrap_or_else(|e| panic!("{qty}: {e:?}"));
    assert_eq!(quote.declined, None, "{qty}");
    quote.price
}

/// The last tier is quoted at the largest size the account can open, when both bounded tops fit.
fn assert_last_tier(engine: &Engine, account: AccountId, declined_for: &str) {
    let quote = read(engine, account, BTC);
    for (side, steps) in sides(&quote) {
        let qtys: Vec<Fixed> = steps.iter().map(|step| step.qty).collect();
        // The two bounded tops (20 and 100 BTC), then the unbounded tier's step.
        assert_eq!(qtys.len(), 3, "{side:?}: {quote:?}");
        assert_eq!(qtys[..2], [Fixed::from_int(20), Fixed::from_int(100)]);
        assert!(steps.iter().all(|step| step.declined.is_none()), "{side:?}: {steps:?}");
        let last = &steps[2];
        assert!(last.qty > Fixed::from_int(100), "{side:?}: {last:?}");
        // It fills at the price the step says (the tier is dearer than those before it) ...
        assert_eq!(fitting(engine, account, BTC, side, last.qty), last.price, "{side:?}");
        // ... and a little more is declined, for the limit that ended the search: the search brackets
        // the size within a thousandth of the last doubling.
        let beyond = last.qty + last.qty.div_int(100);
        let over = engine.quote(account, BTC, side, beyond).unwrap();
        assert_eq!(kind(&over.declined), declined_for, "{side:?}: {beyond}");
        // The steps' prices never improve.
        let prices: Vec<Fixed> = steps.iter().map(|step| step.price).collect();
        let worse = |a: Fixed, b: Fixed| if side == Side::Buy { a <= b } else { a >= b };
        assert!(prices.windows(2).all(|pair| worse(pair[0], pair[1])), "{side:?}: {prices:?}");
    }
}

#[test]
fn the_last_tier_ends_where_the_house_runs_out_of_capacity() {
    // 100 million of house capital caps the account at 30% of it, 300 BTC, and 50 million of its own
    // margins far more than that.
    let engine = funded(90_000_000);
    assert_last_tier(&engine, TRADER, "house_capacity");
    let quote = read(&engine, TRADER, BTC);
    for (_, steps) in sides(&quote) {
        assert!(steps[2].qty <= Fixed::from_int(300) && steps[2].qty > Fixed::from_int(290), "{steps:?}");
    }
}

#[test]
fn the_last_tier_ends_where_the_account_runs_out_of_margin() {
    let mut engine = funded(90_000_000);
    deposit(&mut engine, MARGIN, Fixed::from_int(3_000_000));
    assert_last_tier(&engine, MARGIN, "insufficient_margin");
    let quote = read(&engine, MARGIN, BTC);
    for (_, steps) in sides(&quote) {
        assert!(steps[2].qty < Fixed::from_int(300), "{steps:?}");
    }
}

#[test]
fn a_ladder_in_a_market_quoted_in_euros_converts_each_top_at_the_fx_index() {
    let (fx, mid) = (fixed("1.1"), Fixed::from_int(2000));
    let build = || {
        let mut engine = funded(90_000_000);
        price(&mut engine, EUR_USD, fx, T0);
        price(&mut engine, DE40, mid, T0);
        engine
    };
    let engine = build();
    let quote = read(&engine, TRADER, DE40);
    assert_eq!(quote.unavailable, None);
    // The tiers end at 5 and 25 million USD of notional: euros at the FX index, then units at the mid.
    let tops = [5_000_000, 25_000_000].map(|usd| {
        Fixed::from_int(usd)
            .div_rounded(fx, engine_core::fixed::Rounding::Down)
            .div_rounded(mid, engine_core::fixed::Rounding::Down)
    });
    for (side, steps) in sides(&quote) {
        assert_eq!(steps.len(), 3, "{side:?}: {quote:?}");
        assert_eq!([steps[0].qty, steps[1].qty], tops, "{side:?}");
        assert!(steps.iter().all(|step| step.declined.is_none()), "{steps:?}");
        for step in steps {
            // An order at a step fills at its price, with a basis point to spare for none to be needed.
            let events = apply(&mut build(), T0, order(TRADER, DE40, side, step.qty, step.price, 0));
            assert!(fills(&events), "{side:?} {step:?}: {events:?}");
        }
    }
}

#[test]
fn a_market_that_is_closed_or_has_no_fx_price_says_why_and_has_no_steps() {
    let (fx, mid) = (fixed("1.1"), Fixed::from_int(2000));
    // Priced, then shut: the weekend.
    let mut closed = funded(0);
    price(&mut closed, EUR_USD, fx, T0);
    price(&mut closed, DE40, mid, T0);
    let saturday = T0 + 4 * 86_400_000;
    price(&mut closed, DE40, mid, saturday);
    let quote = read(&closed, TRADER, DE40);
    assert!(!quote.open);
    assert_eq!(kind(&quote.unavailable), "market_closed", "{quote:?}");
    assert!(quote.ask.is_empty() && quote.bid.is_empty() && quote.close.is_none());

    // Open and priced, but the euro has no index to convert at.
    let mut unpriced = funded(0);
    price(&mut unpriced, DE40, mid, T0);
    let quote = read(&unpriced, TRADER, DE40);
    assert!(quote.open);
    assert_eq!(quote.unavailable, Some(Reject::NoFxPrice { fx_market: EUR_USD }), "{quote:?}");
    assert!(quote.ask.is_empty() && quote.bid.is_empty());
}

#[test]
fn an_account_that_can_open_under_a_dollar_gets_a_step_for_it() {
    let mut engine = engine();
    deposit(&mut engine, TINY, fixed("0.05"));
    let quote = read(&engine, TINY, BTC);
    // Five cents at 10% margin opens half a dollar: 0.000005 BTC at 100,000.
    for (side, steps) in sides(&quote) {
        let fitting_steps: Vec<_> = steps.iter().filter(|step| step.declined.is_none()).collect();
        let step = fitting_steps.last().unwrap_or_else(|| panic!("{side:?}: {quote:?}"));
        assert!(step.qty > fixed("0.0000049") && step.qty <= fixed("0.000005"), "{side:?}: {step:?}");
        assert_eq!(fitting(&engine, TINY, BTC, side, step.qty), step.price);
        let beyond = engine.quote(TINY, BTC, side, step.qty + step.qty.div_int(100)).unwrap();
        assert_eq!(kind(&beyond.declined), "insufficient_margin", "{side:?}");
    }
}

/// An engine where `TRADER` is long and `SHORT` short 1,000 ETH at 1,000 USD, so the next unit is
/// in the 3 bps tier.
fn with_an_eth_position() -> Engine {
    let mut engine = funded(90_000_000);
    deposit(&mut engine, SHORT, Fixed::from_int(50_000_000));
    price(&mut engine, ETH, Fixed::from_int(1000), T0);
    for (account, side) in [(TRADER, Side::Buy), (SHORT, Side::Sell)] {
        let events =
            apply(&mut engine, T0, order(account, ETH, side, Fixed::from_int(1000), Fixed::from_int(1000), 10_000));
        assert!(fills(&events), "{events:?}");
    }
    engine
}

/// The documented limit of a step's price: a fill's spread cost rounds up to a unit before it is
/// divided by the order's size, so an order of a cent's notional is within a basis point of the
/// step, and one a thousand millionth of that is not.
#[test]
fn a_step_prices_an_order_of_a_cent_within_a_basis_point_and_a_far_smaller_one_above_it() {
    let engine = with_an_eth_position();
    let one_bp = |price: Fixed, step: Fixed| (price - step).abs().mul_int(10_000) <= step;
    for (account, side) in [(TRADER, Side::Buy), (SHORT, Side::Sell)] {
        let quote = read(&engine, account, ETH);
        let steps = if side == Side::Buy { &quote.ask } else { &quote.bid };
        let step = steps[0].price;
        // 0.00001 ETH is a cent at 1,000 USD.
        let cent = fitting(&engine, account, ETH, side, fixed("0.00001"));
        assert!(one_bp(cent, step), "{side:?}: {cent} against {step}");
        let tiny = fitting(&engine, account, ETH, side, fixed("0.000000001"));
        assert!(!one_bp(tiny, step), "{side:?}: {tiny} against {step}");
    }
}

#[test]
fn the_largest_size_search_reaches_the_smallest_quantity_from_the_largest_order() {
    let up_to = |limit: Fixed| move |qty: Fixed| (qty <= limit).then_some(qty);
    let floor = Fixed::from_raw(1);
    for limit in [Fixed::from_raw(1), Fixed::from_raw(5_000), Fixed::from_raw(3_000_000)] {
        let found = largest(&up_to(limit), floor, Fixed::ZERO, Fixed::MAX_INPUT).expect("a size").qty;
        // Within a thousandth after the bisection, or exact where the gap is under a unit.
        assert!(found <= limit && found >= limit - limit.div_int(500), "{limit}: {found}");
    }
}
