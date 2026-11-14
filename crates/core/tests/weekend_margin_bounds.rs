//! `V59`'s bounds on a market's weekend margin, through `Engine::apply`: a listed market's weekend
//! margin moves only as a listing notice of at least seven days names it, to a rate from none to
//! 100%; it needs a calendar with a weekend gap; a calendar change may not narrow or remove a gap
//! starting within seven days, before or once it starts, nor, while the market is held, start one
//! within seven days; and before `V59` it does nothing and no notice names it, until activation
//! starts it, holding a gap it is in or that starts within the lead with no close-out, through a
//! relisting and to a gap within the lead after it.
//! Its votes are in `parameter_votes/weekend.rs`.

mod common;

use common::close_spread::{NOTICE_MS, trade};
use common::events::{fills, only_fill};
use common::spreads::{applied, refused};
use common::weekend::{OIL, OIL_WEEKEND_BPS, chicago, client, genesis_at, genesis_v59, globex, period, withdraw};
use common::{BTC, Harness};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{MarketId, Millis, Side};
use engine_core::weekend::WeekendGap;

#[test]
fn a_listed_markets_weekend_margin_moves_only_as_a_notice_names_it() {
    let mut h = funded_genesis();
    let gap = h.engine.markets()[&OIL].weekend;
    let events = h.operate(AdminCommand::UpsertMarket(relisting(&h, 1_000)));
    assert_eq!(refused(&events), "market 32's weekend margin change needs a notice of at least seven days");
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(OIL, 1_000, due))));
    let events = h.operate(AdminCommand::UpsertMarket(relisting(&h, 1_000)));
    assert_eq!(refused(&events), format!("market 32's weekend margin change takes effect from {due}"));
    // A relisting at the noticed rate applies from the notice's time, once.
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(relisting(&h, 1_000)))));
    assert_eq!(h.engine.markets()[&OIL].spec.weekend_margin_bps, 1_000);
    assert!(h.engine.operator_bounds().markups.is_empty(), "the notice is spent");
    let events = h.operate(AdminCommand::UpsertMarket(relisting(&h, OIL_WEEKEND_BPS)));
    assert_eq!(refused(&events), "market 32's weekend margin change needs a notice of at least seven days");
    // A noticed removal leaves the market with no weekend gap to keep.
    assert!(gap.is_some() && h.engine.markets()[&OIL].weekend.is_some());
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(OIL, 0, due))));
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(relisting(&h, 0)))));
    assert_eq!(h.engine.markets()[&OIL].weekend, None);
}

#[test]
fn a_weekend_margin_is_a_rate_to_100_percent_on_a_calendar_with_a_gap() {
    let mut h = funded_genesis();
    let due = h.ts + NOTICE_MS;
    // Above 100%, below none, or on a market that never shuts.
    for (market, bps, reason) in [
        (OIL, 10_001, "weekend_margin_bps must be in 0..=10000"),
        (OIL, -1, "weekend_margin_bps must be in 0..=10000"),
        (BTC, 2_000, "weekend_margin_bps needs a weekly schedule"),
    ] {
        assert_eq!(refused(&h.operate(notice(market, bps, due))), reason, "{market} {bps}");
    }
    // A rate under oil's 1% band charges nothing over it, so it is at or above the normal margin too.
    assert!(applied(&h.operate(notice(OIL, 50, due))));
    // Hours whose closures are all shorter than a gap leave a weekend margin nothing to cover.
    let windows: Vec<_> = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        .iter()
        .map(|day| serde_json::json!({ "open": format!("{day} 01:00"), "close": format!("{day} 23:00") }))
        .collect();
    let schedule = serde_json::json!({ "kind": "weekly", "tz": "America/Chicago", "windows": windows });
    let schedule: Schedule = serde_json::from_value(schedule).unwrap();
    let spec = MarketSpec { schedule, ..h.engine.markets()[&OIL].spec.clone() };
    let events = h.operate(AdminCommand::UpsertMarket(spec));
    let reason = "market 32's calendar has no closure of at least 40 hours starting within 366 days, so its weekend margin would never apply";
    assert_eq!(refused(&events), reason);
}

#[test]
fn before_v59_a_weekend_margin_does_nothing_until_activation_starts_it() {
    // Listed under `V56`, oil's weekend margin keeps no close, and an open in what would be its lead
    // takes the normal margin.
    let mut h = genesis_at(ProtocolVersion::V56, globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    assert_eq!(h.engine.markets()[&OIL].weekend, None);
    let close = chicago(2026, 10, 23, 16, 0);
    h.advance_to(close - HOUR_MS);
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // Nor can it move: a notice of one needs `V59`, and a change no notice names is refused.
    let events = h.operate(notice(OIL, 1_000, h.ts + NOTICE_MS));
    assert_eq!(refused(&events), "a weekend margin notice needs protocol V59");
    let events = h.operate(AdminCommand::UpsertMarket(relisting(&h, 1_000)));
    assert_eq!(refused(&events), "market 32's weekend margin change needs a notice of at least seven days");
    // Activation starts it with the gap an hour on, which, less than the lead away, it holds as one
    // it is in: the weekend margin applies, but a, opened at the normal margin, is not closed out.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    let gap = WeekendGap { close, reopen: chicago(2026, 10, 25, 17, 0), closed_out: true };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(gap));
    assert!(withdrawal_refused(&withdraw(&mut h, &a, "100")));
    // A relisting before the close keeps it so.
    h.advance(60_000);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(relisting(&h, OIL_WEEKEND_BPS)))));
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(gap));
    h.advance_to(close + HOUR_MS);
    assert!(fills(&h.cmd(Command::Tick)).is_empty());
}

#[test]
fn an_activation_inside_a_gap_holds_it_to_its_reopening_with_no_close_out() {
    let mut h = genesis_at(ProtocolVersion::V56, globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // Activated on Saturday, in the gap: a, below 7.5%, keeps its long, though it may withdraw nothing
    // the weekend margin needs.
    h.advance_to(chicago(2026, 10, 24, 10, 0));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    let reopen = chicago(2026, 10, 25, 17, 0);
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(WeekendGap { close: h.ts, reopen, closed_out: true }));
    h.advance(HOUR_MS);
    assert!(fills(&h.cmd(Command::Tick)).is_empty());
    assert!(withdrawal_refused(&withdraw(&mut h, &a, "100")));
    // At the reopening it keeps the next gap, whose close-out is to run.
    h.advance_to(reopen + HOUR_MS);
    h.cmd(Command::Tick);
    let next =
        WeekendGap { close: chicago(2026, 10, 30, 16, 0), reopen: chicago(2026, 11, 1, 17, 0), closed_out: false };
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(next));
}

#[test]
fn a_calendar_change_may_not_narrow_or_remove_a_weekend_gap_within_seven_days() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "60000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    let kept = h.engine.markets()[&OIL].weekend.expect("oil keeps a gap");
    let spec = h.engine.markets()[&OIL].spec.clone();
    // A minute's session on a Saturday splits its weekend's gap into two shorter closures.
    let with_session = |day: &str, closures: Vec<Period>| {
        let Schedule::Weekly { tz, windows, .. } = globex(vec![]) else { unreachable!("weekly") };
        let special_sessions = vec![period(&format!("{day} 12:00"), &format!("{day} 12:01"))];
        let schedule = Schedule::Weekly { tz, windows, closed_dates: vec![], closures, special_sessions };
        AdminCommand::UpsertMarket(MarketSpec { schedule, ..spec.clone() })
    };
    let reason = |close: Millis| {
        format!(
            "a calendar change may not narrow or remove market 32's weekend gap from {close} to {}, within 7 days",
            kept.reopen
        )
    };
    assert_eq!(refused(&h.operate(with_session("2026-10-24", vec![]))), reason(kept.close));
    // Closing Friday at noon widens it, which applies.
    let noon = vec![period("2026-10-23 12:00", "2026-10-23 16:00")];
    let early = MarketSpec { schedule: globex(noon.clone()), ..spec.clone() };
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(early))));
    // Flat, the market's gap still may not be split, as new positions could carry it at the normal
    // margin; a Saturday more than seven days on may be.
    only_fill(&trade(&mut h, &a, OIL, Side::Sell, "10000", "70"));
    let split = with_session("2026-10-24", noon.clone());
    assert_eq!(refused(&h.operate(split)), reason(chicago(2026, 10, 23, 12, 0)));
    assert!(applied(&h.operate(with_session("2026-10-31", noon))));
}

#[test]
fn in_a_gap_a_calendar_change_may_not_add_the_next_one() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "60000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // On Sunday at 16:30, half an hour before oil reopens, a closure from 17:30 to Tuesday would start
    // a gap half an hour after it.
    h.advance_to(chicago(2026, 10, 25, 16, 30));
    let closure = vec![period("2026-10-25 17:30", "2026-10-27 17:00")];
    let spec = MarketSpec { schedule: globex(closure), ..h.engine.markets()[&OIL].spec.clone() };
    let reason = format!(
        "market 32 holds positions, so a calendar change may not start a weekend gap at {}, within 7 days, that its calendar does not",
        chicago(2026, 10, 25, 17, 30)
    );
    assert_eq!(refused(&h.operate(AdminCommand::UpsertMarket(spec))), reason);
}

#[test]
fn once_its_gap_has_started_a_held_markets_calendar_may_not_open_it_early() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "60000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // Saturday at 10:00: Friday's close-out has run, and a's long, which met the weekend margin, stays.
    h.advance_to(chicago(2026, 10, 24, 10, 0));
    h.cmd(Command::Tick);
    let kept = h.engine.markets()[&OIL].weekend.expect("oil keeps a gap");
    assert!(kept.closed_out && !h.engine.accounts()[&a.account].positions.is_empty(), "{kept:?}");
    // A session at noon would open the market a day before its reopening.
    let Schedule::Weekly { tz, windows, .. } = globex(vec![]) else { unreachable!("weekly") };
    let special_sessions = vec![period("2026-10-24 12:00", "2026-10-24 12:01")];
    let schedule = Schedule::Weekly { tz, windows, closed_dates: vec![], closures: vec![], special_sessions };
    let early = MarketSpec { schedule, ..h.engine.markets()[&OIL].spec.clone() };
    let reason = format!(
        "a calendar change may not narrow or remove market 32's weekend gap from {} to {}, within 7 days",
        h.ts, kept.reopen
    );
    assert_eq!(refused(&h.operate(AdminCommand::UpsertMarket(early))), reason);
}

#[test]
fn a_calendar_change_may_not_start_a_held_markets_weekend_gap_within_seven_days() {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "60000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    let kept = h.engine.markets()[&OIL].weekend.expect("oil keeps a gap");
    let spec = h.engine.markets()[&OIL].spec.clone();
    let shut = |from: &str, to: &str| {
        AdminCommand::UpsertMarket(MarketSpec { schedule: globex(vec![period(from, to)]), ..spec.clone() })
    };
    // On Wednesday at 10:00, a closure to Friday from now would close a's long out at once at the
    // weekend margin, and one from 11:00 or from noon within hours: a margin raise with less notice
    // than a rate change takes.
    h.advance_to(chicago(2026, 10, 21, 10, 0));
    for (from, start) in
        [("2026-10-21 10:00", h.ts), ("2026-10-21 11:00", h.ts + HOUR_MS), ("2026-10-21 12:00", h.ts + 2 * HOUR_MS)]
    {
        let reason = format!(
            "market 32 holds positions, so a calendar change may not start a weekend gap at {start}, within 7 days, that its calendar does not"
        );
        assert_eq!(refused(&h.operate(shut(from, "2026-10-23 06:00"))), reason, "{from}");
    }
    // The same closure a week on applies, and the market keeps Friday's gap.
    assert!(applied(&h.operate(shut("2026-10-28 12:00", "2026-10-30 06:00"))));
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(kept));
}

#[test]
fn a_gap_within_the_lead_of_an_activation_in_a_gap_is_held_with_no_close_out() {
    // Sunday's reopening at 17:00 lasts an hour, before a closure from 18:00 to Tuesday.
    let schedule = globex(vec![period("2026-10-25 18:00", "2026-10-27 17:00")]);
    let mut h = genesis_at(ProtocolVersion::V56, schedule, OIL_WEEKEND_BPS, 0);
    let a = client(&mut h, 1, "10000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    // Activated on Sunday at 16:30, in the weekend gap: the gap after it, an hour and a half on, is
    // held as well, so a, below 7.5%, keeps its long.
    h.advance_to(chicago(2026, 10, 25, 16, 30));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 });
    h.advance_to(chicago(2026, 10, 25, 19, 0));
    assert!(fills(&h.cmd(Command::Tick)).is_empty());
    let (close, reopen) = (chicago(2026, 10, 25, 18, 0), chicago(2026, 10, 27, 17, 0));
    assert_eq!(h.engine.markets()[&OIL].weekend, Some(WeekendGap { close, reopen, closed_out: true }));
    assert!(!h.engine.accounts()[&a.account].positions.is_empty());
}

/// Whether a withdrawal was refused.
fn withdrawal_refused(events: &[Event]) -> bool {
    matches!(events.last(), Some(Event::WithdrawalRejected { .. }))
}

/// `genesis_v59` with oil at its launch weekend margin and a funded client, so the operator signs.
fn funded_genesis() -> Harness {
    let mut h = genesis_v59(globex(vec![]), OIL_WEEKEND_BPS, 0);
    client(&mut h, 1, "10000");
    h
}

/// Oil's listing as it stands, with `weekend_margin_bps`.
fn relisting(h: &Harness, weekend_margin_bps: i64) -> MarketSpec {
    MarketSpec { weekend_margin_bps, ..h.engine.markets()[&OIL].spec.clone() }
}

/// A notice naming `market`'s weekend margin alone, from `effective_at`.
fn notice(market: MarketId, weekend_margin_bps: i64, effective_at: Millis) -> AdminCommand {
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: Some(weekend_margin_bps),
        effective_at,
    };
    AdminCommand::AnnounceListing { market, notice }
}

const HOUR_MS: Millis = 60 * 60 * 1000;
