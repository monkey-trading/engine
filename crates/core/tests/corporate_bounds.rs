//! Corporate action bounds from `V76`, through `Engine::apply`: a cash dividend is at most a tenth of
//! its share's mark at launch unless scheduled seven days before its previous close, a split at most
//! 20 to 1 either way, every action at least two of its market's sessions before its ex-date, and
//! while votes govern only a passed vote schedules one. So the key in office cannot pay its own
//! account a dividend no issuer declared out of house cash. The bounds move only by a notice, never
//! looser than launch's.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::weekend::{client, genesis_at, globex, period, withdraw};
use common::{AAPL, Client, Harness, f, ny};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::{Adjustment, CappedDividend, CorporateAction, CorporateBounds, LAUNCH_CORPORATE_BOUNDS};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::governance::{GovernanceConfig, PassedProposal, VotedChange, command_hash};
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::operator_bounds::{NOTICE_MS, OperatorBounds, ParamsNotice};
use engine_core::params::{ProtocolParams, Rollover};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::staking::StakingConfig;
use engine_core::turns::{RegistryRotationSet, SetMember};
use engine_core::types::{AccountId, Millis, Side};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;

/// A fresh log at `version` at Tuesday 20 October 2026, 10:00 in New York, with 100m of house capital
/// and AAPL at $200.
fn genesis(version: ProtocolVersion) -> Harness {
    let mut h = genesis_at(version, globex(vec![]), 0, 0);
    h.price(AAPL, "200");
    h
}

/// A dividend of `gross` a share, nothing withheld, before the close on 2026-10-`close_day` and ex at
/// the next morning's open.
fn dividend(id: &str, gross: &str, close_day: u32) -> CorporateAction {
    action(id, close_day, Adjustment::Dividend { gross_per_unit: f(gross), withholding_rate: Fixed::ZERO })
}

/// A split of `new_units` for `old_units` around the close on 2026-10-`close_day`.
fn split(id: &str, new_units: u32, old_units: u32, close_day: u32) -> CorporateAction {
    action(id, close_day, Adjustment::Split { new_units, old_units })
}

/// AAPL's `adjustment` at its close on 2026-10-`close_day` and the next weekday's open.
fn action(id: &str, close_day: u32, adjustment: Adjustment) -> CorporateAction {
    // 23 October 2026 is a Friday; every other close day here is a Monday to Thursday.
    let next = if close_day == 23 { 26 } else { close_day + 1 };
    CorporateAction {
        id: id.into(),
        market: AAPL,
        previous_close: ny(2026, 10, close_day, 16, 0),
        ex_open: ny(2026, 10, next, 9, 30),
        adjustment,
    }
}

fn schedule(h: &mut Harness, action: CorporateAction) -> Vec<Event> {
    h.operate(AdminCommand::ScheduleCorporateAction(action))
}

fn applied(events: &[Event]) -> bool {
    events.last() == Some(&Event::AdminApplied)
}

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

/// What `client` was paid in dividends among `events`.
fn paid(events: &[Event], client: &Client) -> Fixed {
    events
        .iter()
        .filter_map(|event| match event {
            Event::CorporateDividend { account, amount, .. } if *account == client.account => Some(*amount),
            _ => None,
        })
        .sum()
}

/// The key in office buys one share on its own account, signs a $90m dividend ex at Thursday's open,
/// two trading days on, and at that open asks to withdraw $89m. Returns the scheduling's events, what
/// the account gained, what house cash lost, and what the withdrawal approved.
fn take(h: &mut Harness, own: &Client) -> (Vec<Event>, Fixed, Fixed, Fixed) {
    only_fill(&trade(h, own, AAPL, Side::Buy, "1", "200"));
    let (house, balance) = (h.engine.house_cash(), h.balance(own.account));
    let scheduled = schedule(h, dividend("ninety-million", "90000000", 21));
    h.advance_to(ny(2026, 10, 22, 9, 30));
    h.price(AAPL, "200");
    let gained = h.balance(own.account) - balance;
    let approved = withdraw(h, own, "89000000")
        .iter()
        .filter_map(|event| match event {
            Event::WithdrawalApproved { account, amount, .. } if *account == own.account => Some(*amount),
            _ => None,
        })
        .sum();
    (scheduled, gained, house - h.engine.house_cash(), approved)
}

/// Votes govern from now: staking, the recorded vote contract, a rotation set in effect naming the
/// harness operator's key, and rotation on.
fn govern(h: &mut Harness) {
    let config =
        StakingConfig { liquidity_floor: f("1000000"), epoch_ms: DAY_MS, start_ms: h.ts, cooldown_ms: 28 * DAY_MS };
    h.operator_admin(AdminCommand::InitStaking(config));
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: AccountId([0x70; 20]), restriction_ttl_ms: DAY_MS }));
    let member =
        SetMember { candidate: AccountId([0xc1; 20]), admin_key: Client::operator().account, weight: TokenUnits(1) };
    let set = RegistryRotationSet {
        set: 1,
        epoch: 1,
        effective_at: h.ts,
        quorum: true,
        members: vec![member],
        block: 1,
        chain_ref: "0xset:1".into(),
    };
    let events = h.cmd(Command::AdoptRotationSet(set));
    assert!(events.iter().any(|e| matches!(e, Event::RotationSetAdopted { .. })), "{events:?}");
    let switch =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block: 2, chain_ref: "0xon".into() };
    assert!(h.cmd(switch).iter().any(|e| matches!(e, Event::RotationSwitched { .. })));
}

/// The parameters in force with `bounds`.
fn with_bounds(h: &Harness, corporate_bounds: CorporateBounds) -> AdminCommand {
    AdminCommand::SetParams(ProtocolParams { corporate_bounds, ..h.engine.params().cloned().unwrap() })
}

/// The operator's notice of `bounds` from `effective_at`.
fn bounds_notice(corporate_bounds: CorporateBounds, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: None,
        rollover: None,
        corporate_bounds: Some(corporate_bounds),
        effective_at,
    })
}

#[test]
fn a_dividend_above_its_cap_on_short_notice_is_refused_so_the_operators_share_takes_no_house_cash() {
    let mut h = genesis(ProtocolVersion::V76);
    let own = client(&mut h, 2, "1000");
    let (scheduled, gained, house_lost, approved) = take(&mut h, &own);
    assert!(
        gained <= f("200"),
        "dividend scheduled: {}; one share was paid {gained}, house cash fell by {house_lost}, and the engine \
         approved a withdrawal of {approved}",
        applied(&scheduled)
    );
    rejected(&scheduled, "dividends on short notice total at most 1000bps of their market's mark");
    assert!(approved.is_zero());
}

#[test]
fn while_votes_govern_only_a_passed_vote_schedules_a_corporate_action() {
    let mut h = genesis(ProtocolVersion::V76);
    govern(&mut h);
    // Control: the key in office can no longer change a parameter on its own.
    let params = ProtocolParams { house_total_cap_bps: 19_999, ..h.engine.params().cloned().unwrap() };
    rejected(&h.operate(AdminCommand::SetParams(params)), "only a passed vote");

    let own = client(&mut h, 2, "1000");
    let (scheduled, gained, house_lost, approved) = take(&mut h, &own);
    assert!(
        gained <= f("200"),
        "while votes govern, dividend scheduled: {}; one share was paid {gained}, house cash fell by \
         {house_lost}, and the engine approved a withdrawal of {approved}",
        applied(&scheduled)
    );
    rejected(&scheduled, "only a passed vote");
    rejected(&schedule(&mut h, dividend("within-bounds", "1", 26)), "only a passed vote");

    // A passed vote schedules one seven days on, its notice standing for a dividend above the cap: 15%,
    // before Friday 30 October's close and ex at Monday's open, two trading days after it applies.
    let adjustment = Adjustment::Dividend { gross_per_unit: f("30"), withholding_rate: Fixed::ZERO };
    let (previous_close, ex_open) = (ny(2026, 10, 30, 16, 0), ny(2026, 11, 2, 9, 30));
    let action = CorporateAction { id: "voted".into(), market: AAPL, previous_close, ex_open, adjustment };
    let voted = AdminCommand::ScheduleCorporateAction(action);
    let (block, chain_ref) = (h.log.len() as u64, "0xvote:1".to_string());
    let passed = PassedProposal {
        proposal: 1,
        fast: false,
        command_hash: command_hash(&voted),
        command: Some(voted.clone()),
        block,
        chain_ref,
    };
    let events = h.cmd(Command::PassProposal(Box::new(passed)));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { proposal: 1, .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 1, command: voted })));
    assert!(applied(&events), "{events:?}");
    h.advance_to(ex_open);
    assert_eq!(paid(&h.price(AAPL, "200"), &own), f("30"));
}

#[test]
fn a_dividend_up_to_a_tenth_of_the_mark_two_trading_days_ahead_is_scheduled() {
    let mut h = genesis(ProtocolVersion::V76);
    let holder = client(&mut h, 1, "1000");
    only_fill(&trade(&mut h, &holder, AAPL, Side::Buy, "2", "200"));
    // Tuesday's close is the only one before Wednesday's open.
    rejected(&schedule(&mut h, dividend("tuesday", "20", 20)), "at least 2 of its market's trading days");
    rejected(&schedule(&mut h, dividend("above", "20.000000001", 21)), "at most 1000bps of their market's mark");
    assert!(applied(&schedule(&mut h, dividend("tenth", "20", 21))));
    h.advance_to(ny(2026, 10, 22, 9, 30));
    assert_eq!(paid(&h.price(AAPL, "200"), &holder), f("40"));
}

#[test]
fn a_dividend_above_the_cap_needs_seven_days_before_its_previous_close() {
    let mut h = genesis(ProtocolVersion::V76);
    rejected(&schedule(&mut h, dividend("soon", "30", 26)), "needs a notice of at least seven days");
    assert!(ny(2026, 10, 27, 16, 0) - h.ts >= NOTICE_MS);
    assert!(applied(&schedule(&mut h, dividend("noticed", "30", 27))));
}

#[test]
fn a_split_is_at_most_twenty_to_one_either_way_with_seven_days_notice() {
    let mut h = genesis(ProtocolVersion::V76);
    rejected(&schedule(&mut h, split("forward", 21, 1, 27)), "a split's ratio is at most 20 either way");
    rejected(&schedule(&mut h, split("reverse", 1, 21, 27)), "a split's ratio is at most 20 either way");
    // Monday 26 October's close is under seven days away; Tuesday 27's is over.
    rejected(&schedule(&mut h, split("soon", 2, 1, 26)), "a split needs a notice of at least seven days");
    assert!(applied(&schedule(&mut h, split("forward", 20, 1, 27))));
    assert!(applied(&schedule(&mut h, split("reverse", 1, 20, 28))));
    assert!(applied(&schedule(&mut h, split("odd", 41, 3, 29))));
}

#[test]
fn the_corporate_bounds_move_only_by_notice_and_never_looser_than_launch() {
    let mut h = genesis(ProtocolVersion::V76);
    assert_eq!(h.engine.operator_bounds().launch_corporate_bounds, Some(LAUNCH_CORPORATE_BOUNDS));
    let tighter = CorporateBounds { max_dividend_bps: 500, max_split_ratio: 10, min_trading_days: 3 };
    rejected(&h.operate(with_bounds(&h, tighter)), "a corporate bounds change needs a notice of at least seven days");
    let at = h.ts + NOTICE_MS;
    for looser in [
        CorporateBounds { max_dividend_bps: 1_001, ..LAUNCH_CORPORATE_BOUNDS },
        CorporateBounds { max_split_ratio: 21, ..LAUNCH_CORPORATE_BOUNDS },
        CorporateBounds { min_trading_days: 1, ..LAUNCH_CORPORATE_BOUNDS },
    ] {
        rejected(&h.operate(bounds_notice(looser, at)), "looser than launch's");
    }
    let invalid = CorporateBounds { min_trading_days: 21, ..LAUNCH_CORPORATE_BOUNDS };
    rejected(&h.operate(bounds_notice(invalid, at)), "min_trading_days must be in 1..=20");
    assert!(applied(&h.operate(bounds_notice(tighter, at))));
    rejected(&h.operate(with_bounds(&h, tighter)), "the corporate bounds change takes effect from");

    h.advance_to(at);
    h.price(AAPL, "200");
    assert!(applied(&h.operate(with_bounds(&h, tighter))));
    assert_eq!(h.engine.params().unwrap().corporate_bounds, tighter);
    assert!(h.engine.operator_bounds().notice.is_none(), "the change spends the notice");
    // Tuesday 27 October, 10:00: two closes are no longer enough, nor 6% of the mark, nor 11 to 1.
    rejected(&schedule(&mut h, dividend("two", "1", 28)), "at least 3 of its market's trading days");
    rejected(&schedule(&mut h, dividend("six", "12", 29)), "at most 500bps of their market's mark");
    rejected(&schedule(&mut h, split("eleven", 11, 1, 29)), "a split's ratio is at most 10 either way");
    assert!(applied(&schedule(&mut h, dividend("five", "10", 29))));
}

#[test]
fn before_v76_a_corporate_action_has_no_bound_the_parameters_name_none_and_it_stays_pending() {
    let mut h = genesis(ProtocolVersion::V74);
    assert!(applied(&schedule(&mut h, dividend("ninety-million", "90000000", 20))));
    assert!(applied(&schedule(&mut h, split("forty", 40, 1, 21))));
    let tighter = CorporateBounds { max_dividend_bps: 500, ..LAUNCH_CORPORATE_BOUNDS };
    rejected(&h.operate(with_bounds(&h, tighter)), "corporate bounds need protocol V76");
    rejected(&h.operate(bounds_notice(tighter, h.ts + NOTICE_MS)), "a corporate bounds notice needs protocol V76");
    // Instructions pending at V76's activation keep the rules they were scheduled under.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V76 });
    let pending: Vec<_> = h.engine.corporate_actions().keys().cloned().collect();
    assert_eq!(pending, ["forty", "ninety-million"]);
}

#[test]
fn short_notice_dividends_on_one_market_total_at_most_the_cap_within_seven_days() {
    let mut h = genesis(ProtocolVersion::V76);
    // 3% ex Thursday and 7% ex Friday fill the cap; a unit more within seven days is refused.
    assert!(applied(&schedule(&mut h, dividend("three", "6", 21))));
    assert!(applied(&schedule(&mut h, dividend("seven", "14", 22))));
    rejected(&schedule(&mut h, dividend("more", "0.000000001", 26)), "at most 1000bps of their market's mark");
    // Seven days' notice takes a dividend outside the cap, and it counts toward no later one.
    assert!(applied(&schedule(&mut h, dividend("noticed", "30", 27))));
    // From Tuesday 27 October, ex Thursday is within seven days of Friday's 7%; ex Friday 30 is not.
    h.advance_to(ny(2026, 10, 27, 10, 0));
    h.price(AAPL, "200");
    rejected(&schedule(&mut h, dividend("thursday", "20", 28)), "at most 1000bps of their market's mark");
    assert!(applied(&schedule(&mut h, dividend("friday", "20", 29))));
    // A week after Friday's ex-date, the dividends ex before it no longer count, and are dropped.
    h.advance_to(ny(2026, 11, 2, 10, 0));
    h.price(AAPL, "200");
    let adjustment = Adjustment::Dividend { gross_per_unit: f("20"), withholding_rate: Fixed::ZERO };
    let (previous_close, ex_open) = (ny(2026, 11, 5, 16, 0), ny(2026, 11, 6, 9, 30));
    let action = CorporateAction { id: "november".into(), market: AAPL, previous_close, ex_open, adjustment };
    assert!(applied(&schedule(&mut h, action)));
    let kept: Vec<_> = h.engine.operator_bounds().capped_dividends.iter().map(|capped| capped.ex_open).collect();
    assert_eq!(kept, [ny(2026, 10, 30, 9, 30), ex_open]);
}

#[test]
fn a_dividend_is_capped_in_the_units_held_at_its_capture_and_a_split_follows_pending_dividends() {
    let mut h = genesis(ProtocolVersion::V76);
    // Two for one ex Wednesday 28 October, and a noticed dividend captured at Thursday's close.
    assert!(applied(&schedule(&mut h, split("halving", 2, 1, 27))));
    assert!(applied(&schedule(&mut h, dividend("noticed", "1", 29))));
    // No split may land before that dividend's capture; one at its own opening may.
    rejected(
        &schedule(&mut h, split("early", 20, 1, 28)),
        "a split may not apply before a pending dividend's previous close",
    );
    assert!(applied(&schedule(&mut h, split("later", 20, 1, 29))));
    // From Monday 26, Wednesday's close holds twice the shares at half the mark: $10 is the cap.
    h.advance_to(ny(2026, 10, 26, 10, 0));
    h.price(AAPL, "200");
    rejected(&schedule(&mut h, dividend("pre-split", "10.000000001", 28)), "at most 1000bps of their market's mark");
    assert!(applied(&schedule(&mut h, dividend("post-split", "10", 28))));
}

#[test]
fn trading_days_are_distinct_dates_a_day_apart_whatever_sessions_the_calendar_adds() {
    let mut h = genesis(ProtocolVersion::V76);
    let regular = h.engine.markets()[&AAPL].spec.clone();
    let Schedule::Weekly { tz, windows, .. } = regular.schedule.clone() else {
        unreachable!("AAPL keeps weekly hours")
    };
    let relist = |closed_dates, closures, special_sessions| {
        let schedule = Schedule::Weekly { tz, windows: windows.clone(), closed_dates, closures, special_sessions };
        AdminCommand::UpsertMarket(MarketSpec { schedule, ..regular.clone() })
    };
    let dividend_at = |id: &str, previous_close, ex_open| CorporateAction {
        id: id.into(),
        market: AAPL,
        previous_close,
        ex_open,
        adjustment: Adjustment::Dividend { gross_per_unit: f("1"), withholding_rate: Fixed::ZERO },
    };
    // The operator relists AAPL at once with two short closures this morning and a session just after
    // midnight. Two closes this morning are one trading day, and less than a day away.
    let closures = vec![period("2026-10-20 10:31", "2026-10-20 10:32"), period("2026-10-20 10:33", "2026-10-20 10:34")];
    let midnight = vec![period("2026-10-21 00:00", "2026-10-21 00:01")];
    assert!(applied(&h.operate(relist(vec![], closures, midnight))));
    let morning = dividend_at("morning", ny(2026, 10, 20, 10, 33), ny(2026, 10, 20, 10, 34));
    rejected(&schedule(&mut h, morning), "at least 2 of its market's trading days");
    // Tuesday's close and the midnight session's are two dates, but less than a day apart.
    let night = dividend_at("night", ny(2026, 10, 21, 0, 1), ny(2026, 10, 21, 9, 30));
    rejected(&schedule(&mut h, night), "at least 2 of its market's trading days");

    // At three trading days, four closes on two dates two days apart are not enough: two closures on
    // Tuesday 27 October and a holiday on Wednesday leave Tuesday and Thursday.
    let three = CorporateBounds { min_trading_days: 3, ..LAUNCH_CORPORATE_BOUNDS };
    assert!(applied(&h.operate(bounds_notice(three, h.ts + NOTICE_MS))));
    h.advance_to(ny(2026, 10, 27, 10, 0));
    h.price(AAPL, "200");
    assert!(applied(&h.operate(with_bounds(&h, three))));
    let closures = vec![period("2026-10-27 10:31", "2026-10-27 10:32"), period("2026-10-27 10:33", "2026-10-27 10:34")];
    let holiday = vec![chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()];
    assert!(applied(&h.operate(relist(holiday, closures, vec![]))));
    let thursday = dividend_at("thursday", ny(2026, 10, 29, 16, 0), ny(2026, 10, 30, 9, 30));
    rejected(&schedule(&mut h, thursday), "at least 3 of its market's trading days");
    let friday = dividend_at("friday", ny(2026, 10, 30, 16, 0), ny(2026, 11, 2, 9, 30));
    assert!(applied(&schedule(&mut h, friday)));
}

#[test]
fn pending_splits_that_take_the_mark_past_what_it_can_hold_refuse_a_short_notice_dividend() {
    let mut h = genesis(ProtocolVersion::V76);
    // Twenty-one short closures on Wednesday 28 October's afternoon, each a boundary for a noticed
    // one-for-twenty split.
    let regular = h.engine.markets()[&AAPL].spec.clone();
    let Schedule::Weekly { tz, windows, closed_dates, .. } = regular.schedule.clone() else {
        unreachable!("AAPL keeps weekly hours")
    };
    let minute = |m: u32| format!("2026-10-28 12:{m:02}");
    let closures: Vec<_> = (0..21).map(|i| period(&minute(2 * i), &minute(2 * i + 1))).collect();
    let boundaries: Vec<_> = (0..21).map(|i| (ny(2026, 10, 28, 12, 2 * i), ny(2026, 10, 28, 12, 2 * i + 1))).collect();
    let hours = Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions: vec![] };
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(MarketSpec { schedule: hours, ..regular }))));
    for (i, (previous_close, ex_open)) in boundaries.into_iter().enumerate() {
        let adjustment = Adjustment::Split { new_units: 1, old_units: 20 };
        let split = CorporateAction { id: format!("reverse-{i}"), market: AAPL, previous_close, ex_open, adjustment };
        assert!(applied(&schedule(&mut h, split)));
    }
    // From Monday 26, twenty to the twenty-first times $200 is past what a mark holds: no cap, so no
    // short notice.
    h.advance_to(ny(2026, 10, 26, 10, 0));
    h.price(AAPL, "200");
    rejected(&schedule(&mut h, dividend("after", "1", 29)), "at most 1000bps of their market's mark");
}

#[test]
fn v76s_state_and_notice_fields_encode_by_name_and_only_when_set() {
    let notice = |rollover, corporate_bounds| ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: None,
        rollover,
        corporate_bounds,
        effective_at: 7,
    };
    assert_eq!(encode(&notice(None, None)), r#"{"effective_at":7}"#);
    let rollover = Rollover { tz: chrono_tz::UTC, hour: 21, minute: 30 };
    assert_eq!(
        encode(&notice(Some(rollover), Some(LAUNCH_CORPORATE_BOUNDS))),
        r#"{"rollover":{"tz":"UTC","hour":21,"minute":30},"corporate_bounds":{"max_dividend_bps":1000,"max_split_ratio":20,"min_trading_days":2},"effective_at":7}"#
    );
    let rules = PriceRules { max_age_ms: 1_000, max_divergence_bps: 50, min_sources: 2 };
    let listing = ListingNotice { price_rules: Some(rules), ..ListingNotice::markup(0, 7) };
    let listing = ListingNotice { markup_bps: None, ..listing };
    assert_eq!(
        encode(&listing),
        r#"{"price_rules":{"max_age_ms":1000,"max_divergence_bps":50,"min_sources":2},"effective_at":7}"#
    );
    // Parameters at launch's corporate bounds name none, and other bounds by name.
    let params = common::params();
    assert!(!encode(&params).contains("corporate_bounds"));
    let tighter = ProtocolParams {
        corporate_bounds: CorporateBounds { max_dividend_bps: 500, ..LAUNCH_CORPORATE_BOUNDS },
        ..params
    };
    assert!(
        encode(&tighter)
            .contains(r#""corporate_bounds":{"max_dividend_bps":500,"max_split_ratio":20,"min_trading_days":2}"#)
    );
    // The bounds' state names a short-notice dividend and the last rollover only once set.
    let mut bounds = OperatorBounds::default();
    assert!(bounds.is_default() && encode(&bounds) == "{}");
    bounds.last_rollover = Some(5);
    bounds.capped_dividends.push(CappedDividend { market: AAPL, ex_open: 6, share: f("0.1") });
    assert_eq!(encode(&bounds), r#"{"last_rollover":5,"capped_dividends":[{"market":3,"ex_open":6,"share":"0.1"}]}"#);
}

/// `value`'s JSON, the encoding notices are signed as and the state tree hashes.
fn encode(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).expect("serialises")
}

#[test]
fn a_passed_vote_schedules_a_split_on_short_notice_but_never_within_two_trading_days() {
    let mut h = genesis(ProtocolVersion::V76);
    govern(&mut h);
    // Both apply on Tuesday 27 October at 10:00: a split before Thursday's close, and a dividend
    // before Tuesday's own.
    let split = AdminCommand::ScheduleCorporateAction(split("voted-split", 2, 1, 29));
    let dividend = AdminCommand::ScheduleCorporateAction(dividend("voted-dividend", "1", 27));
    for (proposal, command) in [(2, &split), (3, &dividend)] {
        let (block, chain_ref) = (h.log.len() as u64, format!("0xvote:{proposal}"));
        let command_hash = command_hash(command);
        let passed =
            PassedProposal { proposal, fast: false, command_hash, command: Some(command.clone()), block, chain_ref };
        let events = h.cmd(Command::PassProposal(Box::new(passed)));
        assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    }
    h.advance_to(ny(2026, 10, 27, 10, 0));
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 2, command: split })));
    assert!(
        applied(&events) && events.iter().any(|e| matches!(e, Event::CorporateActionScheduled { .. })),
        "{events:?}"
    );
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 3, command: dividend })));
    rejected(&events, "at least 2 of its market's trading days");
}
