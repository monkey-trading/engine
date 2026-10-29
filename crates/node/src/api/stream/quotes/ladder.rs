//! One account's executable quotes in one market, read from the engine with `Engine::quote`: the
//! same pricing and risk checks a fill uses.

use engine_core::event::Reject;
use engine_core::fixed::Rounding;
use engine_core::types::{AccountId, MarketId, Side};
use engine_core::{Engine, Fixed, Quoter};
use serde::Serialize;

/// Halvings of the gap between the largest size known to fill and the smallest known to be declined.
const BISECTIONS: usize = 10;
/// Halvings of a declined size while looking for one the account can open: enough to go from the
/// largest order (`Fixed::MAX_INPUT`) to the smallest (one unit), so the floor is always reached.
const HALVINGS: usize = 64;
/// Doublings from the least size while looking for a size the account cannot open: enough to pass
/// the largest order.
const DOUBLINGS: usize = 64;

/// A market's quotes for one account. A buy pays `ask` and a sell receives `bid`; an order takes
/// the price of the first step whose `qty` is at least its own. That price is the average for the
/// step's whole size, which is at least that of any smaller order but for the rounding of a fill's
/// cost up to a unit (`Engine::quote`), so an order quoted at it fills within its slippage bound
/// while nothing moves, and one with a `declined` step is declined.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename = "quote")]
pub(super) struct MarketQuote {
    pub account: AccountId,
    pub market: MarketId,
    pub open: bool,
    /// Whether the market takes no new risk: its own flag, or every market's.
    pub reduce_only: bool,
    pub stress_level: usize,
    pub mark: Option<Fixed>,
    /// Why the market has no price to fill at now; the sides are then empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<Reject>,
    pub ask: Vec<Step>,
    pub bid: Vec<Step>,
    /// The price of closing the account's whole position, if it holds one.
    pub close: Option<Close>,
}

/// The average price of an order of up to `qty`, counted from the account's position.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Step {
    pub qty: Fixed,
    pub price: Fixed,
    /// Why a fill of this size would be declined for margin or house capacity; absent when it fills.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declined: Option<Reject>,
}

/// The price of closing the account's whole position, a `qty` order on `side`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(super) struct Close {
    pub side: Side,
    pub qty: Fixed,
    pub price: Fixed,
}

/// `account`'s quotes in `market_id`; `None` for a market the engine does not list.
pub(super) fn market_quote(
    engine: &Engine,
    quoter: &Quoter,
    account: AccountId,
    market_id: MarketId,
) -> Option<MarketQuote> {
    let market = engine.markets().get(&market_id)?;
    let mut quote = MarketQuote {
        account,
        market: market_id,
        open: market.open,
        reduce_only: engine.is_reduce_only(market),
        stress_level: market.stress.level,
        mark: market.mark,
        unavailable: None,
        ask: Vec::new(),
        bid: Vec::new(),
        close: None,
    };
    let mid = market.index(engine.now()).map(|index| index.mid);
    let priced = match (market.open, mid, engine.live_fx(market_id)) {
        (false, ..) => Err(Reject::MarketClosed),
        (_, Err(error), _) => Err(Reject::NoPrice { error }),
        (_, _, Err(reject)) => Err(reject),
        (_, Ok(mid), Ok(fx)) => Ok((mid, fx)),
    };
    let (mid, fx) = match priced {
        Ok(priced) => priced,
        Err(reject) => {
            quote.unavailable = Some(reject);
            return Some(quote);
        }
    };
    let tops: Vec<Fixed> = market
        .spec
        .spread_tiers
        .iter()
        .filter_map(|tier| tier.up_to)
        .map(|usd| market.spec.quote_currency.from_usd(usd, fx, Rounding::Down).div_rounded(mid, Rounding::Down))
        .collect();
    // The least size worth looking for: the market's minimum order, and at least the smallest quantity.
    let floor = market
        .spec
        .quote_currency
        .from_usd(market.spec.min_order_notional, fx, Rounding::Up)
        .div_rounded(mid, Rounding::Up)
        .max(Fixed::from_raw(1));
    let held = engine.accounts().get(&account).map_or(Fixed::ZERO, |acct| acct.position(market_id).qty);
    let position = Held { qty: held, floor, tops: &tops };
    quote.ask = steps(quoter, account, market_id, Side::Buy, &position);
    quote.bid = steps(quoter, account, market_id, Side::Sell, &position);
    if !held.is_zero() {
        let side = if held.is_positive() { Side::Sell } else { Side::Buy };
        quote.close = quoter.quote(account, market_id, side, held.abs()).ok().map(|q| Close {
            side,
            qty: held.abs(),
            price: q.price,
        });
    }
    Some(quote)
}

/// What a ladder is built from: the account's position, the least size searched for, and the
/// spread tiers' upper bounds as absolute positions.
struct Held<'a> {
    qty: Fixed,
    floor: Fixed,
    tops: &'a [Fixed],
}

/// The steps of one side, by size: each spread tier's top as the account would reach it, with the
/// reason a fill of that size is declined, if one is; the largest size that fills below the first
/// declined top, where there is one; and the largest size the account can open in the unbounded
/// last tier, unless a tier before it was declined. An order grows the absolute position from the
/// account's, so it is shorter than a top by what the account holds on its own side, and longer by
/// what it holds on the other, which it closes first. Each price is the worst of its own and the
/// steps before it, so it holds for every smaller order even where a tier charges less than the
/// one before. A side with no step cannot open any size.
fn steps(quoter: &Quoter, account: AccountId, market: MarketId, side: Side, held: &Held) -> Vec<Step> {
    let quote = |qty: Fixed| quoter.quote(account, market, side, qty).ok();
    let fits = |qty: Fixed| quote(qty).filter(|quote| quote.declined.is_none()).map(|quote| quote.price);
    let same_side = held.qty.is_zero() || held.qty.signum() == side.sign();
    let orders = held.tops.iter().filter_map(|top| {
        let qty = if same_side { *top - held.qty.abs() } else { *top + held.qty.abs() };
        qty.is_positive().then_some(qty)
    });
    let mut steps: Vec<Step> = Vec::new();
    // The largest size known to fill, which includes closing what the account holds on the other
    // side, and whether a tier's top is declined.
    let (mut known, mut declined) = (if same_side { Fixed::ZERO } else { held.qty.abs() }, false);
    for qty in orders {
        let Some(priced) = quote(qty) else { continue };
        if priced.declined.is_none() {
            known = known.max(qty);
        } else if !declined {
            declined = true;
            let step = largest(&fits, held.floor, known, qty)
                .filter(|step| steps.last().is_none_or(|last| step.qty > last.qty));
            steps.extend(step);
        }
        steps.push(Step { qty, price: priced.price, declined: priced.declined });
    }
    if !declined {
        // The unbounded last tier: double until the account cannot open the size.
        let mut probe = known.max(held.floor);
        let mut over = None;
        for _ in 0..DOUBLINGS {
            probe = probe.mul_int(2);
            if fits(probe).is_none() {
                over = Some(probe);
                break;
            }
            known = probe;
        }
        let step = match over {
            Some(over) => largest(&fits, held.floor, known, over),
            None => fits(known).map(|price| Step { qty: known, price, declined: None }),
        };
        let step = step.filter(|step| steps.last().is_none_or(|last| step.qty > last.qty));
        steps.extend(step);
    }
    let worst = |a: Fixed, b: Fixed| if side == Side::Buy { a.max(b) } else { a.min(b) };
    let mut bound: Option<Fixed> = None;
    for step in &mut steps {
        step.price = bound.map_or(step.price, |bound| worst(step.price, bound));
        bound = Some(step.price);
    }
    steps
}

/// The largest size between `known`, which fills (zero if none is known), and `declined`, which does
/// not, with its price: found by trying `floor`, the least size worth looking for, then halving from
/// `declined` down to it while none is known, and then bisecting. `None` if no size from `floor` to
/// `declined` fills.
pub(super) fn largest(
    fits: &impl Fn(Fixed) -> Option<Fixed>,
    floor: Fixed,
    known: Fixed,
    declined: Fixed,
) -> Option<Step> {
    let (mut low, mut high) = (known, declined);
    if low.is_zero() {
        fits(floor)?;
        let mut probe = high;
        for _ in 0..HALVINGS {
            probe = probe.div_int(2).max(floor);
            if probe >= high {
                break;
            }
            if fits(probe).is_some() {
                low = probe;
                break;
            }
            high = probe;
        }
        // Halving ended at the floor, which fits.
        if low.is_zero() {
            low = floor;
        }
    }
    for _ in 0..BISECTIONS {
        let mid = low + (high - low).div_int(2);
        if mid == low {
            break;
        }
        if fits(mid).is_some() {
            low = mid;
        } else {
            high = mid;
        }
    }
    fits(low).map(|price| Step { qty: low, price, declined: None })
}
