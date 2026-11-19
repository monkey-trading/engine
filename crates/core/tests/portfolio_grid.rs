//! `V81`'s offset grid in the parameters, through `Engine::apply`: it must fit the listings when it is
//! set, and a listing must keep each group's listed members on one asset class and schedule, quoted in
//! USD; `RelistMarkets` moves a group's calendar, all or nothing; the groups, members, loadings and
//! bounds are fixed at launch, a notice moves only the shocks and floor shares, up within their bounds,
//! and a log whose launch grid is empty keeps none; a grid named before `V81` is taken up by its
//! activation; and a grouped market with no weekend margin keeps no weekend gap, and none of `V60`'s
//! calendar checks, as before `V81`.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::portfolio::{genesis_with_grid, grid_in_force, grid_notice, group, member, pairs, params_with, with_grid};
use common::spreads::{applied, refused, tenths};
use common::weekend::{GOLD, OIL, chicago, client, commodity, globex, period};
use common::{BTC, CHAIN_ID, Harness, USDJPY, VAULT, f, market, operator};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::portfolio::PortfolioGrid;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Side};

const V81: ProtocolVersion = ProtocolVersion::V81;
/// The version before `V81`.
const BEFORE: ProtocolVersion = ProtocolVersion::V79;
/// A commodity the harness does not list, which a grid may name before it is.
const SILVER: MarketId = 31;

/// An edit to a grid.
type Change = fn(&mut PortfolioGrid);

#[test]
fn a_grid_must_fit_the_listings_when_it_is_set() {
    // Before `V81` the grid decides nothing and changes freely, but must fit the markets listed.
    let mut h = genesis_with_grid(BEFORE, PortfolioGrid::default(), 750, 0);
    let mixed = PortfolioGrid { groups: vec![group("mixed", 5_000, vec![member(BTC, "1", 20), member(OIL, "1", 20)])] };
    let refusal = refused(&h.operate(AdminCommand::SetParams(with_grid(&h, mixed)))).to_owned();
    assert!(refusal.contains("markets 1 and 32 differ in asset class or schedule"), "{refusal}");
    // A member not yet listed counts for nothing.
    let silver =
        PortfolioGrid { groups: vec![group("metals", 5_000, vec![member(GOLD, "1", 20), member(SILVER, "1", 20)])] };
    assert!(applied(&h.operate(AdminCommand::SetParams(with_grid(&h, silver.clone())))));
    assert_eq!(grid_in_force(&h), silver);
}

#[test]
fn a_listing_that_would_split_a_group_is_refused() {
    let metals = PortfolioGrid {
        groups: vec![group(
            "metals",
            5_000,
            vec![member(OIL, "1", 20), member(GOLD, "1", 20), member(SILVER, "1", 20)],
        )],
    };
    let mut h = genesis_with_grid(V81, metals.clone(), 750, 0);
    // Gold onto other hours, with a holiday added.
    let christmas = globex(vec![period("2026-12-24 00:00", "2026-12-25 00:00")]);
    let gold = MarketSpec { schedule: christmas, ..h.engine.markets()[&GOLD].spec.clone() };
    let refusal = refused(&h.operate(AdminCommand::UpsertMarket(gold))).to_owned();
    assert!(refusal.contains("markets 32 and 30 differ in asset class or schedule"), "{refusal}");
    // Silver first listed in another class, or quoted outside USD, as a genesis lists it before any
    // ceiling is adopted.
    let mut g = Harness::empty();
    g.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    g.admin(AdminCommand::SetOperator(operator()));
    g.admin(AdminCommand::ActivateProtocol { version: V81 });
    g.admin(AdminCommand::SetParams(params_with(metals)));
    // From `V79` every listing names a minimum level distance.
    let distanced = |spec: MarketSpec| MarketSpec { min_level_distance_bps: Some(25), ..spec };
    g.admin(AdminCommand::UpsertMarket(distanced(commodity(OIL, "WTI", globex(vec![]), 750))));
    let usdjpy = MarketSpec {
        quote_currency: QuoteCurrency::Jpy { fx_market: USDJPY },
        spread_tiers: tenths(&[(None, 10)]),
        markout_floor_bps: Some(f("1")),
        ..market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200)
    };
    g.admin(AdminCommand::UpsertMarket(distanced(usdjpy)));
    let silver = distanced(commodity(SILVER, "XAG-USD", globex(vec![]), 0));
    let listing = |spec: MarketSpec, g: &mut Harness| g.cmd(Command::Admin(AdminCommand::UpsertMarket(spec)));
    let equity = MarketSpec { asset_class: AssetClass::Equity, ..silver.clone() };
    let refusal = refused(&listing(equity, &mut g)).to_owned();
    assert!(refusal.contains("markets 32 and 31 differ in asset class or schedule"), "{refusal}");
    let yen = MarketSpec { quote_currency: QuoteCurrency::Jpy { fx_market: USDJPY }, ..silver.clone() };
    let refusal = refused(&listing(yen, &mut g)).to_owned();
    assert!(refusal.contains("market 31 is not quoted in USD"), "{refusal}");
    // On the group's own class, hours and currency it lists.
    assert!(applied(&listing(silver, &mut g)));
}

#[test]
fn relisting_every_member_at_once_moves_a_groups_calendar() {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    // Thanksgiving, five weeks out.
    let holiday = globex(vec![period("2026-11-26 00:00", "2026-11-27 00:00")]);
    let relisted =
        |id: MarketId, h: &Harness| MarketSpec { schedule: holiday.clone(), ..h.engine.markets()[&id].spec.clone() };
    // One member alone would leave the group on two schedules.
    let refusal = refused(&h.operate(AdminCommand::UpsertMarket(relisted(GOLD, &h)))).to_owned();
    assert!(refusal.contains("markets 32 and 30 differ in asset class or schedule"), "{refusal}");
    let both = AdminCommand::RelistMarkets { specs: vec![relisted(OIL, &h), relisted(GOLD, &h)] };
    assert!(applied(&h.operate(both)));
    for id in [OIL, GOLD] {
        assert_eq!(h.engine.markets()[&id].spec.schedule, holiday, "market {id} takes the holiday");
    }
    let closed = chicago(2026, 11, 26, 12, 0);
    assert!(!h.engine.markets()[&GOLD].spec.schedule.is_open(closed));
}

#[test]
fn a_relisting_of_several_markets_applies_all_or_nothing() {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    let holiday = globex(vec![period("2026-11-26 00:00", "2026-11-27 00:00")]);
    let oil = MarketSpec { schedule: holiday.clone(), ..h.engine.markets()[&OIL].spec.clone() };
    let mut gold = MarketSpec { schedule: holiday, ..h.engine.markets()[&GOLD].spec.clone() };
    // Gold's markup moves with no notice, which a listing of it alone would be refused for.
    gold.financing.markup_bps += 100;
    let refusal = refused(&h.operate(AdminCommand::RelistMarkets { specs: vec![oil, gold] })).to_owned();
    assert!(refusal.contains("markup change"), "{refusal}");
    assert_eq!(h.engine.markets()[&OIL].spec.schedule, globex(vec![]), "oil's relisting did not apply");
}

#[test]
fn a_relisting_of_several_markets_refuses_what_it_cannot_relist() {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    let spec = |id: MarketId, h: &Harness| h.engine.markets()[&id].spec.clone();
    let cases: [(Vec<MarketSpec>, &str); 4] = [
        (vec![], "no markets to relist"),
        (vec![spec(OIL, &h), spec(OIL, &h)], "market 32 is named twice"),
        (vec![spec(OIL, &h), commodity(SILVER, "XAG-USD", globex(vec![]), 0)], "market 31 is not listed"),
        (vec![MarketSpec { symbol: "CL".into(), ..spec(OIL, &h) }], "keeps market 32's quote currency and symbol"),
    ];
    for (specs, reason) in cases {
        let refusal = refused(&h.operate(AdminCommand::RelistMarkets { specs })).to_owned();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
    }
    let mut before = genesis_with_grid(BEFORE, pairs(5_000), 750, 0);
    let specs = vec![spec(OIL, &before), spec(GOLD, &before)];
    let refusal = refused(&before.operate(AdminCommand::RelistMarkets { specs })).to_owned();
    assert!(refusal.contains("needs protocol V81"), "{refusal}");
}

#[test]
fn the_grid_moves_only_by_a_notice_whose_time_has_come() {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    let mut raised = pairs(5_000);
    let eth = &mut raised.groups[0].members[1];
    eth.residual_shock_bps = 40;
    raised.groups[1].floor_share_bps = 7_000;
    let set = AdminCommand::SetParams(with_grid(&h, raised.clone()));
    assert!(refused(&h.operate(set.clone())).contains("needs a notice of at least seven days"));
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(grid_notice(raised.clone(), due))));
    assert!(refused(&h.operate(set.clone())).contains(&format!("takes effect from {due}")));
    h.advance_to(due);
    assert!(applied(&h.operate(set)));
    assert_eq!(grid_in_force(&h), raised);
    assert_eq!(h.engine.operator_bounds().notice, None, "the applied grid spent the notice");
}

#[test]
fn a_notice_moves_only_the_shocks_and_floor_shares_up_within_their_bounds() {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    let due = h.ts + NOTICE_MS;
    let cases: [(Change, &str); 5] = [
        (|g| g.groups[0].members[0].residual_shock_bps = 10, "residual shock 10 must be from its launch value 20"),
        (|g| g.groups[1].members[1].residual_shock_bps = 1_601, "bound on its residual shock must be from the shock"),
        (|g| g.groups[0].floor_share_bps = 4_000, "floor share 4000 must be from its launch value 5000"),
        (|g| g.groups[0].members[1].loading = f("1.5"), "fixed at its launch grid"),
        (|g| g.groups[1].members.swap(0, 1), "fixed at its launch grid"),
    ];
    for (change, reason) in cases {
        let mut moved = pairs(5_000);
        change(&mut moved);
        let refusal = refused(&h.operate(grid_notice(moved, due))).to_owned();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
    }
    assert_eq!(h.engine.operator_bounds().launch_grid, Some(pairs(5_000)));
}

#[test]
fn a_log_whose_launch_grid_is_empty_keeps_none() {
    let mut h = genesis_with_grid(V81, PortfolioGrid::default(), 750, 0);
    assert_eq!(h.engine.operator_bounds().launch_grid, Some(PortfolioGrid::default()));
    assert!(h.engine.portfolio_rules().is_none());
    let refusal = refused(&h.operate(grid_notice(pairs(5_000), h.ts + NOTICE_MS))).to_owned();
    assert!(refusal.contains("groups are fixed at its launch grid"), "{refusal}");
    let refusal = refused(&h.operate(AdminCommand::SetParams(with_grid(&h, pairs(5_000))))).to_owned();
    assert!(refusal.contains("groups are fixed at its launch grid"), "{refusal}");
}

#[test]
fn a_grid_named_before_v81_is_taken_up_by_its_activation() {
    let mut h = genesis_with_grid(BEFORE, pairs(5_000), 750, 0);
    assert!(h.engine.portfolio_rules().is_none(), "before V81 the grid decides nothing");
    assert_eq!(h.engine.operator_bounds().launch_grid, None);
    assert_eq!(h.engine.markets()[&GOLD].weekend, None, "gold has no weekend margin");
    h.admin(AdminCommand::ActivateProtocol { version: V81 });
    assert_eq!(h.engine.portfolio_rules(), Some(&pairs(5_000)));
    assert_eq!(h.engine.operator_bounds().launch_grid, Some(pairs(5_000)));
    // The activation gives gold, with no weekend margin, no weekend gap, and keeps oil's.
    assert_eq!(h.engine.markets()[&GOLD].weekend, None);
    assert!(h.engine.markets()[&OIL].weekend.is_some_and(|gap| !gap.is_frontier()));
}

#[test]
fn a_grouped_market_with_no_weekend_margin_keeps_no_gap_and_its_calendar_checks_are_v60s() {
    let mut h = genesis_with_grid(V81, pairs(5_000), 750, 0);
    assert_eq!(h.engine.markets()[&GOLD].weekend, None, "a market with no weekend margin keeps no gap");
    let a = client(&mut h, 1, "1000000");
    h.price(GOLD, "2000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, GOLD, Side::Buy, "10", "2000"));
    // A closure from tomorrow, which `V60` refuses for a held market with a weekend margin, moves held gold
    // with unheld oil; gold still keeps no gap, and its holder keeps its position.
    let early = |id: MarketId, h: &Harness| MarketSpec {
        schedule: globex(vec![period("2026-10-21 10:00", "2026-10-23 10:00")]),
        ..h.engine.markets()[&id].spec.clone()
    };
    let both = AdminCommand::RelistMarkets { specs: vec![early(GOLD, &h), early(OIL, &h)] };
    assert!(applied(&h.operate(both)));
    assert_eq!(h.engine.markets()[&GOLD].weekend, None);
    assert_eq!(h.engine.accounts()[&a.account].position(GOLD).qty, f("10"));
}
