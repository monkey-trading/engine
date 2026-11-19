//! `V81`'s close-outs under portfolio margin, through `Engine::apply` (#913, D5, D6, D10): a hedged
//! book is closed out only below the close-out level of its portfolio requirement; a close-out closes,
//! step by step, the candidate that restores the level with the least notional, else the one gaining
//! the most per dollar closed, keeping each offset group whole while all its held members can trade
//! and closing the members that can alone while one cannot, even when every candidate worsens the
//! slack; a failed fill ends the account's close-out mid-group; and every close-out ends within the
//! account's position count. A log at `V81` whose parameters name no grid keeps the rule before it.
//! The check after a command's fills is in `after_fills`, the weekend gap's start in `weekend`, and the
//! close-out index's exactness under the portfolio requirement in `index`.

mod common;

use common::close_spread::trade;
use common::events::{fills, only_fill};
use common::portfolio::{genesis_with_grid, pairs};
use common::weekend::{GOLD, OIL, client};
use common::{AAPL, BTC, Client, ETH, Harness, f};
use engine_core::Fixed;
use engine_core::event::{Event, Fill};
use engine_core::portfolio::PortfolioGrid;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};

const V81: ProtocolVersion = ProtocolVersion::V81;

#[test]
fn a_hedged_pair_stays_open_between_its_portfolio_and_per_position_levels() {
    // $1M of BTC long against $1M of ETH short. ETH rises 5% and BTC falls 8%: equity is about $79k,
    // below half the $197k of bands, above half the portfolio requirement of about $114k.
    let (h, a, events) = pair_after_basis_move(pairs(5_000));
    assert!(fills(&events).is_empty(), "{events:?}");
    let (equity, margin) = h.equity_and_margin(a.account);
    assert!(equity < margin.mul_bps(5_000), "below the per-position level: {equity} against {margin}");
    let (_, level) = standing(&h, &a);
    assert!(equity > level, "above the portfolio level: {equity} against {level}");
    // At `V81` with no grid, the per-position rule closes the largest loser first, BTC.
    let (h, a, events) = pair_after_basis_move(PortfolioGrid::default());
    let closed: Vec<&Fill> = fills(&events).into_iter().filter(|fill| fill.liquidation).collect();
    assert_eq!(closed.first().map(|fill| fill.market), Some(BTC), "{events:?}");
    assert!(h.engine.accounts()[&a.account].positions.len() < 2);
}

#[test]
fn the_position_restoring_the_level_with_the_least_notional_closes_alone() {
    // ETH up 17%: the account is about $13k short of half its $167k portfolio requirement. Closing $200k
    // of AAPL releases half its $40k of bands and restores the level; so would the $2.17M pair, so the
    // least notional closes. The rule before closed the largest loser, ETH, first.
    let (mut h, a) = pair_and_shares();
    let events = h.price(ETH, "4680");
    let closed = liquidations(&events);
    assert_eq!(closed, vec![AAPL], "{events:?}");
    assert!(matches!(events.iter().find(|e| matches!(e, Event::CloseOut { .. })), Some(Event::CloseOut { .. })));
    let (equity, level) = standing(&h, &a);
    assert!(equity >= level, "restored: {equity} against {level}");
    assert_eq!(h.engine.accounts()[&a.account].positions.keys().copied().collect::<Vec<_>>(), vec![BTC, ETH]);
}

#[test]
fn with_none_restoring_the_most_per_dollar_closes_first_and_a_group_closes_whole() {
    // ETH up 30%: the account is about $150k short. Neither AAPL, gaining about $20k on $200k, nor the
    // pair, gaining about $70k on $2.3M, restores the level. The pair gains more, but AAPL gains more
    // per dollar, so it closes first; then the pair closes whole, BTC before ETH.
    let (mut h, a) = pair_and_shares();
    let events = h.price(ETH, "5200");
    assert_eq!(liquidations(&events), vec![AAPL, BTC, ETH], "{events:?}");
    let announced = events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).count();
    assert_eq!(announced, 1, "a close-out is announced once: {events:?}");
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}

#[test]
fn a_candidate_is_scored_at_the_stress_level_its_fill_pays() {
    // ETH up 22% takes its stress to level 1 when its next fill re-evaluates it, so a close of ETH pays
    // about 7bp where it paid 1bp. The account is left about $20.6k of equity: closing the pair restores
    // the level only if both legs cost under about $570, which they do at level 0 but not at level 1.
    // Scored at the level the fill pays, neither candidate restores, so AAPL, gaining the most per dollar,
    // closes first; scored at the level as it stood, the pair would close first, and then AAPL too.
    let (mut h, a) = pair_and_shares();
    let acct = &h.engine.accounts()[&a.account];
    let (equity, _) = h.engine.equity_and_closeout_requirement(acct);
    let eth = f("4000") + (equity - f("20570")).div_int(250);
    let events = h.price(ETH, &eth.to_string());
    assert!(events.iter().any(|e| matches!(e, Event::StressChanged { market: ETH, level: 1 })), "{events:?}");
    assert_eq!(liquidations(&events), vec![AAPL, BTC, ETH], "{events:?}");
}

#[test]
fn a_groups_later_member_is_scored_at_the_nav_its_earlier_fills_leave() {
    // Stress level 1 is raised only by utilisation, with no cooldown, and only ETH's cap is a share of the
    // house's NAV, all of it. The NAV stands $50 under ETH's notional, which it follows one for one while
    // the account's short is ETH's only position, so ETH is at level 1 until a fill gives the house $50.
    // ETH rises until closing the pair restores the level only if ETH pays level 0's spread, as above.
    // BTC closes first and gives the house its spread, so ETH's close pays level 0: scored at the NAV
    // BTC's fill leaves, the pair restores the level alone and AAPL stays open.
    let (mut h, a) = pair_and_shares();
    eth_capped_at_the_house_nav(&mut h, &a, f("-50"), |_| {});
    let acct = &h.engine.accounts()[&a.account];
    let (equity, _) = h.engine.equity_and_closeout_requirement(acct);
    let eth = f("4000") + (equity - f("20570")).div_int(250);
    let events = h.price(ETH, &eth.to_string());
    assert_eq!(liquidations(&events), vec![BTC, ETH], "{events:?}");
    assert_eq!(h.engine.accounts()[&a.account].position(AAPL).qty, f("1000"));
}

#[test]
fn a_member_that_cannot_trade_waits_and_the_others_close_alone_even_worsening_the_slack() {
    // At a 1% floor share the pair needs about $32k. ETH's quotes go stale, and BTC falls to 80,000:
    // ETH counts at its last mark, 4,000. BTC is the only candidate, and closing it alone gives up the
    // group's credit, so it worsens the slack; it still closes, and ETH waits.
    let mut h = genesis_with_grid(V81, pairs(100), 750, 0);
    let a = client(&mut h, 1, "210000");
    open_pair(&mut h, &a);
    h.advance(2_000);
    let events = h.price(BTC, "80000");
    assert_eq!(liquidations(&events), vec![BTC], "{events:?}");
    let (equity, level) = standing(&h, &a);
    assert!(equity < level, "still below, with ETH unable to trade: {equity} against {level}");
    assert_eq!(h.engine.accounts()[&a.account].position(ETH).qty, f("-250"));
    // ETH's next price closes it.
    let events = h.price(ETH, "4000");
    assert_eq!(liquidations(&events), vec![ETH], "{events:?}");
}

#[test]
fn a_member_whose_close_does_not_price_at_its_projected_level_waits() {
    // Every pure close clamps to its market's close-spread ceiling, so this takes ETH's ceiling away and
    // raises stress level 1's spread to the whole price. ETH rises 17%: scoring projects ETH's level 1,
    // where its close does not price, so ETH waits, BTC is the only candidate and closes alone, and
    // nothing fails in execution.
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    let a = client(&mut h, 1, "210000");
    open_pair(&mut h, &a);
    let mut state = serde_json::to_value(&h.engine).expect("the engine encodes");
    state["close_spread_ceilings"].as_object_mut().expect("installed").remove(&ETH.to_string());
    state["params"]["stress_levels"][1]["min_spread_bps"] = serde_json::json!(10_000);
    h.engine = serde_json::from_value(state).expect("the engine decodes");
    let events = h.price(ETH, "4680");
    assert_eq!(liquidations(&events), vec![BTC], "{events:?}");
    assert_eq!(h.engine.accounts()[&a.account].position(ETH).qty, f("-250"));
}

#[test]
fn a_failed_fill_ends_the_close_out_mid_group() {
    // The house's NAV stands $50 over ETH's notional, its cap, so ETH is at level 0, and BTC's mark
    // $1,000 under its index: the NAV is read at the mark, and the account's equity and fills are at the
    // index. Scoring takes the NAV BTC's fill leaves from the index, about $10,000 more than it leaves,
    // so it scores ETH at level 0 and the pair as restoring the level; ETH's fill pays level 1, whose
    // spread reaches the price with no ceiling to clamp it, and is refused. The close-out ends there: ETH
    // stays open, and AAPL, the next candidate now the pair has not restored the level, stays open too.
    let (mut h, a) = pair_and_shares();
    let (equity, _) = h.engine.equity_and_closeout_requirement(&h.engine.accounts()[&a.account]);
    eth_capped_at_the_house_nav(&mut h, &a, f("50"), |state| {
        state["close_spread_ceilings"].as_object_mut().expect("installed").remove(&ETH.to_string());
        state["params"]["stress_levels"][1]["min_spread_bps"] = serde_json::json!(10_000);
        state["markets"][BTC.to_string()]["mark"] = serde_json::json!("99000");
    });
    let eth = f("4000") + (equity - f("20570")).div_int(250);
    let events = h.price(ETH, &eth.to_string());
    assert_eq!(liquidations(&events), vec![BTC], "{events:?}");
    let announced = events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).count();
    assert_eq!(announced, 1, "{events:?}");
    let held = &h.engine.accounts()[&a.account];
    assert_eq!(held.position(ETH).qty, f("-250"));
    assert_eq!(held.position(AAPL).qty, f("1000"));
}

#[test]
fn every_close_out_ends_within_the_accounts_position_count() {
    let config = Config { cases: 24, rng_seed: RngSeed::Fixed(0x0639), failure_persistence: None, ..Config::default() };
    let moves = proptest::collection::vec((0usize..5, -3_000i64..3_000), 1..6);
    let books = proptest::collection::vec((0usize..5, any::<bool>(), 1i64..8), 2..6);
    TestRunner::new(config)
        .run(&(books, moves, 1i64..20), |(book, moves, floor)| {
            let mut h = genesis_with_grid(V81, pairs(floor * 500), 750, 0);
            reprice(&mut h);
            let a = client(&mut h, 1, "400000");
            for (market, buy, hundred_k) in book {
                let (id, price) = MARKETS[market];
                let side = if buy { Side::Buy } else { Side::Sell };
                let qty =
                    Fixed::from_int(hundred_k * 100_000).div_rounded(f(price), engine_core::fixed::Rounding::Down);
                trade(&mut h, &a, id, side, &qty.to_string(), price);
            }
            for (market, bps) in moves {
                let (id, _) = MARKETS[market];
                let held = h.engine.accounts()[&a.account].positions.len();
                let mark = h.engine.markets()[&id].mark.unwrap_or(f(MARKETS[market].1));
                let moved = mark + mark.mul_bps(bps);
                let events = h.price(id, &moved.to_string());
                let closed = liquidations(&events);
                prop_assert!(closed.len() <= held, "{} fills for {held} positions: {events:?}", closed.len());
                if !closed.is_empty() {
                    // Every market can trade, so a close-out stops at the level or with the account flat.
                    let (equity, level) = standing(&h, &a);
                    let left = h.engine.accounts()[&a.account].positions.len();
                    prop_assert!(equity >= level || left == 0, "{equity} against {level} with {left} left: {events:?}");
                }
            }
            Ok(())
        })
        .unwrap();
}

// ── Helpers ──

/// The markets the property books hold, each with its opening price.
const MARKETS: [(MarketId, &str); 5] = [(BTC, "100000"), (ETH, "4000"), (AAPL, "200"), (OIL, "70"), (GOLD, "2000")];

/// Prices every market `MARKETS` names at its opening price.
fn reprice(h: &mut Harness) {
    for (id, price) in MARKETS {
        h.price(id, price);
    }
}

/// Rewrites `h`'s engine so that ETH's cap is all of the house's NAV, which stands `margin` over ETH's
/// notional and follows it one for one while `client`'s short is ETH's only position: ETH is at stress
/// level 1 while the NAV is under its notional, with no cooldown. `edit` changes the encoded state first.
fn eth_capped_at_the_house_nav(
    h: &mut Harness,
    client: &Client,
    margin: Fixed,
    edit: impl FnOnce(&mut serde_json::Value),
) {
    let eth_notional = |h: &Harness| {
        let held = h.engine.accounts()[&client.account].position(ETH).qty.abs();
        held.mul(h.engine.markets()[&ETH].mark.expect("ETH is priced"))
    };
    let mut state = serde_json::to_value(&h.engine).expect("the engine encodes");
    edit(&mut state);
    h.engine = serde_json::from_value(state.clone()).expect("the engine decodes");
    let lower_by = h.engine.house_nav() - (eth_notional(h) + margin);
    let cash: Fixed = serde_json::from_value(state["house_cash"].clone()).expect("house cash");
    state["house_cash"] = serde_json::to_value(cash - lower_by).expect("house cash");
    for id in [BTC, AAPL] {
        state["markets"][id.to_string()]["spec"]["house_cap_floor"] = serde_json::json!("1000000000");
    }
    state["markets"][ETH.to_string()]["spec"]["house_cap_bps"] = serde_json::json!(10_000);
    state["markets"][ETH.to_string()]["spec"]["house_cap_floor"] = serde_json::json!("0");
    state["params"]["stress_cooldown_ms"] = serde_json::json!(0);
    state["params"]["stress_levels"][1]["trigger"] = serde_json::json!({ "utilisation_bps": 10_000 });
    h.engine = serde_json::from_value(state).expect("the engine decodes");
    assert_eq!(h.engine.house_nav(), eth_notional(h) + margin);
}

/// `client`'s equity and the close-out level of the requirement its close-out reads, at the marks.
fn standing(h: &Harness, client: &Client) -> (Fixed, Fixed) {
    let acct = &h.engine.accounts()[&client.account];
    let (equity, requirement) = h.engine.equity_and_closeout_requirement(acct);
    (equity, requirement.mul_bps(5_000))
}

/// The markets the close-out fills among `events` close, in order.
fn liquidations(events: &[Event]) -> Vec<MarketId> {
    fills(events).into_iter().filter(|fill| fill.liquidation).map(|fill| fill.market).collect()
}

/// Opens $1M of BTC long and $1M of ETH short for `client`, at 100,000 and 4,000.
fn open_pair(h: &mut Harness, client: &Client) {
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    only_fill(&trade(h, client, BTC, Side::Buy, "10", "100000"));
    only_fill(&trade(h, client, ETH, Side::Sell, "250", "4000"));
}

/// A pair holder under `grid`, holding $210k, after ETH rises 5% and BTC falls 8%, with the events of
/// BTC's price.
fn pair_after_basis_move(grid: PortfolioGrid) -> (Harness, Client, Vec<Event>) {
    let mut h = genesis_with_grid(V81, grid, 750, 0);
    let a = client(&mut h, 1, "210000");
    open_pair(&mut h, &a);
    h.price(ETH, "4200");
    let events = h.price(BTC, "92000");
    (h, a, events)
}

/// A holder of the pair and $200k of AAPL long, on $241k, under a 50% floor share.
fn pair_and_shares() -> (Harness, Client) {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    let a = client(&mut h, 1, "241000");
    open_pair(&mut h, &a);
    h.price(AAPL, "200");
    only_fill(&trade(&mut h, &a, AAPL, Side::Buy, "1000", "200"));
    (h, a)
}

#[path = "portfolio_closeouts/after_fills.rs"]
mod after_fills;
#[path = "portfolio_closeouts/index.rs"]
mod index;
#[path = "portfolio_closeouts/weekend.rs"]
mod weekend;
