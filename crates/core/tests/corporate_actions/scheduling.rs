use chrono::NaiveDate;
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::CorporateAction;
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule, Window};
use engine_core::types::{AssetClass, MAX_STAMP_MS};

use crate::common::*;
use crate::support::*;

#[test]
fn signed_operator_instructions_require_a_rate_and_validate_both_boundaries() {
    let mut h = fixture();
    let valid = dividend("valid", AAPL, "1", "0.25");
    let unsigned = h.cmd(Command::Admin(AdminCommand::ScheduleCorporateAction(valid.clone())));
    rejected(&unsigned, "signed operator");
    let mut missing = serde_json::to_value(&valid).unwrap();
    missing["adjustment"].as_object_mut().unwrap().remove("withholding_rate");
    assert!(serde_json::from_value::<CorporateAction>(missing).unwrap_err().to_string().contains("withholding_rate"));
    for rate in ["-0.000000001", "1.000000001"] {
        rejected(&schedule(&mut h, dividend("rate", AAPL, "1", rate)), "withholding_rate");
    }
    for amount in ["0", "-1", "1000000000.000000001"] {
        rejected(&schedule(&mut h, dividend("amount", AAPL, amount, "0")), "gross_per_unit");
    }
    for (new, old) in [(0, 1), (1, 0), (1, 1)] {
        rejected(&schedule(&mut h, split("ratio", new, old)), "unequal");
    }
    for delta in [-1, 1] {
        let mut wrong = valid.clone();
        wrong.previous_close += delta;
        rejected(&schedule(&mut h, wrong), "actual venue");
        let mut wrong = valid.clone();
        wrong.ex_open += delta;
        rejected(&schedule(&mut h, wrong), "actual venue");
    }
    let mut skipped = valid.clone();
    skipped.ex_open = ny(2026, 10, 22, 9, 30);
    rejected(&schedule(&mut h, skipped), "intervening");
    accepted(&schedule(&mut h, valid.clone()));
    rejected(&schedule(&mut h, valid), "duplicate corporate action id");
    rejected(&schedule(&mut h, dividend("other-id", AAPL, "1", "0.25")), "duplicate or inconsistent");
    assert_eq!(h.engine.corporate_actions().len(), 1);
}

#[test]
fn equity_and_index_are_the_only_entitlement_classes_and_only_equities_split() {
    for class in [
        AssetClass::Fx,
        AssetClass::Commodity,
        AssetClass::Etf,
        AssetClass::Bond,
        AssetClass::Rates,
        AssetClass::Crypto,
        AssetClass::Index,
    ] {
        let mut h = fixture();
        let mut spec = h.engine.markets()[&AAPL].spec.clone();
        spec.asset_class = class;
        h.operator_admin(AdminCommand::UpsertMarket(spec));
        if class == AssetClass::Index {
            accepted(&schedule(&mut h, dividend("index", AAPL, "1", "0")));
        } else {
            rejected(&schedule(&mut h, dividend("cash", AAPL, "1", "0")), "unsupported");
        }
        rejected(&schedule(&mut h, split("stock", 4, 1)), "unsupported");
    }
}

#[test]
fn pre_cutoff_is_strict_and_capture_precedes_commands_at_the_close() {
    for at in [close() - 1, close(), close() + 1] {
        let mut h = held("2", "3");
        h.advance_to(at);
        let events = schedule(&mut h, dividend("cutoff", AAPL, "1", "0"));
        if at < close() {
            accepted(&events);
        } else {
            rejected(&events, "cutoff");
        }
    }
    let mut h = held("2", "3");
    accepted(&schedule(&mut h, dividend("capture", AAPL, "1", "0")));
    h.advance_to(close());
    let events = h.market_order(acct(1), AAPL, engine_core::types::Side::Sell, "2", "80");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::OrderRejected { reason: engine_core::event::Reject::MarketClosed, .. }))
    );
    assert_eq!(h.engine.corporate_actions()["capture"].quantities.as_ref().unwrap()[&acct(1)], f("2"));
    assert_eq!(payment(&tick(&mut h, open()), 1), f("2"));
}

#[test]
fn pending_actions_lock_calendar_currency_identity_and_class_but_allow_margin_edits() {
    let mut h = fixture();
    h.list_fx();
    accepted(&schedule(&mut h, dividend("locked", AAPL, "1", "0")));
    for field in ["schedule", "symbol", "asset_class", "quote_currency"] {
        let mut spec = h.engine.markets()[&AAPL].spec.clone();
        match field {
            "schedule" => spec.schedule = Schedule::AlwaysOpen,
            "symbol" => spec.symbol = "DIFFERENT".into(),
            "asset_class" => spec.asset_class = AssetClass::Etf,
            _ => spec.quote_currency = QuoteCurrency::Eur { fx_market: EURUSD },
        }
        let events = h.operate(AdminCommand::UpsertMarket(spec));
        assert!(
            events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("locks"))),
            "{events:?}"
        );
    }
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    spec.margin_tiers[0].bps = 3000;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    assert_eq!(h.engine.markets()[&AAPL].spec.margin_tiers[0].bps, 3000);
}

#[test]
fn dst_weekends_holidays_and_dated_early_and_special_sessions_use_actual_boundaries() {
    let cases = [
        (ny(2026, 10, 30, 16, 0), ny(2026, 11, 2, 9, 30), None, None, None),
        (ny(2026, 11, 25, 16, 0), ny(2026, 11, 27, 9, 30), Some("2026-11-26"), None, None),
        (
            ny(2026, 11, 27, 13, 0),
            ny(2026, 11, 30, 9, 30),
            None,
            Some(("2026-11-27T13:00:00", "2026-11-27T16:00:00")),
            None,
        ),
        (
            ny(2026, 10, 23, 16, 0),
            ny(2026, 10, 24, 10, 0),
            None,
            None,
            Some(("2026-10-24T10:00:00", "2026-10-24T12:00:00")),
        ),
    ];
    for (cutoff, opening, holiday, closure, special) in cases {
        let mut h = fixture();
        let mut spec = h.engine.markets()[&AAPL].spec.clone();
        let Schedule::Weekly { closed_dates, closures, special_sessions, .. } = &mut spec.schedule else {
            unreachable!()
        };
        if let Some(day) = holiday {
            closed_dates.push(NaiveDate::parse_from_str(day, "%Y-%m-%d").unwrap());
        }
        let period = |(from, to): (&str, &str)| Period { from: from.parse().unwrap(), to: to.parse().unwrap() };
        closures.extend(closure.map(period));
        special_sessions.extend(special.map(period));
        h.operator_admin(AdminCommand::UpsertMarket(spec));
        let mut action = dividend("calendar", AAPL, "1", "0");
        action.previous_close = cutoff;
        action.ex_open = opening;
        accepted(&schedule(&mut h, action));
        let events = tick(&mut h, opening);
        assert!(
            events.iter().any(|e| matches!(e, Event::CorporateActionApplied { at, .. } if *at == opening)),
            "{events:?}"
        );
    }
}

#[test]
fn rate_and_calendar_rejections_burn_authenticated_nonce_without_installing_an_action() {
    let mut h = fixture();
    let operator = h.engine.operator().unwrap().address;
    let nonce = h.engine.next_request_nonce(operator);
    rejected(&schedule(&mut h, dividend("invalid", AAPL, "1", "2")), "withholding_rate");
    assert_eq!(h.engine.next_request_nonce(operator), nonce + 1);
    assert!(h.engine.corporate_actions().is_empty());
    // Ordered and in range, so only the venue calendar refuses it.
    let mut wrong = split("invalid-schedule", 4, 1);
    wrong.previous_close += 1;
    rejected(&schedule(&mut h, wrong), "actual venue");
    assert_eq!(h.engine.next_request_nonce(operator), nonce + 2);
    assert!(h.engine.corporate_actions().is_empty());
}

#[test]
fn submillisecond_special_sessions_cannot_hide_an_intervening_venue_open() {
    let mut h = fixture();
    let mut spec = h.engine.markets()[&AAPL].spec.clone();
    let Schedule::Weekly { special_sessions, .. } = &mut spec.schedule else { unreachable!() };
    special_sessions.push(Period {
        from: "2026-10-20T17:00:00.000000001".parse().unwrap(),
        to: "2026-10-20T17:01:00".parse().unwrap(),
    });
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    rejected(&schedule(&mut h, dividend("skips-special", AAPL, "1", "0")), "immediately before");
}

#[test]
fn offset_changes_cannot_hide_reopenings_across_weekly_or_dated_closing_boundaries() {
    let utc = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis();
    let cases = [
        ("Sun 00:30", "Sun 01:30", None, None, "2026-10-25T00:30:00Z", "2026-10-25T01:00:00Z", "2026-11-01T00:30:00Z"),
        (
            "Sun 00:30",
            "Sun 03:00",
            Some(("2026-10-25T01:30:00", "2026-10-25T02:00:00")),
            None,
            "2026-10-25T00:30:00Z",
            "2026-10-25T01:00:00Z",
            "2026-10-25T02:00:00Z",
        ),
        (
            "Mon 09:30",
            "Mon 16:00",
            None,
            Some(("2026-10-25T00:30:00", "2026-10-25T01:30:00")),
            "2026-10-25T00:30:00Z",
            "2026-10-25T01:00:00Z",
            "2026-10-26T09:30:00Z",
        ),
        ("Sun 01:30", "Sun 02:30", None, None, "2027-03-21T02:30:00Z", "2027-03-28T01:00:00Z", "2027-04-04T00:30:00Z"),
    ];
    for (from, to, closure, special, cutoff, first_open, skipped_open) in cases {
        let mut h = fixture();
        let mut spec = h.engine.markets()[&AAPL].spec.clone();
        let period = |(from, to): (&str, &str)| Period { from: from.parse().unwrap(), to: to.parse().unwrap() };
        spec.schedule = Schedule::Weekly {
            tz: chrono_tz::Europe::London,
            windows: vec![Window { open: from.parse().unwrap(), close: to.parse().unwrap() }],
            closed_dates: vec![],
            closures: closure.map(period).into_iter().collect(),
            special_sessions: special.map(period).into_iter().collect(),
        };
        h.operator_admin(AdminCommand::UpsertMarket(spec));
        let mut action = dividend("offset-change", AAPL, "1", "0");
        action.previous_close = utc(cutoff);
        action.ex_open = utc(skipped_open);
        rejected(&schedule(&mut h, action.clone()), "previous_close");
        action.ex_open = utc(first_open);
        accepted(&schedule(&mut h, action));
        assert!(tick(&mut h, utc(first_open)).iter().any(|e| matches!(e, Event::CorporateActionApplied { .. })));
    }
}

#[test]
fn instructions_are_refused_before_protocol_v23() {
    let mut h = Harness::v17();
    for version in
        [ProtocolVersion::V18, ProtocolVersion::V19, ProtocolVersion::V20, ProtocolVersion::V21, ProtocolVersion::V22]
    {
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    rejected(&schedule(&mut h, dividend("early", AAPL, "1", "0")), "protocol V23");
    assert!(h.engine.corporate_actions().is_empty());
}

#[test]
fn ids_must_hold_one_to_128_bytes() {
    let mut h = fixture();
    rejected(&schedule(&mut h, dividend("", AAPL, "1", "0")), "1–128 bytes");
    rejected(&schedule(&mut h, dividend(&"x".repeat(129), AAPL, "1", "0")), "1–128 bytes");
    // A multi-byte id counts bytes, not characters: 65 two-byte characters are 130 bytes.
    rejected(&schedule(&mut h, dividend(&"é".repeat(65), AAPL, "1", "0")), "1–128 bytes");
    accepted(&schedule(&mut h, dividend(&"x".repeat(128), AAPL, "1", "0")));
}

#[test]
fn boundaries_must_be_ordered_positive_engine_timestamps() {
    let mut h = fixture();
    for (previous_close, ex_open) in [
        (close(), close()),
        (close(), close() - 1),
        (open(), close()),
        (0, open()),
        (-1, open()),
        (close(), MAX_STAMP_MS + 1),
    ] {
        let mut action = dividend("bounds", AAPL, "1", "0");
        action.previous_close = previous_close;
        action.ex_open = ex_open;
        rejected(&schedule(&mut h, action), "ordered positive engine timestamps");
    }
    // The last representable stamp passes this check and is refused by the venue calendar instead.
    let mut last = dividend("bounds", AAPL, "1", "0");
    last.ex_open = MAX_STAMP_MS;
    rejected(&schedule(&mut h, last), "actual venue");
    assert!(h.engine.corporate_actions().is_empty());
}

#[test]
fn an_unknown_market_is_refused() {
    let mut h = fixture();
    assert!(!h.engine.markets().contains_key(&999));
    rejected(&schedule(&mut h, dividend("ghost", 999, "1", "0")), "unknown corporate action market");
    let mut ghost = split("ghost-split", 2, 1);
    ghost.market = 999;
    rejected(&schedule(&mut h, ghost), "unknown corporate action market");
}
