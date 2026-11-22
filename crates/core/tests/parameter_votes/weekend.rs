//! Parameter votes over a market's weekend margin, from `V60`: a voted relisting moves it with no
//! listing notice, to none or to a rate its listing admits; the operator's notice of one waits for
//! rotation to end; the fast path cannot move it, and may raise a margin band past it; and a voted
//! calendar change meets no calendar bound, even for a held market, whose holders close out at once
//! when it puts the market in a gap.

use super::*;
use chrono::TimeZone;
use engine_core::Fixed;
use engine_core::listing_bounds::ListingNotice;
use engine_core::session::{Period, Schedule};
use engine_core::weekend::WeekendGap;

/// `governed_under(V56)`, with `V60` activated by a vote, as rotation requires.
fn weekend_governed() -> Harness {
    weekend_governed_from(governed_under(ProtocolVersion::V56, TTL))
}

/// `h`, a `governed_under(V56)` harness, with `V60` activated by a vote.
fn weekend_governed_from(mut h: Harness) -> Harness {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V60 };
    pass(&mut h, 90, false, activate.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 90, activate).last(), Some(&Event::AdminApplied));
    h
}

/// AAPL's baseline listing with a weekend margin of `bps`.
fn weekend(h: &Harness, bps: i64) -> AdminCommand {
    let spec = h.engine.baseline_spec(AAPL).expect("listed");
    AdminCommand::UpsertMarket(MarketSpec { weekend_margin_bps: bps, ..spec })
}

#[test]
fn a_voted_relisting_moves_a_weekend_margin_with_no_listing_notice() {
    let mut h = weekend_governed();
    let (set, over) = (weekend(&h, 3_000), weekend(&h, 10_001));
    pass(&mut h, 1, false, set.clone());
    pass(&mut h, 2, false, over.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, over), "weekend_margin_bps must be in 0..=10000");
    assert_eq!(apply(&mut h, 1, set).last(), Some(&Event::AdminApplied));
    let aapl = &h.engine.markets()[&AAPL];
    assert_eq!((aapl.spec.weekend_margin_bps, aapl.weekend.is_some()), (3_000, true));
    assert!(h.engine.operator_bounds().markups.is_empty(), "no listing notice was named or spent");
    // A vote takes it away again, and the market keeps no weekend close.
    let none = weekend(&h, 0);
    pass(&mut h, 3, false, none.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 3, none).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&AAPL].weekend, None);
}

#[test]
fn the_operators_weekend_notice_waits_for_rotation_to_end_and_the_fast_path_cannot_move_it() {
    let mut h = weekend_governed();
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: Some(3_000),
        price_rules: None,
        effective_at: h.ts + NOTICE_MS,
        min_level_distance_bps: None,
        contracts: None,
    };
    let announce = AdminCommand::AnnounceListing { market: AAPL, notice };
    rejected(&h.operate(announce.clone()), "only a passed vote changes this");
    let fast = weekend(&h, 3_000);
    refused(&pass(&mut h, 1, true, fast), "changes another field");
    switch_rotation(&mut h, false);
    assert_eq!(h.operate(announce).last(), Some(&Event::AdminApplied));
}

#[test]
fn a_fast_path_margin_raise_may_pass_the_weekend_rate() {
    let mut h = weekend_governed();
    let set = weekend(&h, 3_000);
    pass(&mut h, 1, false, set.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, set).last(), Some(&Event::AdminApplied));
    // AAPL's one 20% band raised to 40%, past its 30% weekend rate, as a restriction.
    let raise = margin(&h, AAPL, 4_000);
    let events = pass(&mut h, 2, true, raise);
    assert!(events.iter().any(|event| matches!(event, Event::RestrictionApplied { .. })), "{events:?}");
    assert_eq!(margin_bps(&h, AAPL), one_band(4_000));
}

/// AAPL's baseline listing with `closure` (New York wall-clock bounds, `"YYYY-MM-DD HH:MM"`) added to
/// its calendar.
fn closed(h: &Harness, from: &str, to: &str) -> AdminCommand {
    let at = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
    let mut spec = h.engine.baseline_spec(AAPL).expect("listed");
    let Schedule::Weekly { closures, .. } = &mut spec.schedule else { unreachable!("a weekly calendar") };
    closures.push(Period { from: at(from), to: at(to) });
    AdminCommand::UpsertMarket(spec)
}

/// A New York wall-clock time as epoch millis.
fn ny_at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}

/// AAPL with a voted weekend margin of 30% from Tuesday 3 November, and `calendar` passed that day,
/// which applies from Tuesday 10 November at 09:00 New York time. Account 2 buys 50 shares at 250 with
/// `equity` at `V21`, keeps them over Friday's gap with its equity above the weekend margin at 250,
/// and the last mark before Monday's close is `last`. Each change is a vote, so none meets the
/// operator's calendar bounds or listing notice.
fn held_aapl(equity: &str, last: &str, calendar: impl FnOnce(&Harness) -> AdminCommand) -> (Harness, AdminCommand) {
    let mut base = Harness::v21();
    base.deposit(acct(2), equity);
    base.price(AAPL, "250");
    let events = base.market_order(acct(2), AAPL, Side::Buy, "50", "250");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let mut h = weekend_governed_from(governed_from(base, ProtocolVersion::V56, TTL));
    let set = weekend(&h, 3_000);
    pass(&mut h, 1, false, set.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, set).last(), Some(&Event::AdminApplied));
    assert_eq!(h.ts, ny_at(2026, 11, 3, 9, 0));
    let command = calendar(&h);
    pass(&mut h, 2, false, command.clone());
    h.advance_to(ny_at(2026, 11, 6, 15, 0));
    h.price(AAPL, "250");
    h.advance_to(ny_at(2026, 11, 9, 15, 0));
    h.price(AAPL, last);
    h.advance_to(ny_at(2026, 11, 10, 9, 0));
    assert_eq!(position(&h), Some(f("50")), "Friday's gap closed nothing out");
    (h, command)
}

fn position(h: &Harness) -> Option<Fixed> {
    h.engine.accounts()[&acct(2)].positions.get(&AAPL).map(|position| position.qty)
}

#[test]
fn a_voted_calendar_change_may_start_a_gap_within_seven_days_for_a_held_market() {
    // A closure from Wednesday 11 November at 10:00 to Saturday, which the calendar in force has the
    // market open for. A held market's calendar may not start a gap within seven days by the
    // operator's relisting, but a vote may.
    let (mut h, command) = held_aapl("20000", "248", |h| closed(h, "2026-11-11 10:00", "2026-11-14 10:00"));
    let events = apply(&mut h, 2, command);
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. } | Event::Fill(_))), "{events:?}");
    // The gap runs from the closure's start to Monday's open, with its close-out still to come.
    let gap = WeekendGap { close: ny_at(2026, 11, 11, 10, 0), reopen: ny_at(2026, 11, 16, 9, 30), closed_out: false };
    assert_eq!(h.engine.markets()[&AAPL].weekend, Some(gap));
    assert_eq!(position(&h), Some(f("50")));
}

#[test]
fn a_voted_calendar_change_putting_a_held_market_in_a_gap_closes_its_holders_out_once() {
    // A closure from Monday 9 November at 15:30 to Wednesday 18 November at 10:00, which has the market
    // in a gap on Tuesday 10 November, before the weekend gap the calendar in force had kept.
    let (mut h, command) = held_aapl("4500", "200", |h| closed(h, "2026-11-09 15:30", "2026-11-18 10:00"));
    assert_eq!(
        h.engine.markets()[&AAPL].weekend.map(|gap| (gap.close, gap.closed_out)),
        Some((ny_at(2026, 11, 13, 16, 0), false)),
        "the kept gap's close-out is still to come"
    );
    // 50 shares at the 200 mark are $10,000: 30% is $3,000, which the account's equity, about $2,000, is below.
    let events = apply(&mut h, 2, command.clone());
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    let close_outs = events.iter().filter(|event| matches!(event, Event::CloseOut { .. })).count();
    assert_eq!(close_outs, 1, "{events:?}");
    let fills: Vec<_> =
        events.iter().filter_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None }).collect();
    let [closed] = fills[..] else { panic!("{events:?}") };
    assert!(closed.liquidation && closed.account == acct(2) && closed.market == AAPL, "{closed:?}");
    assert_eq!((closed.side, closed.qty, closed.index, closed.position), (Side::Sell, f("50"), f("200"), Fixed::ZERO));
    let Some(Event::CloseOut { margin, .. }) = events.iter().find(|event| matches!(event, Event::CloseOut { .. }))
    else {
        unreachable!("counted above")
    };
    assert_eq!(*margin, f("3000"));
    assert_eq!(position(&h), None);
    let gap = WeekendGap { close: h.ts, reopen: ny_at(2026, 11, 18, 10, 0), closed_out: true };
    assert_eq!(h.engine.markets()[&AAPL].weekend, Some(gap));

    // The same calendar voted again, applying inside the gap, closes nothing and keeps the gap.
    pass(&mut h, 3, false, command.clone());
    h.advance(NOTICE_MS);
    let events = apply(&mut h, 3, command);
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. } | Event::Fill(_))), "{events:?}");
    assert_eq!(h.engine.markets()[&AAPL].weekend, Some(gap));
}
