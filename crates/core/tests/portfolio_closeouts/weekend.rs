//! A weekend gap's start under portfolio margin (#913): a market with no weekend margin keeps no gap,
//! grouped or not, so a hedged group of such markets is neither closed out nor held to more over a
//! weekend; at the gap of a market with one, the close-out reads the portfolio requirement in full with
//! the weekend margins in its bands, each group keeping its credit, valued and filled at the marks, and
//! closes only what the weekend close-out can fill.

use super::*;
use common::weekend::{OIL_WEEKEND_BPS, chicago};
use engine_core::command::{AdminCommand, Command};
use engine_core::types::Millis;
use engine_core::weekend::WeekendGap;

/// Friday 23 October 2026's 16:00 Globex close in Chicago, which starts that weekend's gap.
fn friday_close() -> Millis {
    chicago(2026, 10, 23, 16, 0)
}

/// Sunday 25 October 2026's 17:00 Globex open in Chicago, which ends it.
fn sunday_open() -> Millis {
    chicago(2026, 10, 25, 17, 0)
}

/// Oil at `oil_weekend_bps` and gold at `gold_weekend_bps` under `grid`, and a holder of $700k of oil long
/// against $700k of gold short, opened with $70k, after oil falls to `oil`.
fn hedged_commodities(
    grid: PortfolioGrid,
    oil_weekend_bps: i64,
    gold_weekend_bps: i64,
    oil: &str,
) -> (Harness, Client) {
    let mut h = genesis_with_grid(V81, grid, oil_weekend_bps, gold_weekend_bps);
    let a = client(&mut h, 1, "70000");
    h.price(OIL, "70");
    h.price(GOLD, "2000");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "350", "2000"));
    assert!(fills(&h.price(OIL, oil)).is_empty());
    (h, a)
}

/// The first command after Friday's close, Saturday morning, with oil at `oil` and gold at 2,000 until then.
fn past_the_close(h: &mut Harness, oil: &str) -> Vec<Event> {
    h.advance_to(friday_close() - 500);
    h.price(OIL, oil);
    h.price(GOLD, "2000");
    h.advance_to(friday_close() + 20 * 60 * 60 * 1000);
    h.cmd(Command::Tick)
}

/// What `client` must have over the oil gap that starts at Friday's close.
fn over_the_gap(h: &Harness, client: &Client) -> Fixed {
    let required = h.engine.weekend_requirements(client.account);
    assert_eq!(required.keys().copied().collect::<Vec<_>>(), vec![OIL], "only oil keeps a gap");
    required[&OIL].required
}

/// `client`'s oil and gold quantities.
fn held(h: &Harness, client: &Client) -> (Fixed, Fixed) {
    let acct = &h.engine.accounts()[&client.account];
    (acct.position(OIL).qty, acct.position(GOLD).qty)
}

#[test]
fn a_group_with_no_weekend_margin_keeps_no_gap_and_holds_over_the_weekend() {
    // About $9.7k of equity against a portfolio requirement of about $9.4k and $13.4k of bands.
    let (mut h, a) = hedged_commodities(pairs(5_000), 0, 0, "64");
    let (equity, level) = standing(&h, &a);
    let (_, bands) = h.equity_and_margin(a.account);
    assert!(equity > level && equity < bands, "{equity} between {level} and {bands}");
    for id in [OIL, GOLD] {
        assert_eq!(h.engine.markets()[&id].weekend, None, "market {id}");
    }
    assert!(h.engine.weekend_requirements(a.account).is_empty());
    let events = past_the_close(&mut h, "64");
    assert!(liquidations(&events).is_empty(), "{events:?}");
    assert_eq!(held(&h, &a), (f("10000"), f("-350")));
}

#[test]
fn activating_v81_gives_a_grouped_market_with_no_weekend_margin_no_gap() {
    // Within the lead before Friday's close, gold keeps none, and oil the one `V60` set it.
    let mut h = genesis_with_grid(ProtocolVersion::V79, pairs(5_000), OIL_WEEKEND_BPS, 0);
    h.advance_to(friday_close() - 60 * 60 * 1000);
    h.admin(AdminCommand::ActivateProtocol { version: V81 });
    assert_eq!(h.engine.markets()[&GOLD].weekend, None);
    let gap = WeekendGap { close: friday_close(), reopen: sunday_open(), closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(gap));
}

#[test]
fn at_a_weekend_margins_gap_a_hedged_group_keeps_its_credit() {
    // Oil at 67 charges 7.5% over its gap, $50,250, and gold its $7,000. The matched bands are gold's and
    // oil's $47,250 of weekend bands, against a $9,310 grid charge, so the credit is half of them,
    // $27,125, and the requirement $30,125: below the holder's equity, which the bands alone are above.
    let (mut h, a) = hedged_commodities(pairs(5_000), OIL_WEEKEND_BPS, 0, "67");
    let (mut alone, b) = hedged_commodities(PortfolioGrid { groups: vec![] }, OIL_WEEKEND_BPS, 0, "67");
    assert_eq!(h.engine.markets()[&GOLD].weekend, None);
    let grouped = over_the_gap(&h, &a);
    let per_position = over_the_gap(&alone, &b);
    assert_eq!((grouped, per_position), (f("30125"), f("57250")));
    let equity = standing(&h, &a).0;
    assert!(grouped < equity && equity < per_position, "{equity} between {grouped} and {per_position}");
    let events = past_the_close(&mut h, "67");
    assert!(liquidations(&events).is_empty(), "{events:?}");
    assert_eq!(held(&h, &a), (f("10000"), f("-350")));
    // With no group, the same book is closed out at the gap's start.
    let events = past_the_close(&mut alone, "67");
    assert_eq!(liquidations(&events), vec![OIL], "{events:?}");
}

#[test]
fn below_it_the_close_out_closes_the_gapped_market_at_its_mark() {
    // Oil at 64: $48,000 of weekend bands against the same $27,125 credit, $27,875, far above the equity.
    // Only oil's gap starts, so only oil closes, at its mark, and gold stays.
    let (mut h, a) = hedged_commodities(pairs(5_000), OIL_WEEKEND_BPS, 0, "64");
    let required = over_the_gap(&h, &a);
    assert_eq!(required, f("27875"));
    let events = past_the_close(&mut h, "64");
    assert_eq!(liquidations(&events), vec![OIL], "{events:?}");
    for fill in fills(&events) {
        assert_eq!(fill.index, h.engine.markets()[&fill.market].mark.unwrap(), "filled at the mark");
    }
    let Some(Event::CloseOut { margin, .. }) = events.iter().find(|e| matches!(e, Event::CloseOut { .. })) else {
        panic!("{events:?}")
    };
    assert_eq!(*margin, required);
    assert_eq!(held(&h, &a), (Fixed::ZERO, f("-350")));
}

#[test]
fn a_member_blocked_by_a_failed_split_waits_and_the_rest_close_alone() {
    let (mut h, a) = hedged_commodities(pairs(5_000), OIL_WEEKEND_BPS, OIL_WEEKEND_BPS, "64");
    h.advance_to(friday_close() - 500);
    h.price(OIL, "64");
    h.price(GOLD, "2000");
    // A failed split blocks its market, which needs a share price that rounds to zero: set the flag the
    // gap's close-out reads directly, on gold.
    let mut state = serde_json::to_value(&h.engine).expect("the engine encodes");
    state["markets"][GOLD.to_string()]["corporate_action_blocked"] = serde_json::json!(true);
    h.engine = serde_json::from_value(state).expect("the engine decodes");
    h.advance_to(friday_close() + 20 * 60 * 60 * 1000);
    let events = h.cmd(Command::Tick);
    assert_eq!(liquidations(&events), vec![OIL], "{events:?}");
    assert_eq!(held(&h, &a), (Fixed::ZERO, f("-350")));
}

#[test]
fn a_hedged_group_with_both_markets_on_a_weekend_margin_closes_whole_at_the_gaps_start() {
    // Oil and gold each carry 7.5% over their gaps, which start together at Friday's close. $700k of oil
    // long against $700k of gold short, opened with $40k, is under the portfolio requirement over the gap.
    let mut h = genesis_with_grid(V81, pairs(5_000), OIL_WEEKEND_BPS, OIL_WEEKEND_BPS);
    let a = client(&mut h, 1, "40000");
    h.price(OIL, "70");
    h.price(GOLD, "2000");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "350", "2000"));
    let required = h.engine.weekend_requirements(a.account);
    assert_eq!(required.keys().copied().collect::<Vec<_>>(), vec![GOLD, OIL], "both keep a gap");
    let requirement = required[&OIL].required;
    assert_eq!(required[&GOLD].required, requirement);
    let equity = standing(&h, &a).0;
    assert!(equity < requirement, "{equity} under {requirement}");
    let events = past_the_close(&mut h, "70");
    // One close-out, announced once at the weekend requirement, closes both legs, gold before oil.
    assert_eq!(liquidations(&events), vec![GOLD, OIL], "{events:?}");
    let announced: Vec<_> = events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).collect();
    assert!(matches!(announced[..], [Event::CloseOut { margin, .. }] if *margin == requirement), "{events:?}");
    for fill in fills(&events) {
        assert_eq!(fill.index, h.engine.markets()[&fill.market].mark.unwrap(), "filled at the mark");
        assert_eq!(fill.position, Fixed::ZERO);
    }
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}

#[test]
fn a_groups_later_member_is_scored_at_the_nav_the_gap_starts_earlier_fills_leave() {
    // Oil and gold are one group, both with a weekend margin, and Nikkei is alone: all three gaps start at
    // Friday's close. BTC's rise puts the house about $50 behind on the day, so level 2, whose loss
    // threshold is zero, is what a close-out pays at the NAV now, in both members' markets (each stored at
    // level 0). Closing gold gives the house its spread and puts it ahead, so oil's close pays level 0:
    // scored at the NAV gold's fill leaves, the pair restores the requirement and closes, gold before oil.
    // Scored at the NAV before the step, oil's close would pay level 2, the pair would not restore it, and
    // Nikkei, which does, would close alone.
    let mut h = commodity_gap_book("16600");
    let events = at_the_gap(&mut h);
    let closed: Vec<MarketId> =
        fills(&events).into_iter().filter(|fill| fill.liquidation).map(|fill| fill.market).collect();
    assert_eq!(closed, vec![GOLD, OIL], "{events:?}");
    let spreads: Vec<i64> = fills(&events).iter().map(|fill| fill.spread_bps).collect();
    assert!(spreads[0] > spreads[1], "gold pays level 2, oil level 0: {spreads:?}");
}

/// A holder of the oil and gold group, $200k each way, and $1m of Nikkei, on `funds`, and a second holder
/// whose BTC the house is behind on by about $50 once oil, gold and Nikkei have gone stale, 2 s before
/// Friday's close.
fn commodity_gap_book(funds: &str) -> Harness {
    use common::USDJPY;
    use common::house_loss::{level_two, loss};
    use common::portfolio::{group, member, params_with};
    use common::weekend::{NIKKEI, genesis_with_params, globex};
    use engine_core::params::{StressLevel, StressTrigger};
    let grid =
        PortfolioGrid { groups: vec![group("commodities", 100, vec![member(OIL, "1", 20), member(GOLD, "0.9", 20)])] };
    let mut params = params_with(grid);
    let trigger = Some(StressTrigger { house_loss: Some(Fixed::ZERO), ..StressTrigger::default() });
    params.stress_levels.push(StressLevel { trigger, ..level_two() });
    let mut h = genesis_with_params(V81, globex(vec![]), OIL_WEEKEND_BPS, OIL_WEEKEND_BPS, params);
    let a = client(&mut h, 1, funds);
    let b = client(&mut h, 2, "1000000");
    for (market, price) in [(OIL, "70"), (GOLD, "2000"), (USDJPY, "150"), (NIKKEI, "38000"), (BTC, "100000")] {
        h.price(market, price);
    }
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "2850", "70"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "100", "2000"));
    only_fill(&trade(&mut h, &a, NIKKEI, Side::Buy, "4000", "38000"));
    only_fill(&trade(&mut h, &b, BTC, Side::Buy, "5", "100000"));
    h.advance_to(friday_close() - 5_000);
    for (market, price) in [(OIL, "70"), (GOLD, "2000"), (USDJPY, "150"), (NIKKEI, "38000")] {
        h.price(market, price);
    }
    let behind = loss(&h);
    h.advance_to(friday_close() - 2_000);
    let btc = f("100000") + (f("50") - behind).div_int(5);
    h.price(BTC, &btc.to_string());
    h
}

/// The first command after Friday's close, which starts every gap.
fn at_the_gap(h: &mut Harness) -> Vec<Event> {
    h.advance_to(friday_close() + 1_000);
    h.cmd(Command::Tick)
}
