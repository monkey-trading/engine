//! Parameter votes held to the listing bounds: a voted relisting needs no listing notice, but each
//! band table it changes stays within its market's launch bands; the operator's listing notice is
//! refused while rotation is on; a voted stress level keeps its size multiplier at 1x or more and a
//! voted ceiling covers its market's launch close charge; with rotation off a relisting under a
//! margin restriction is measured from the baseline; and from `V82` a voted relisting may shut a held
//! market within seven days, its seven days being the notice, while the operator's own is refused, by
//! the votes while they govern and by the calendar notice once rotation is off.

use super::*;
use chrono::NaiveDateTime;
use common::ny;
use engine_core::listing_bounds::ListingNotice;
use engine_core::session::{Period, Schedule};
use engine_core::spread_tier::SpreadTier;

/// `market`'s baseline listing with each launch spread band's rate, in tenths of a bp, mapped by
/// `rate` from the band's index and launch rate.
fn spreads(h: &Harness, market: MarketId, rate: impl Fn(usize, i64) -> i64) -> AdminCommand {
    let launch = &h.engine.operator_bounds().launch_bands[&market].spread_tiers;
    let spread_tiers =
        launch.iter().enumerate().map(|(i, band)| SpreadTier::tenths(band.up_to, rate(i, band.tenth_bps()))).collect();
    AdminCommand::UpsertMarket(MarketSpec { spread_tiers, ..h.engine.baseline_spec(market).expect("listed") })
}

#[test]
fn a_voted_relisting_needs_no_listing_notice_and_keeps_its_spread_bands_within_launch() {
    let mut h = governed();
    let last = h.engine.operator_bounds().launch_bands[&BTC].spread_tiers.len() - 1;
    let doubled = spreads(&h, BTC, |_, rate| rate * 2);
    let over = spreads(&h, BTC, |_, rate| rate * 2 + 1);
    let under = spreads(&h, BTC, |i, rate| if i == last { rate - 1 } else { rate });
    for (proposal, command) in [(1, doubled.clone()), (2, over.clone()), (3, under.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, over), "its launch rate to twice it");
    rejected(&apply(&mut h, 3, under), "its launch rate to twice it");
    assert_eq!(apply(&mut h, 1, doubled.clone()).last(), Some(&Event::AdminApplied));
    let AdminCommand::UpsertMarket(spec) = doubled else { unreachable!() };
    assert_eq!(h.engine.markets()[&BTC].spec.spread_tiers, spec.spread_tiers);
    assert!(h.engine.operator_bounds().markups.is_empty(), "no listing notice was named or spent");
}

#[test]
fn a_voted_margin_band_change_stays_at_or_above_launch() {
    let mut h = governed();
    let launch = h.engine.operator_bounds().launch_bands[&BTC].margin_tiers.clone();
    assert_eq!(launch, one_band(1_000));
    let (below, raised) = (margin(&h, BTC, 999), margin(&h, BTC, 1_200));
    pass(&mut h, 1, false, below.clone());
    pass(&mut h, 2, false, raised.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, below), "below its launch rate of 1000bps");
    assert_eq!(apply(&mut h, 2, raised).last(), Some(&Event::AdminApplied));
    assert_eq!(margin_bps(&h, BTC), one_band(1_200));
}

#[test]
fn the_operators_listing_notice_waits_for_rotation_to_end() {
    let mut h = governed();
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: Some(5),
        weekend_margin_bps: None,
        price_rules: None,
        effective_at: h.ts + NOTICE_MS,
        min_level_distance_bps: None,
        contracts: None,
    };
    let announce = AdminCommand::AnnounceListing { market: BTC, notice };
    rejected(&h.operate(announce.clone()), "only a passed vote changes this");
    switch_rotation(&mut h, false);
    assert_eq!(h.operate(announce).last(), Some(&Event::AdminApplied));
}

#[test]
fn a_voted_stress_level_and_ceiling_meet_the_multiplier_and_close_charge_floors() {
    let mut h = governed();
    let waived = with_params(&h, |params| params.stress_levels[0].size_spread_multiplier_bps = 9_999);
    pass(&mut h, 1, false, waived.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, waived), "size multiplier is at least 10000 bps");
    let low =
        AdminCommand::AdoptCloseSpreadCeiling { market: BTC, max_close_spread_bps: 1, effective_at: 0, listing: None };
    pass(&mut h, 2, false, low.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, low), "below its launch close charge");
}

#[test]
fn with_rotation_off_a_relisting_under_a_margin_restriction_is_measured_from_the_baseline() {
    let mut h = governed();
    let raise = margin(&h, BTC, 4_000);
    pass(&mut h, 1, true, raise);
    assert_eq!(margin_bps(&h, BTC), one_band(4_000));
    switch_rotation(&mut h, false);
    let unchanged = AdminCommand::UpsertMarket(h.engine.baseline_spec(BTC).expect("listed"));
    assert_eq!(h.operate(unchanged).last(), Some(&Event::AdminApplied), "the baseline's margins need no notice");
    assert_eq!(margin_bps(&h, BTC), one_band(4_000), "the restriction holds over the relisting");
    let lowered = margin(&h, BTC, 1_100);
    rejected(&h.operate(lowered), "margin band change needs a notice");
}

#[test]
fn a_passed_vote_may_shut_a_held_market_within_seven_days_and_the_operators_relisting_may_not() {
    // Account 2 holds 50 AAPL; votes govern under the latest rules.
    let mut base = Harness::v21();
    base.deposit(acct(2), "20000");
    base.price(AAPL, "250");
    let events = base.market_order(acct(2), AAPL, Side::Buy, "50", "250");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let mut h = governed_from(base, ProtocolVersion::LATEST, TTL);
    assert_eq!(h.ts, ny(2026, 10, 20, 10, 0));

    // A closure of Wednesday 28 October's session, a day after the vote's notice runs.
    let wednesday = closure(&h, "2026-10-28 09:30", "2026-10-28 16:00");
    pass(&mut h, 1, false, wednesday.clone());
    h.advance(NOTICE_MS);
    rejected(&h.operate(wednesday.clone()), "only a passed vote");
    assert_eq!(apply(&mut h, 1, wednesday).last(), Some(&Event::AdminApplied));
    assert!(!h.engine.markets()[&AAPL].spec.schedule.is_open(ny(2026, 10, 28, 12, 0)));

    // With rotation off, the operator's relisting shutting Thursday's session meets the calendar bound.
    switch_rotation(&mut h, false);
    let thursday = closure(&h, "2026-10-29 09:30", "2026-10-29 16:00");
    rejected(&h.operate(thursday), "a calendar change may not shut market 3 within 7 days");
    assert!(h.engine.markets()[&AAPL].spec.schedule.is_open(ny(2026, 10, 29, 12, 0)));
}

/// AAPL's baseline listing with a closure from `from` to `to`, New York wall-clock `"YYYY-MM-DD HH:MM"`,
/// added to its calendar.
fn closure(h: &Harness, from: &str, to: &str) -> AdminCommand {
    let at = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
    let mut spec = h.engine.baseline_spec(AAPL).expect("listed");
    let Schedule::Weekly { closures, .. } = &mut spec.schedule else { unreachable!("a weekly calendar") };
    closures.push(Period { from: at(from), to: at(to) });
    AdminCommand::UpsertMarket(spec)
}
