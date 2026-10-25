//! The generated sessions: commands with values inside and outside the engine's input bounds,
//! and how each is resolved against the engine's marks when it is sent.

use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::market::{FinancingSpec, MarketSpec, PriceRules, Tier};
use engine_core::params::{NotionalWeighting, ProtocolParams, ToxicityParams};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};
use proptest::arbitrary::any;
use proptest::array::uniform3;
use proptest::collection::vec;
use proptest::option;
use proptest::prop_oneof;
use proptest::sample::select;
use proptest::strategy::{Just, Strategy};

use crate::common::stress::with_divergence_shares;
use crate::common::{AAPL, BTC, Client, DE40, ETH, EURUSD, UNLISTED, USDJPY, acct, market, params, us_equities};

/// A market id only generated listings use.
const NEW_MARKET: MarketId = 10;
const DAY_MS: Millis = 86_400_000;

/// A generated session: deposits for the three traders, then the steps.
#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub(crate) funding: [Fixed; 3],
    pub(crate) steps: Vec<Step>,
}

/// One generated step: move the clock, then sequence the command built from `action`.
#[derive(Clone, Debug)]
pub(crate) struct Step {
    pub(crate) advance_ms: Millis,
    pub(crate) action: Action,
}

/// A command before its prices are resolved against the engine's marks at the time it is sent.
#[derive(Clone, Debug)]
pub(crate) enum Action {
    Tick,
    Prices { market: MarketId, move_bps: i64, quotes: Vec<Quote> },
    Deposit { account: AccountId, amount: Fixed, chain_ref: u32 },
    Withdraw { account: AccountId, amount: Fixed },
    HouseCapital { amount: Fixed, chain_ref: u32 },
    Order(Order),
    Cancel { account: AccountId, order_id: u64 },
    Admin(Box<AdminCommand>),
}

/// An order before its size and price are resolved.
#[derive(Clone, Debug)]
pub(crate) struct Order {
    account: AccountId,
    market: MarketId,
    side: Side,
    size: Size,
    kind: Kind,
    price: Price,
    max_slippage_bps: Option<i64>,
    reduce_only: bool,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Market,
    Limit,
    Stop,
}

/// An order size: a leverage on the account's equity or a notional, both at the market's mark, or
/// an exact qty that may be out of bounds. Notionals stay within `cash`, so converting one at a
/// valid mark cannot overflow.
#[derive(Clone, Copy, Debug)]
enum Size {
    Leverage(i64),
    Notional(Fixed),
    Qty(Fixed),
}

/// A price relative to the market's mark, or an exact value that may be out of bounds.
#[derive(Clone, Copy, Debug)]
enum Price {
    Bps(i64),
    Exact(Fixed),
}

/// A venue quote: its mid relative to the moved market price, and its age at the sequenced time or
/// an exact timestamp.
#[derive(Clone, Debug)]
pub(crate) struct Quote {
    mid: Price,
    stamp: Stamp,
}

#[derive(Clone, Copy, Debug)]
enum Stamp {
    Age(Millis),
    Exact(Millis),
}

/// The command a step sends, with relative prices resolved against the engine's current marks, from
/// `V8` admin commands signed by the operator with its next nonce, and from `V13` new parameters
/// with divergence triggers as shares.
pub(crate) fn command(action: &Action, engine: &Engine, now: Millis) -> Command {
    match action {
        Action::Tick => Command::Tick,
        Action::Prices { market, move_bps, quotes } => {
            let center = moved(reference(engine, *market), *move_bps);
            let quotes = quotes
                .iter()
                .enumerate()
                .map(|(venue, quote)| SourceQuote {
                    source: format!("venue{venue}"),
                    mid: resolve(quote.mid, center),
                    ts: match quote.stamp {
                        Stamp::Age(ms) => now - ms,
                        Stamp::Exact(ts) => ts,
                    },
                    contract: None,
                })
                .collect();
            Command::Prices { market: *market, quotes }
        }
        Action::Deposit { account, amount, chain_ref } => {
            Command::Deposit { account: *account, amount: *amount, chain_ref: format!("0xref:{chain_ref}") }
        }
        Action::Withdraw { account, amount } => Command::Withdraw { account: *account, amount: *amount, auth: None },
        Action::HouseCapital { amount, chain_ref } => {
            Command::HouseCapital { amount: *amount, chain_ref: format!("0xref:{chain_ref}") }
        }
        Action::Order(order) => {
            let mark = reference(engine, order.market);
            let price = resolve(order.price, mark);
            let qty = match order.size {
                Size::Leverage(bps) => {
                    let equity =
                        engine.accounts().get(&order.account).map(|acct| engine.equity_and_margin(acct, None).0);
                    equity.unwrap_or_default().mul_bps(bps).div(mark)
                }
                Size::Notional(notional) => notional.div(mark),
                Size::Qty(qty) => qty,
            };
            let kind = match order.kind {
                Kind::Market => OrderKind::Market { quoted: price, max_slippage_bps: order.max_slippage_bps },
                Kind::Limit => OrderKind::Limit { price },
                Kind::Stop => OrderKind::Stop { price },
            };
            Command::PlaceOrder(OrderRequest {
                account: order.account,
                client_id: 0,
                market: order.market,
                side: order.side,
                qty,
                kind,
                reduce_only: order.reduce_only,
                auth: None,
            })
        }
        Action::Cancel { account, order_id } => {
            Command::CancelOrder { account: *account, order_id: *order_id, auth: None }
        }
        Action::Admin(cmd) if engine.protocol_version() >= ProtocolVersion::V8 => {
            // From V13 a divergence trigger is a share of each market's bound.
            let cmd = match cmd.as_ref() {
                AdminCommand::SetParams(params) if engine.protocol_version() >= ProtocolVersion::V13 => {
                    AdminCommand::SetParams(with_divergence_shares(params.clone()))
                }
                cmd => cmd.clone(),
            };
            let operator = Client { expires_at: now + 60_000, ..Client::operator() };
            operator.admin(engine.next_request_nonce(operator.account), cmd)
        }
        Action::Admin(cmd) => Command::Admin(cmd.as_ref().clone()),
    }
}

/// The market's mark, or a typical level before it has one.
fn reference(engine: &Engine, market: MarketId) -> Fixed {
    let typical = match market {
        BTC => 100_000,
        ETH => 3_000,
        AAPL => 200,
        DE40 => 20_000,
        USDJPY => 150,
        EURUSD => 1,
        _ => 100,
    };
    engine.markets().get(&market).and_then(|m| m.mark).unwrap_or(Fixed::from_int(typical))
}

/// `price` moved by `bps`.
fn moved(price: Fixed, bps: i64) -> Fixed {
    price + price.mul_bps(bps)
}

/// A relative price against `base`, or the exact one.
fn resolve(price: Price, base: Fixed) -> Fixed {
    match price {
        Price::Bps(bps) => moved(base, bps),
        Price::Exact(value) => value,
    }
}

/// Funding for the three traders and up to 200 steps.
pub(crate) fn session() -> impl Strategy<Value = Session> {
    (uniform3(cash()), vec(step(), 1..=200)).prop_map(|(funding, steps)| Session { funding, steps })
}

/// A step: mostly a short clock move so quotes stay fresh, sometimes up to two days so sessions
/// turn and rollovers fire.
fn step() -> impl Strategy<Value = Step> {
    let advance = prop_oneof![40 => 0..=100i64, 1 => 0..=2 * DAY_MS];
    (advance, action()).prop_map(|(advance_ms, action)| Step { advance_ms, action })
}

/// Any command, weighted towards prices and orders.
fn action() -> impl Strategy<Value = Action> {
    prop_oneof![
        1 => Just(Action::Tick),
        16 => (market_id(), move_bps(), prop_oneof![4 => vec(quote(), 2..=3), 1 => vec(quote(), 0..2)])
            .prop_map(|(market, move_bps, quotes)| Action::Prices { market, move_bps, quotes }),
        2 => (account(), funds(), chain_ref())
            .prop_map(|(account, amount, chain_ref)| Action::Deposit { account, amount, chain_ref }),
        1 => (account(), funds()).prop_map(|(account, amount)| Action::Withdraw { account, amount }),
        1 => (signed_amount(), chain_ref()).prop_map(|(amount, chain_ref)| Action::HouseCapital { amount, chain_ref }),
        10 => order().prop_map(Action::Order),
        1 => (account(), 0u64..40).prop_map(|(account, order_id)| Action::Cancel { account, order_id }),
        2 => admin().prop_map(|cmd| Action::Admin(Box::new(cmd))),
    ]
}

/// An order of any kind: a market order quoted near the mark, a limit or stop up to 5% away, or an
/// exact, possibly invalid, price; sized as a leverage on equity, a notional, or an exact qty.
fn order() -> impl Strategy<Value = Order> {
    let exact = || amount().prop_map(Price::Exact);
    let quoted = prop_oneof![8 => (-40i64..=40).prop_map(Price::Bps), 1 => exact()];
    let trigger = || prop_oneof![8 => (-500i64..=500).prop_map(Price::Bps), 1 => exact()];
    let kind = prop_oneof![
        2 => (Just(Kind::Market), quoted),
        1 => (Just(Kind::Limit), trigger()),
        1 => (Just(Kind::Stop), trigger()),
    ];
    let size = prop_oneof![
        6 => (1i64..=120_000).prop_map(Size::Leverage),
        2 => cash().prop_map(Size::Notional),
        1 => amount().prop_map(Size::Qty),
    ];
    let slippage = prop_oneof![Just(None), (-50i64..=50).prop_map(Some), bps().prop_map(Some)];
    let reduce_only = prop_oneof![4 => Just(false), 1 => Just(true)];
    (account(), market_id(), side(), size, kind, slippage, reduce_only).prop_map(
        |(account, market, side, size, (kind, price), max_slippage_bps, reduce_only)| Order {
            account,
            market,
            side,
            size,
            kind,
            price,
            max_slippage_bps,
            reduce_only,
        },
    )
}

/// A venue quote: mostly close to the moved price and fresh, sometimes divergent, stale, from the
/// future, or out of bounds.
fn quote() -> impl Strategy<Value = Quote> {
    let mid = prop_oneof![8 => (-15i64..=15).prop_map(Price::Bps), 1 => (-100i64..=100).prop_map(Price::Bps),
        1 => amount().prop_map(Price::Exact)];
    let stamp = prop_oneof![300 => (0i64..=300).prop_map(Stamp::Age), 30 => (300i64..=3_000).prop_map(Stamp::Age),
        30 => (-1_000i64..0).prop_map(Stamp::Age), 10 => (i64::MIN..0).prop_map(Stamp::Exact),
        1 => any::<i64>().prop_map(Stamp::Exact)];
    (mid, stamp).prop_map(|(mid, stamp)| Quote { mid, stamp })
}

/// An operator command. Parameter and listing changes alter one field of the harness values at a
/// time, so most pass validation and the generated value takes effect.
fn admin() -> impl Strategy<Value = AdminCommand> {
    let benchmark = select(vec!["SOFR", "ESTR"]);
    let level = prop_oneof![3 => 0usize..4, 1 => any::<usize>(), 1 => Just(usize::MAX)];
    prop_oneof![
        (benchmark, bps()).prop_map(|(benchmark, annual_bps)| AdminCommand::SetBenchmarkRate {
            benchmark: benchmark.into(),
            annual_bps
        }),
        (option::of(market_id()), level).prop_map(|(market, level)| AdminCommand::SetStressFloor { market, level }),
        (option::of(market_id()), any::<bool>())
            .prop_map(|(market, reduce_only)| AdminCommand::SetReduceOnly { market, reduce_only }),
        params_change().prop_map(AdminCommand::SetParams),
        listing().prop_map(AdminCommand::UpsertMarket),
    ]
}

/// The harness parameters with one field, or one stress level, changed.
fn params_change() -> impl Strategy<Value = ProtocolParams> {
    let toxicity = |horizon_ms, ewma_weight_bps| ToxicityParams { horizon_ms, ewma_weight_bps, ..params().toxicity };
    prop_oneof![
        bps().prop_map(|profit_fee_bps| ProtocolParams { profit_fee_bps, ..params() }),
        bps().prop_map(|closeout_level_bps| ProtocolParams { closeout_level_bps, ..params() }),
        bps().prop_map(|house_total_cap_bps| ProtocolParams { house_total_cap_bps, ..params() }),
        duration().prop_map(|stress_move_window_ms| ProtocolParams { stress_move_window_ms, ..params() }),
        duration().prop_map(|stress_cooldown_ms| ProtocolParams { stress_cooldown_ms, ..params() }),
        amount().prop_map(|house_total_cap_floor| ProtocolParams { house_total_cap_floor, ..params() }),
        duration().prop_map(move |horizon| ProtocolParams { toxicity: toxicity(horizon, 5_000), ..params() }),
        bps().prop_map(move |weight| ProtocolParams { toxicity: toxicity(1_000, weight), ..params() }),
        // A prior as large as the memory: a fixed history every markout weighs its fill against.
        amount().prop_map(|history| ProtocolParams {
            toxicity: ToxicityParams {
                notional: Some(NotionalWeighting { prior: history, memory: history }),
                ..params().toxicity
            },
            ..params()
        }),
        prop_oneof![4 => 0usize..=4, 1 => any::<usize>()]
            .prop_map(|max_resting_orders| ProtocolParams { max_resting_orders, ..params() }),
        (bps(), bps()).prop_map(|(multiplier_bps, min_spread_bps)| {
            let mut changed = params();
            changed.stress_levels[1].size_spread_multiplier_bps = multiplier_bps;
            changed.stress_levels[1].min_spread_bps = min_spread_bps;
            changed
        }),
    ]
}

/// A 24/7 USD listing of a harness market or a new one, with one field changed. Listing Germany 40
/// in USD changes its quote currency, which only a market nobody holds may do.
fn listing() -> impl Strategy<Value = MarketSpec> {
    let id = || select(vec![BTC, ETH, AAPL, DE40, NEW_MARKET]);
    let listed = |id| market(id, "GEN", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    prop_oneof![
        id().prop_map(move |id| MarketSpec { schedule: us_equities(), ..listed(id) }),
        (id(), amount(), bps()).prop_map(move |(id, band, bps)| MarketSpec {
            margin_tiers: vec![Tier { up_to: Some(band), bps }, Tier { up_to: None, bps: 10_000 }],
            ..listed(id)
        }),
        (id(), amount(), bps()).prop_map(move |(id, band, bps)| MarketSpec {
            spread_tiers: vec![Tier { up_to: Some(band), bps: 0 }, Tier { up_to: None, bps }],
            ..listed(id)
        }),
        (id(), bps()).prop_map(move |(id, house_cap_bps)| MarketSpec { house_cap_bps, ..listed(id) }),
        (id(), amount()).prop_map(move |(id, house_cap_floor)| MarketSpec { house_cap_floor, ..listed(id) }),
        (id(), bps()).prop_map(move |(id, max_slippage_bps)| MarketSpec { max_slippage_bps, ..listed(id) }),
        (id(), amount()).prop_map(move |(id, min_order_notional)| MarketSpec { min_order_notional, ..listed(id) }),
        (id(), duration(), prop_oneof![0usize..4, any::<usize>()], bps()).prop_map(
            move |(id, max_age_ms, min_sources, max_divergence_bps)| MarketSpec {
                price_rules: PriceRules { max_age_ms, min_sources, max_divergence_bps },
                ..listed(id)
            }
        ),
        (id(), bps(), prop_oneof![4 => select(vec![360, 365]), 1 => any::<i64>()]).prop_map(
            move |(id, markup_bps, day_basis)| MarketSpec {
                financing: FinancingSpec { benchmark: "SOFR".into(), markup_bps, day_basis },
                ..listed(id)
            }
        ),
    ]
}

/// A positive value spread over eighteen orders of magnitude around the input bound, or an edge
/// case: zero, the bound and one past it, negative, or anything `Fixed` can hold.
fn amount() -> impl Strategy<Value = Fixed> {
    prop_oneof![
        8 => (1i128..1_000, 0u32..=17).prop_map(|(mantissa, exp)| Fixed::from_raw(mantissa * 10i128.pow(exp))),
        1 => select(vec![
            Fixed::ZERO,
            Fixed::from_raw(1),
            Fixed::MAX_INPUT,
            Fixed::MAX_INPUT + Fixed::from_raw(1),
            Fixed::from_raw(i128::MAX),
            Fixed::from_raw(-i128::MAX),
        ]),
        1 => (-i128::MAX..0).prop_map(Fixed::from_raw),
        1 => (-i128::MAX..=i128::MAX).prop_map(Fixed::from_raw),
    ]
}

/// Cash a trader might move: `cash`, or sometimes any `amount`.
fn funds() -> impl Strategy<Value = Fixed> {
    prop_oneof![4 => cash(), 1 => amount()]
}

/// From 1 to 10m, spread evenly over the orders of magnitude.
fn cash() -> impl Strategy<Value = Fixed> {
    (1i128..1_000, 9u32..=13).prop_map(|(mantissa, exp)| Fixed::from_raw(mantissa * 10i128.pow(exp)))
}

/// A custody reference: usually fresh, sometimes one of a few that repeat.
fn chain_ref() -> impl Strategy<Value = u32> {
    prop_oneof![1 => 0u32..3, 4 => any::<u32>()]
}

/// An amount of either sign.
fn signed_amount() -> impl Strategy<Value = Fixed> {
    (amount(), any::<bool>()).prop_map(|(amount, negate)| if negate { -amount } else { amount })
}

/// A market move in bps: mostly small, sometimes a gap of up to 30%.
fn move_bps() -> impl Strategy<Value = i64> {
    prop_oneof![6 => -30i64..=30, 3 => -300i64..=300, 1 => -3_000i64..=3_000]
}

/// A rate in bps: mostly within what validation accepts, sometimes any `i64` or an edge of the
/// range.
fn bps() -> impl Strategy<Value = i64> {
    prop_oneof![8 => -100i64..=20_000, 1 => any::<i64>(), 1 => edge_i64()]
}

/// A duration in ms: up to ten minutes, any `i64`, or an edge of the range.
fn duration() -> impl Strategy<Value = i64> {
    prop_oneof![2 => 1i64..=600_000, 1 => any::<i64>(), 1 => edge_i64()]
}

/// The ends of the `i64` range, and the values either side of zero.
fn edge_i64() -> impl Strategy<Value = i64> {
    select(vec![i64::MIN, -1, 0, 1, i64::MAX])
}

/// Three traders and an account that is rarely credited.
fn account() -> impl Strategy<Value = AccountId> {
    prop_oneof![8 => select(vec![acct(1), acct(2), acct(3)]), 1 => Just(acct(9))]
}

/// A listed market, mostly a 24/7 one, or an unlisted id.
fn market_id() -> impl Strategy<Value = MarketId> {
    prop_oneof![16 => select(vec![BTC, ETH]), 4 => Just(AAPL), 8 => select(vec![EURUSD, USDJPY, DE40]), 1 => Just(UNLISTED)]
}

/// Either side.
fn side() -> impl Strategy<Value = Side> {
    select(vec![Side::Buy, Side::Sell])
}
