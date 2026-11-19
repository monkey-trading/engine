//! Close-outs under portfolio margin from `V81` at 100k accounts (#639): a price's detection, with
//! accounts well above their per-position level and with every account between its portfolio and
//! per-position levels, which the close-out index cannot skip; a price closing 20% of the accounts out;
//! the same over accounts holding a group of twenty shares; a rollover and a weekend margin's gap start,
//! with and without close-outs; and the check after the fills of 20k resting stops. The last three are each
//! held to twice `V79`'s command on the same book plus a guard, with #904's 500 ms bar for one command
//! printed beside them. Detection is timed over `SAMPLES` prices on one engine. A command that closes accounts out acts once
//! on an engine, so each of its `EXEC_SAMPLES` samples loads a fresh copy of the same fixture; a
//! fixture's construction and loading are outside the timed region.

use super::common::close_spread::{rest, trade};
use super::common::events::only_fill;
use super::common::portfolio::{genesis_with_grid, group, member, pairs};
use super::common::weekend::{GOLD, OIL, OIL_WEEKEND_BPS, chicago, client};
use super::*;
use engine_core::Fixed;
use engine_core::account::Position;
use engine_core::event::Event;
use engine_core::portfolio::PortfolioGrid;
use engine_core::types::Millis;

pub(super) const COUNT: usize = 100_000;
/// Samples of a command that acts once on an engine.
const EXEC_SAMPLES: usize = 3;
/// One account in this many is the one a case closes out or stops.
const ONE_IN: usize = 5;
/// The first of the share markets, listed as copies of AAPL.
const SHARES: MarketId = 200;
/// The `concentrated_100k` budget a price at 100k holders is held to.
const PRICE_BUDGET_US: u128 = 250_000;
/// The bar for a rollover, and for one command that closes out.
const ROLLOVER_BUDGET_US: u128 = 500_000;
/// The guard on the share book, whose bars of 250 and 500 ms are printed: on CI a price over 100k
/// accounts between their levels measured 0.43 s at p99, and closing out 20k of them 3.1 s.
const SHARES_GUARD_US: u128 = 6_000_000;
/// What `V81` may add to a command over twice `V79`'s on the same book where no account closes out: the
/// exact check of 100k unkeyed accounts, which took 0.15 s on CI for a price.
const ADDED_GUARD_US: u128 = 300_000;
/// The same where a fifth of the accounts close out, each a close-out `V79` does not run.
const CLOSING_GUARD_US: u128 = 1_000_000;
const V79: ProtocolVersion = ProtocolVersion::V79;
const V81: ProtocolVersion = ProtocolVersion::V81;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_price_detection_latency() {
    // One BTC-USD price among 100k holders of a two-market group and an ungrouped position, at a 50%
    // floor share, beside the plain book's `concentrated_100k`: no account closes out.
    let (_, reference) = detect(&mut fixture(COUNT, true, false), BTC, "100", "100.0001");
    println!("LATENCY concentrated_100k_reference accounts={COUNT} p99_us={reference}");
    for (name, standing) in [("portfolio_price_above", Standing::Above), ("portfolio_price_between", Standing::Between)]
    {
        let (mut engine, _) = build(&crypto(V81, 5_000), COUNT, standing);
        let unkeyed = COUNT as u64 - engine.closeout_coverage().keys;
        let (p50, p99) = detect(&mut engine, BTC, "100000", "100001");
        println!("LATENCY {name}_100k accounts={COUNT} unkeyed={unkeyed} samples={SAMPLES} p50_us={p50} p99_us={p99}");
        assert!(p99 <= PRICE_BUDGET_US, "{name} p99 {p99}us exceeds {PRICE_BUDGET_US}us");
    }
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_price_execution_latency() {
    // The same book, a BTC-USD price that takes a fifth of the accounts below their portfolio level.
    let (engine, targets) = build(&crypto(V81, 5_000), COUNT, Standing::Near(1_000));
    let bytes = serde_json::to_vec(&engine).expect("fixture encoding");
    drop(engine);
    let drop_to = f("100000") - targets.gap().mul_bps(3_000);
    let times = once("portfolio_price_execution_100k", &bytes, |engine, ts| {
        apply_events(engine, ts, prices(BTC, &drop_to.to_string(), ts))
    });
    assert!(times.0 <= ROLLOVER_BUDGET_US, "executing took {}us", times.0);
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_large_group_latency() {
    // Accounts each holding twenty shares of one group, ten long and ten short, at a 50% floor share, so
    // a close-out's step scores candidates over twenty members. `SHARE_ACCOUNTS` accounts.
    let spec = shares(20);
    for (name, standing) in
        [("portfolio_shares_above", Standing::Above), ("portfolio_shares_between", Standing::Between)]
    {
        let (mut engine, _) = build(&spec, SHARE_ACCOUNTS, standing);
        let unkeyed = SHARE_ACCOUNTS as u64 - engine.closeout_coverage().keys;
        let (p50, p99) = detect(&mut engine, SHARES, "200", "200.0001");
        println!(
            "LATENCY {name} accounts={SHARE_ACCOUNTS} unkeyed={unkeyed} samples={SAMPLES} p50_us={p50} p99_us={p99}"
        );
        let verdict = if p99 <= PRICE_BUDGET_US { "within" } else { "OVER" };
        println!("LATENCY bar {name} p99_us={p99} bar_us={PRICE_BUDGET_US} {verdict}");
        assert!(p99 <= SHARES_GUARD_US, "{name} p99 {p99}us exceeds {SHARES_GUARD_US}us");
    }
    let (engine, targets) = build(&spec, SHARE_ACCOUNTS, Standing::Near(1_000));
    let bytes = serde_json::to_vec(&engine).expect("fixture encoding");
    drop(engine);
    // Each account is long 50 of the first share.
    let drop_to = f("200") - targets.gap().mul_bps(3_000).div_int(50);
    let (p99, _) = once("portfolio_shares_execution", &bytes, |engine, ts| {
        apply_events(engine, ts, prices(SHARES, &drop_to.to_string(), ts))
    });
    // Closing twenty positions of each of 20k accounts is 400k fills, so the 500 ms bar for one command
    // is printed, as `hold_fill` prints its bar, and only a regression past `SHARES_GUARD_US` fails.
    let verdict = if p99 <= ROLLOVER_BUDGET_US { "within" } else { "OVER" };
    println!("LATENCY bar portfolio_shares_execution p99_us={p99} bar_us={ROLLOVER_BUDGET_US} {verdict}");
    assert!(p99 <= SHARES_GUARD_US, "executing took {p99}us");
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_rollover_latency() {
    // A day's tick over the portfolio book: every account checked after the day's financing. `V79` on the
    // same book with every account keyed, then `V81` with every account between its levels, which the
    // close-out index cannot key, with no close-out (`SAMPLES` days each), and with a fifth of the
    // accounts taken below their portfolio level by it.
    let (mut engine, _) = build(&crypto(V79, 5_000), COUNT, Standing::Above);
    let (_, reference) = rollovers("portfolio_rollover_v79_100k", &mut engine, &crypto(V79, 5_000));
    drop(engine);
    let spec = crypto(V81, 5_000);
    let (mut engine, _) = build(&spec, COUNT, Standing::Between);
    let (_, p99) = rollovers("portfolio_rollover_100k", &mut engine, &spec);
    drop(engine);
    bar("portfolio_rollover_100k", p99, ROLLOVER_BUDGET_US);
    assert!(p99 <= 2 * reference + ADDED_GUARD_US, "V81's rollover p99 {p99}us against V79's {reference}us");
    let (engine, _) = build(&spec, COUNT, Standing::NearDollars(f("0.5")));
    let bytes = serde_json::to_vec(&engine).expect("fixture encoding");
    drop(engine);
    let (p99, closed) = once("portfolio_rollover_closing_100k", &bytes, |engine, _| {
        let due = engine.next_rollover().expect("a rollover is scheduled");
        quote_all(engine, &spec, due - 1);
        apply_events(engine, due, Command::Tick)
    });
    assert!(closed > 0, "the financing takes accounts below their level");
    bar("portfolio_rollover_closing_100k", p99, ROLLOVER_BUDGET_US);
    assert!(p99 <= 2 * reference + CLOSING_GUARD_US, "the closing rollover took {p99}us against V79's {reference}us");
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_weekend_latency() {
    // The first command past Friday's Globex close over 100k holders of oil long and gold short as one
    // group, oil with its weekend margin and gold with none: oil's gap starts, and every account is valued
    // at the requirement in full over it, from `V81` the portfolio requirement and before it the
    // per-position one. The day's rollover falls at the same instant, 22:00 in London, so the command
    // carries it too.
    let mut reference = 0;
    for (name, version, standing) in [
        ("portfolio_weekend_v79", V79, Standing::Clear),
        ("portfolio_weekend_clear", V81, Standing::Clear),
        ("portfolio_weekend_closing", V81, Standing::Short),
    ] {
        let (engine, _) = build(&commodities(version), COUNT, standing);
        let bytes = serde_json::to_vec(&engine).expect("fixture encoding");
        drop(engine);
        let (p99, closed) = once(&format!("{name}_100k"), &bytes, |engine, _| {
            apply_events(engine, friday_close() + 1_000, Command::Tick)
        });
        assert_eq!(closed > 0, name.ends_with("closing"), "{name} closed {closed}");
        if version == V79 {
            reference = p99;
            continue;
        }
        bar(&format!("{name}_100k"), p99, ROLLOVER_BUDGET_US);
        let guard = if closed > 0 { CLOSING_GUARD_US } else { ADDED_GUARD_US };
        assert!(p99 <= 2 * reference + guard, "{name} took {p99}us against V79's {reference}us");
    }
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_stops_latency() {
    // One BTC-USD price triggers a sell stop for 20k of 100k accounts, each closing one leg of a hedged
    // pair: at `V79` nothing follows the fills; from `V81` each of the 20k is checked once they are done,
    // and, at a 1% floor share, the lone ETH-USD and AAPL positions left need more than the pair did, so
    // a fifth of them close out.
    let mut took = Vec::new();
    for (name, spec, standing) in [
        ("portfolio_stops_v79", stops(V79), Standing::Above),
        ("portfolio_stops_v81", stops(V81), Standing::Above),
        ("portfolio_stops_v81_closing", stops(V81), Standing::Near(2_800)),
    ] {
        let (engine, _) = build(&spec, COUNT, standing);
        let bytes = serde_json::to_vec(&engine).expect("fixture encoding");
        drop(engine);
        let (us, _) = once(name, &bytes, |engine, ts| apply_events(engine, ts, prices(BTC, "98900", ts)));
        took.push(us);
    }
    println!("LATENCY portfolio_stops_added_v81 us={}", took[1] as i128 - took[0] as i128);
    bar("portfolio_stops_v81_closing", took[2], ROLLOVER_BUDGET_US);
    assert!(took[1] <= 2 * took[0] + 50_000, "V81's stops took {}us against V79's {}us", took[1], took[0]);
    assert!(took[2] <= 2 * took[0] + CLOSING_GUARD_US, "V81's stops with close-outs took {}us", took[2]);
}

/// How an account stands against its two levels at the marks, by its index.
#[derive(Clone, Copy)]
pub(super) enum Standing {
    /// At twice its per-position level.
    Above,
    /// Three quarters of the way from its portfolio level to its per-position one.
    Between,
    /// A fifth of the accounts, within this share (bps) of the gap above their portfolio level, the
    /// rest `Between`.
    Near(i64),
    /// As `Near`, within these dollars of the portfolio level.
    NearDollars(Fixed),
    /// At one and a half times the requirement over the weekend gap.
    Clear,
    /// A fifth of the accounts at nine tenths of the requirement over the weekend gap, the rest `Clear`.
    Short,
}

/// An account's levels at the marks: the close-out level of the portfolio requirement and of the
/// per-position bands, and the requirement in full over oil's weekend gap, or the bands with none.
pub(super) struct Targets {
    portfolio: Fixed,
    per_position: Fixed,
    weekend: Fixed,
}

impl Targets {
    fn gap(&self) -> Fixed {
        self.per_position - self.portfolio
    }
}

impl Standing {
    fn equity(self, index: usize, t: &Targets) -> Fixed {
        let one_in = index.is_multiple_of(ONE_IN);
        let between = t.portfolio + t.gap().mul_bps(7_500);
        match self {
            Standing::Above => t.per_position.mul_bps(20_000),
            Standing::Between => between,
            Standing::Near(share) if one_in => t.portfolio + t.gap().mul_bps(share),
            Standing::NearDollars(dollars) if one_in => t.portfolio + dollars,
            Standing::Near(_) | Standing::NearDollars(_) => between,
            Standing::Clear => t.weekend.mul_bps(15_000),
            Standing::Short if one_in => t.weekend.mul_bps(9_000),
            Standing::Short => t.weekend.mul_bps(15_000),
        }
    }
}

/// What every account holds, and the markets and clock the book needs.
pub(super) struct Spec {
    version: ProtocolVersion,
    floor_bps: i64,
    /// Share markets listed as copies of AAPL, in one group.
    shares: usize,
    /// The clock stands just before Friday's close, and oil carries its weekend margin.
    weekend: bool,
    /// Each fifth account rests a sell stop on BTC-USD at 99,000.
    stop: bool,
    /// Market, side, quantity and price.
    pub(super) holdings: Vec<(MarketId, Side, &'static str, &'static str)>,
}

/// BTC long and ETH short, $100k each, as one group, and $20k of AAPL.
pub(super) fn crypto(version: ProtocolVersion, floor_bps: i64) -> Spec {
    let holdings =
        vec![(BTC, Side::Buy, "1", "100000"), (ETH, Side::Sell, "25", "4000"), (common::AAPL, Side::Buy, "100", "200")];
    Spec { version, floor_bps, shares: 0, weekend: false, stop: false, holdings }
}

fn stops(version: ProtocolVersion) -> Spec {
    Spec { stop: true, ..crypto(version, 100) }
}

/// Oil long and gold short, $70k each, as one group.
fn commodities(version: ProtocolVersion) -> Spec {
    let holdings = vec![(OIL, Side::Buy, "1000", "70"), (GOLD, Side::Sell, "35", "2000")];
    Spec { version, floor_bps: 5_000, shares: 0, weekend: true, stop: false, holdings }
}

/// `count` shares of one group, $10k of each, alternately long and short.
fn shares(count: usize) -> Spec {
    let holdings = (0..count)
        .map(|i| (SHARES + i as MarketId, if i % 2 == 0 { Side::Buy } else { Side::Sell }, "50", "200"))
        .collect();
    Spec { version: V81, floor_bps: 5_000, shares: count, weekend: false, stop: false, holdings }
}

/// The accounts a share book holds: 100k holders of twenty positions each, about 2M positions, which the
/// fixture holds in under 4 GB.
const SHARE_ACCOUNTS: usize = COUNT;

fn friday_close() -> Millis {
    chicago(2026, 10, 23, 16, 0)
}

pub(super) fn address(index: usize) -> String {
    let mut bytes = [0xbb; 20];
    bytes[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
    serde_json::to_value(AccountId(bytes)).unwrap().as_str().unwrap().to_owned()
}

/// Lists `spec.shares` copies of AAPL as one group named by the parameters, in place of the genesis grid.
fn list_shares(h: &mut Harness, spec: &Spec) {
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let members = (0..spec.shares).map(|i| member(SHARES + i as MarketId, "1", 20)).collect();
    let grid = PortfolioGrid { groups: vec![group("shares", spec.floor_bps, members)] };
    state["params"]["portfolio_grid"] = serde_json::to_value(grid).expect("grid encoding");
    for id in SHARES..SHARES + spec.shares as MarketId {
        let mut copy = state["markets"][common::AAPL.to_string()].clone();
        copy["spec"]["id"] = json!(id);
        copy["spec"]["symbol"] = json!(format!("SHARE-{id}"));
        state["markets"][id.to_string()] = copy;
        state["close_spread_ceilings"][id.to_string()] =
            state["close_spread_ceilings"][common::AAPL.to_string()].clone();
    }
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
}

/// `count` accounts, each a copy of one client holding `spec`'s positions with its equity set by
/// `standing`, and the levels it was set against. Markets' aggregates match, house cash is ample, and
/// the close-out index is built.
pub(super) fn build(spec: &Spec, count: usize, standing: Standing) -> (Engine, Targets) {
    let oil_weekend_bps = if spec.weekend { OIL_WEEKEND_BPS } else { 0 };
    let mut h = genesis_with_grid(spec.version, pairs(spec.floor_bps), oil_weekend_bps, 0);
    if spec.shares > 0 {
        list_shares(&mut h, spec);
    }
    for (market, _, _, price) in &spec.holdings {
        h.price(*market, price);
    }
    let a = client(&mut h, 1, "100000000");
    for (market, side, qty, price) in &spec.holdings {
        only_fill(&trade(&mut h, &a, *market, *side, qty, price));
    }
    if spec.weekend {
        h.advance_to(friday_close() - 500);
        for (market, _, _, price) in &spec.holdings {
            h.price(*market, price);
        }
    }
    if spec.stop {
        let kind = OrderKind::Stop { price: f("99000") };
        let events = rest(&mut h, &a, BTC, Side::Sell, "1", kind);
        assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "{events:?}");
    }
    let (equity, bands) = h.equity_and_margin(a.account);
    let acct = &h.engine.accounts()[&a.account];
    let (_, requirement) = h.engine.equity_and_closeout_requirement(acct);
    let level_bps = h.engine.params().expect("genesis sets parameters").closeout_level_bps;
    let weekend = h.engine.weekend_requirements(a.account).get(&OIL).map_or(bands, |over| over.required);
    let targets =
        Targets { portfolio: requirement.mul_bps(level_bps), per_position: bands.mul_bps(level_bps), weekend };
    let balance = acct.balance;
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let template_key = serde_json::to_value(a.account).unwrap().as_str().unwrap().to_owned();
    let template = state["accounts"][&template_key].clone();
    let eligibility = state["eligibility"][&template_key].clone();
    let order = template["orders"].as_object().and_then(|orders| orders.values().next().cloned());
    let mut next_order = state["next_order_id"].as_u64().expect("an order id");
    state["markets"][BTC.to_string()]["resting"] = json!({});
    let mut accounts = serde_json::Map::new();
    for index in 0..count {
        let key = address(index);
        let mut value = template.clone();
        value["balance"] = json!(balance + standing.equity(index, &targets) - equity);
        value["orders"] = json!({});
        if let (Some(order), true) = (&order, index.is_multiple_of(ONE_IN)) {
            let mut order = order.clone();
            order["order_id"] = json!(next_order);
            value["orders"] = json!({ next_order.to_string(): order });
            state["markets"][BTC.to_string()]["resting"][next_order.to_string()] = json!(key);
            next_order += 1;
        }
        state["eligibility"][&key] = eligibility.clone();
        accounts.insert(key, value);
    }
    for (market, ..) in &spec.holdings {
        let held: Position = serde_json::from_value(template["positions"][market.to_string()].clone()).unwrap();
        let aggregates = &mut state["markets"][market.to_string()];
        aggregates["client_qty"] = json!(held.qty.mul_int(count as i64));
        aggregates["client_cost"] = json!(held.cost().mul_int(count as i64));
    }
    state["accounts"] = accounts.into();
    state["next_order_id"] = json!(next_order);
    state["markouts"] = json!([]);
    state["house_cash"] = json!("100000000000");
    let engine = load_state(state);
    (engine, targets)
}

fn load_state(state: serde_json::Value) -> Engine {
    let mut engine: Engine = serde_json::from_value(state).expect("benchmark fixture");
    engine.restore_indexes();
    engine
}

/// A fresh copy of a fixture `build` serialised, with its indexes built.
fn load(bytes: &[u8]) -> Engine {
    let mut engine: Engine = serde_json::from_slice(bytes).expect("benchmark fixture");
    engine.restore_indexes();
    engine
}

fn prices(market: MarketId, mid: &str, ts: i64) -> Command {
    let quotes = (0..2).map(|v| SourceQuote { source: format!("venue{v}"), mid: f(mid), ts, contract: None }).collect();
    Command::Prices { market, quotes }
}

/// Quotes every market `spec` holds at its opening price, at `ts`.
fn quote_all(engine: &mut Engine, spec: &Spec, ts: i64) {
    for (market, _, _, price) in &spec.holdings {
        apply_events(engine, ts, prices(*market, price, ts));
    }
}

/// `SAMPLES` daily rollovers of `engine`, each after quoting `spec`'s markets at their opening prices and
/// none closing an account out: prints and returns `(p50, p99)` in microseconds.
fn rollovers(name: &str, engine: &mut Engine, spec: &Spec) -> (u128, u128) {
    let mut times = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let due = engine.next_rollover().expect("a rollover is scheduled");
        quote_all(engine, spec, due - 1);
        let began = Instant::now();
        let events = apply_events(engine, due, Command::Tick);
        times.push(began.elapsed().as_micros());
        assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "no account closes out");
    }
    let (p50, p99) = percentiles(&mut times);
    println!("LATENCY {name} accounts={COUNT} samples={SAMPLES} p50_us={p50} p99_us={p99}");
    (p50, p99)
}

/// Prints `name`'s `p99` against #904's bar for one command, `bar_us`.
fn bar(name: &str, p99: u128, bar_us: u128) {
    let verdict = if p99 <= bar_us { "within" } else { "OVER" };
    println!("LATENCY bar {name} p99_us={p99} bar_us={bar_us} {verdict}");
}

/// `(p50, p99)` of `times`, sorted.
fn percentiles(times: &mut [u128]) -> (u128, u128) {
    times.sort_unstable();
    (times[times.len().div_ceil(2) - 1], times[(times.len() * 99).div_ceil(100) - 1])
}

/// `SAMPLES` prices of `market` alternating between `up` and `base`, none closing an account out:
/// `(p50, p99)` in microseconds.
fn detect(engine: &mut Engine, market: MarketId, base: &str, up: &str) -> (u128, u128) {
    let start = engine.now();
    apply(engine, start, Command::Tick);
    let mut times = Vec::with_capacity(SAMPLES);
    for sample in 0..SAMPLES {
        let ts = start + sample as i64 + 1;
        let command = prices(market, if sample % 2 == 0 { up } else { base }, ts);
        let began = Instant::now();
        let events = apply_events(engine, ts, command);
        times.push(began.elapsed().as_micros());
        assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "no account closes out");
    }
    percentiles(&mut times)
}

/// `EXEC_SAMPLES` runs of `command` on a fresh load of `bytes`, each given the clock a command may use:
/// prints and returns the slowest in microseconds with the close-outs the last announced.
fn once(name: &str, bytes: &[u8], command: impl Fn(&mut Engine, i64) -> Vec<Event>) -> (u128, usize) {
    let (mut times, mut closed, mut fills) = (Vec::new(), 0, 0);
    for _ in 0..EXEC_SAMPLES {
        let mut engine = load(bytes);
        let ts = engine.now() + 1;
        apply(&mut engine, ts, Command::Tick);
        let began = Instant::now();
        let events = command(&mut engine, ts + 1);
        times.push(began.elapsed().as_micros());
        closed = events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).count();
        fills = events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
    }
    let (p50, p99) = percentiles(&mut times);
    println!("LATENCY {name} samples={EXEC_SAMPLES} closed_out={closed} fills={fills} p50_us={p50} p99_us={p99}");
    (p99, closed)
}
