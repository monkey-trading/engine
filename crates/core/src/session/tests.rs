use super::*;

fn w(open: &str, close: &str) -> Window {
    Window { open: open.parse().unwrap(), close: close.parse().unwrap() }
}

fn ny_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}

fn us_equities() -> Schedule {
    Schedule::Weekly {
        tz: chrono_tz::America::New_York,
        windows: DAYS[..5].iter().map(|d| w(&format!("{d} 09:30"), &format!("{d} 16:00"))).collect(),
        closed_dates: vec![NaiveDate::from_ymd_opt(2026, 11, 26).unwrap()],
        closures: vec![],
        special_sessions: vec![],
    }
}

#[test]
fn equity_session_respects_local_time_and_holidays() {
    let s = us_equities();
    // Tue 20 Oct 2026: closed one minute before the open, open until the minute of the close.
    assert!(!s.is_open(ny_ms(2026, 10, 20, 9, 29)));
    assert!(s.is_open(ny_ms(2026, 10, 20, 9, 30)));
    assert!(s.is_open(ny_ms(2026, 10, 20, 15, 59)));
    assert!(!s.is_open(ny_ms(2026, 10, 20, 16, 0)));
    // Saturday.
    assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0)));
    // Thanksgiving is a listed closure.
    assert!(!s.is_open(ny_ms(2026, 11, 26, 12, 0)));
    // DST ends 1 Nov 2026; 09:30 local is still the open.
    assert!(s.is_open(ny_ms(2026, 11, 2, 9, 30)));
}

#[test]
fn fx_window_wraps_the_week() {
    let s = Schedule::Weekly {
        tz: chrono_tz::America::New_York,
        windows: vec![w("Sun 17:00", "Fri 17:00")],
        closed_dates: vec![],
        closures: vec![],
        special_sessions: vec![],
    };
    assert!(s.is_open(ny_ms(2026, 10, 19, 3, 0))); // Mon
    assert!(!s.is_open(ny_ms(2026, 10, 23, 17, 0))); // Fri close
    assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0))); // Sat
    assert!(!s.is_open(ny_ms(2026, 10, 25, 16, 59))); // Sun pre-open
    assert!(s.is_open(ny_ms(2026, 10, 25, 17, 0))); // Sun open
}

#[test]
fn out_of_range_timestamps_read_as_closed() {
    assert!(!us_equities().is_open(i64::MIN));
    assert!(!us_equities().is_open(i64::MAX));
}

#[test]
fn validation_rejects_schedules_that_never_open() {
    let empty = Schedule::Weekly {
        tz: chrono_tz::UTC,
        windows: vec![],
        closed_dates: vec![],
        closures: vec![],
        special_sessions: vec![],
    };
    assert!(empty.validate().is_err());
    let zero = Schedule::Weekly {
        tz: chrono_tz::UTC,
        windows: vec![w("Mon 09:00", "Mon 09:00")],
        closed_dates: vec![],
        closures: vec![],
        special_sessions: vec![],
    };
    assert!(zero.validate().is_err());
    assert!(us_equities().validate().is_ok());
}

#[test]
fn unknown_schedule_fields_are_rejected() {
    let typo = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": [], "closed_date": [] });
    assert!(serde_json::from_value::<Schedule>(typo).is_err());
    let typo = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": [], "closure": [] });
    assert!(serde_json::from_value::<Schedule>(typo).is_err());
    let typo = serde_json::json!({ "from": "2026-11-27T13:00:00", "until": "2026-11-27T16:00:00" });
    assert!(serde_json::from_value::<Period>(typo).is_err());
}

#[test]
fn an_early_close_shortens_only_its_own_session() {
    let s = us_equities_with(vec![period("2026-11-27 13:00", "2026-11-27 16:00")], vec![]);
    assert!(s.is_open(ny_ms(2026, 11, 27, 12, 59)));
    assert!(!s.is_open(ny_ms(2026, 11, 27, 13, 0)));
    assert!(!s.is_open(ny_ms(2026, 11, 27, 15, 59)));
    // The sessions either side keep their hours.
    assert!(s.is_open(ny_ms(2026, 11, 25, 15, 59)));
    assert!(s.is_open(ny_ms(2026, 11, 30, 9, 30)));
    assert!(s.is_open(ny_ms(2026, 11, 30, 15, 59)));
}

#[test]
fn a_closure_spans_midnight_and_the_weekend() {
    // Halts Thu 24 Dec 2026 at 12:15 Chicago and reopens with the regular Sunday session.
    let s = globex(vec![period("2026-12-24 12:15", "2026-12-27 17:00")], vec![]);
    assert!(s.is_open(chicago_ms(2026, 12, 24, 12, 14)));
    assert!(!s.is_open(chicago_ms(2026, 12, 24, 12, 15)));
    // Thursday evening's session, which the windows alone would open, stays shut.
    assert!(!s.is_open(chicago_ms(2026, 12, 24, 17, 0)));
    assert!(!s.is_open(chicago_ms(2026, 12, 25, 10, 0)));
    assert!(s.is_open(chicago_ms(2026, 12, 27, 17, 0)));
    // A week earlier the same hours are open.
    assert!(s.is_open(chicago_ms(2026, 12, 17, 17, 0)));
}

#[test]
fn a_special_session_opens_on_a_weekend_a_closed_date_and_inside_a_closure() {
    let s = us_equities_with(vec![], vec![period("2026-10-24 10:00", "2026-10-24 12:00")]);
    assert!(!s.is_open(ny_ms(2026, 10, 24, 9, 59)));
    assert!(s.is_open(ny_ms(2026, 10, 24, 10, 0)));
    assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0)));
    // Thanksgiving is a closed date.
    let s = us_equities_with(vec![], vec![period("2026-11-26 10:00", "2026-11-26 12:00")]);
    assert!(s.is_open(ny_ms(2026, 11, 26, 11, 0)));
    assert!(!s.is_open(ny_ms(2026, 11, 26, 12, 0)));
    let s = globex(
        vec![period("2026-12-24 12:15", "2026-12-27 17:00")],
        vec![period("2026-12-25 08:00", "2026-12-25 10:00")],
    );
    assert!(s.is_open(chicago_ms(2026, 12, 25, 9, 0)));
    assert!(!s.is_open(chicago_ms(2026, 12, 25, 10, 0)));
}

#[test]
fn validation_refuses_exceptions_that_change_nothing_where_they_start() {
    let early_close = period("2026-11-27 13:00", "2026-11-27 16:00");
    assert_eq!(us_equities_with(vec![early_close], vec![]).validate(), Ok(()));
    let backwards = period("2026-11-27 16:00", "2026-11-27 13:00");
    let err = us_equities_with(vec![backwards], vec![]).validate().unwrap_err();
    assert!(err.contains("empty period"), "{err}");
    // Saturday, and Thanksgiving, are already shut.
    for shut in [period("2026-11-28 13:00", "2026-11-28 16:00"), period("2026-11-26 13:00", "2026-11-26 16:00")] {
        let err = us_equities_with(vec![shut], vec![]).validate().unwrap_err();
        assert!(err.contains("already shut"), "{err}");
    }
    let err = us_equities_with(vec![], vec![period("2026-10-20 10:00", "2026-10-20 11:00")]).validate().unwrap_err();
    assert!(err.contains("already open"), "{err}");
    let empty = period("2026-10-24 10:00", "2026-10-24 10:00");
    assert!(us_equities_with(vec![], vec![empty]).validate().unwrap_err().contains("empty period"));
    // A session inside the hour New York skips when clocks go forward never opens.
    let skipped = period("2027-03-14 02:00", "2027-03-14 02:30");
    let err = us_equities_with(vec![], vec![skipped]).validate().unwrap_err();
    assert!(err.contains("skips"), "{err}");
    // A special session inside a closure starts while the venue is shut.
    let closure = period("2026-12-24 12:15", "2026-12-27 17:00");
    let inside = period("2026-12-25 08:00", "2026-12-25 10:00");
    assert_eq!(globex(vec![closure], vec![inside]).validate(), Ok(()));
}

#[test]
fn has_exceptions_sees_either_kind() {
    let early_close = vec![period("2026-11-27 13:00", "2026-11-27 16:00")];
    let special = vec![period("2026-10-24 10:00", "2026-10-24 12:00")];
    assert!(!Schedule::AlwaysOpen.has_exceptions());
    assert!(!us_equities().has_exceptions());
    assert!(us_equities_with(early_close.clone(), vec![]).has_exceptions());
    assert!(us_equities_with(vec![], special.clone()).has_exceptions());
    assert!(us_equities_with(early_close, special).has_exceptions());
}

#[test]
fn a_schedule_without_exceptions_encodes_as_before_they_existed() {
    // Exactly the keys of the encoding the committed reference logs carry.
    let json = serde_json::to_value(us_equities()).unwrap();
    let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["closed_dates", "kind", "tz", "windows"]);
}

#[test]
fn exceptions_encode_as_local_date_times_and_round_trip() {
    let s = us_equities_with(
        vec![period("2026-11-27 13:00", "2026-11-27 16:00")],
        vec![period("2026-10-24 10:00", "2026-10-24 12:00")],
    );
    let json = serde_json::to_value(&s).unwrap();
    let early_close = serde_json::json!([{ "from": "2026-11-27T13:00:00", "to": "2026-11-27T16:00:00" }]);
    assert_eq!(json["closures"], early_close);
    assert_eq!(json["special_sessions"][0]["from"], "2026-10-24T10:00:00");
    assert_eq!(serde_json::from_value::<Schedule>(json).unwrap(), s);
}

#[test]
fn open_throughout_sees_every_closure_between_two_open_instants() {
    let s = us_equities_with(vec![period("2026-11-27 13:00", "2026-11-27 13:00:30")], vec![]);
    assert!(s.open_throughout(ny_ms(2026, 10, 20, 9, 30), ny_ms(2026, 10, 20, 15, 59)));
    // Across the close, a night and the next open; across a 30-second halt; and backwards.
    assert!(!s.open_throughout(ny_ms(2026, 10, 20, 15, 59), ny_ms(2026, 10, 21, 9, 31)));
    assert!(!s.open_throughout(ny_ms(2026, 11, 27, 12, 59), ny_ms(2026, 11, 27, 13, 1)));
    assert!(!s.open_throughout(ny_ms(2026, 10, 20, 15, 0), ny_ms(2026, 10, 20, 14, 0)));
    assert!(Schedule::AlwaysOpen.open_throughout(0, i64::MAX / 2));
    // A special session on a Saturday is open throughout, and not past its end.
    let s = us_equities_with(vec![], vec![period("2026-10-24 10:00", "2026-10-24 12:00")]);
    assert!(s.open_throughout(ny_ms(2026, 10, 24, 10, 0), ny_ms(2026, 10, 24, 11, 59)));
    assert!(!s.open_throughout(ny_ms(2026, 10, 24, 11, 59), ny_ms(2026, 10, 24, 12, 1)));
}

#[test]
fn week_time_round_trips() {
    let t: WeekTime = "Sun 17:05".parse().unwrap();
    assert_eq!(t.to_string(), "Sun 17:05");
    assert!("Xyz 10:00".parse::<WeekTime>().is_err());
    assert!("Mon 24:00".parse::<WeekTime>().is_err());
}

#[test]
fn a_weekend_gap_starts_at_the_last_close_before_a_closure_of_forty_hours() {
    let s = globex(vec![], vec![]);
    // From Tuesday morning the daily one-hour breaks pass, and Friday's 16:00 close starts the gap,
    // which Sunday's 17:00 open ends.
    let (friday, sunday) = (chicago_ms(2026, 10, 23, 16, 0), chicago_ms(2026, 10, 25, 17, 0));
    assert_eq!(s.next_weekend_gap(chicago_ms(2026, 10, 20, 9, 0)), Some((friday, sunday)));
    assert_eq!(s.next_weekend_gap(friday - 1), Some((friday, sunday)));
    // From the close itself, and from inside the gap, the next is a week on.
    let next = Some((chicago_ms(2026, 10, 30, 16, 0), chicago_ms(2026, 11, 1, 17, 0)));
    assert_eq!(s.next_weekend_gap(friday), next);
    assert_eq!(s.next_weekend_gap(chicago_ms(2026, 10, 25, 12, 0)), next);
    // Inside the gap, its reopening; in a daily break or while open, none.
    assert_eq!((s.weekend_gap_at(friday), s.weekend_gap_at(sunday - 1)), (Some(sunday), Some(sunday)));
    assert_eq!(s.weekend_gap_at(sunday), None);
    assert_eq!(s.weekend_gap_at(chicago_ms(2026, 10, 22, 16, 30)), None);
    assert_eq!(s.weekend_gap_at(chicago_ms(2026, 10, 22, 10, 0)), None);
    // FX shuts once a week; US equities' overnight closes are shorter than a gap.
    let fx = Schedule::Weekly {
        tz: chrono_tz::America::New_York,
        windows: vec![w("Sun 17:00", "Fri 17:00")],
        closed_dates: vec![],
        closures: vec![],
        special_sessions: vec![],
    };
    let close = |s: &Schedule, after| s.next_weekend_gap(after).map(|(close, _)| close);
    assert_eq!(close(&fx, ny_ms(2026, 10, 20, 9, 0)), Some(ny_ms(2026, 10, 23, 17, 0)));
    assert_eq!(close(&us_equities(), ny_ms(2026, 10, 19, 9, 0)), Some(ny_ms(2026, 10, 23, 16, 0)));
    assert_eq!(
        (Schedule::AlwaysOpen.next_weekend_gap(friday), Schedule::AlwaysOpen.weekend_gap_at(friday)),
        (None, None)
    );
}

#[test]
fn a_holiday_beside_the_weekend_moves_its_gap_start() {
    let close = |s: &Schedule, after| s.next_weekend_gap(after).map(|(close, _)| close);
    // An early close on the Friday after Thanksgiving starts the gap at noon.
    let black_friday = globex(vec![period("2026-11-27 12:00", "2026-11-27 16:00")], vec![]);
    let noon = chicago_ms(2026, 11, 27, 12, 0);
    assert_eq!(close(&black_friday, chicago_ms(2026, 11, 25, 9, 0)), Some(noon));
    // Good Friday's closure from Thursday evening starts it at Thursday's 16:00 close.
    let good_friday = globex(vec![period("2027-03-25 17:00", "2027-03-26 16:00")], vec![]);
    let thursday = chicago_ms(2027, 3, 25, 16, 0);
    assert_eq!(close(&good_friday, chicago_ms(2027, 3, 23, 9, 0)), Some(thursday));
    // A Monday holiday ends the gap later, and it starts at the same close.
    let osaka = osaka(vec![period("2026-11-23 08:45", "2026-11-24 06:00")]);
    let saturday = tokyo_ms(2026, 11, 21, 6, 0);
    let tuesday = tokyo_ms(2026, 11, 24, 8, 45);
    assert_eq!(osaka.next_weekend_gap(tokyo_ms(2026, 11, 18, 9, 0)), Some((saturday, tuesday)));
    assert_eq!(close(&osaka, saturday), Some(tokyo_ms(2026, 11, 28, 6, 0)));
    // A Wednesday holiday shuts US equities from Tuesday's close for 41.5 hours: a gap too.
    let Schedule::Weekly { tz, windows, .. } = us_equities() else { unreachable!("US equities keep weekly hours") };
    let closed_dates = vec![NaiveDate::from_ymd_opt(2029, 7, 4).unwrap()];
    let s = Schedule::Weekly { tz, windows, closed_dates, closures: vec![], special_sessions: vec![] };
    assert_eq!(close(&s, ny_ms(2029, 7, 2, 10, 0)), Some(ny_ms(2029, 7, 3, 16, 0)));
}

#[test]
fn only_a_closure_of_forty_hours_or_more_is_a_gap() {
    let close = |s: &Schedule, after| s.next_weekend_gap(after).map(|(close, _)| close);
    // Windows that leave a 39 h 59 m closure and then a 40 h one, every week.
    let s = Schedule::Weekly {
        tz: chrono_tz::UTC,
        windows: vec![w("Mon 00:00", "Tue 00:00"), w("Wed 15:59", "Fri 00:00"), w("Sat 16:00", "Mon 00:00")],
        closed_dates: vec![],
        closures: vec![],
        special_sessions: vec![],
    };
    let utc = |d, h, mi| chrono_tz::UTC.with_ymd_and_hms(2026, 10, d, h, mi, 0).unwrap().timestamp_millis();
    assert_eq!(s.next_weekend_gap(utc(19, 12, 0)), Some((utc(23, 0, 0), utc(24, 16, 0))));
    assert_eq!((s.weekend_gap_at(utc(20, 12, 0)), s.weekend_gap_at(utc(23, 12, 0))), (None, Some(utc(24, 16, 0))));
    // A special session splitting a gap leaves a gap only where a closure still lasts 40 hours.
    let split = us_equities_with(vec![], vec![period("2026-10-24 10:00", "2026-10-24 12:00")]);
    assert_eq!(close(&split, ny_ms(2026, 10, 22, 10, 0)), Some(ny_ms(2026, 10, 24, 12, 0)));
    assert_eq!(split.weekend_gap_at(ny_ms(2026, 10, 23, 18, 0)), None);
    // Hours with no closure that long have no gap within the search.
    let breaks = Schedule::Weekly {
        tz: chrono_tz::UTC,
        windows: DAYS.iter().map(|d| w(&format!("{d} 01:00"), &format!("{d} 23:00"))).collect(),
        closed_dates: vec![],
        closures: vec![],
        special_sessions: vec![],
    };
    assert_eq!(breaks.next_weekend_gap(utc(19, 12, 0)), None);
}

#[test]
fn a_long_closure_is_one_gap_to_its_reopening() {
    // Six weeks shut from a Monday noon halt to the Monday evening session seven weeks on.
    let s = globex(vec![period("2026-10-26 12:00", "2026-12-07 16:00")], vec![]);
    let (halt, reopen) = (chicago_ms(2026, 10, 26, 12, 0), chicago_ms(2026, 12, 7, 17, 0));
    assert_eq!(s.next_weekend_gap(chicago_ms(2026, 10, 25, 18, 0)), Some((halt, reopen)));
    assert_eq!(s.weekend_gap_at(chicago_ms(2026, 11, 15, 12, 0)), Some(reopen));
    assert_eq!(s.next_weekend_gap(reopen).map(|(close, _)| close), Some(chicago_ms(2026, 12, 11, 16, 0)));
}

#[test]
fn a_dated_edge_between_milliseconds_takes_effect_at_the_next_one() {
    // A Friday early close a nanosecond after noon shuts the market from the next millisecond.
    let noon = NaiveDate::from_ymd_opt(2026, 10, 23).unwrap().and_hms_nano_opt(12, 0, 0, 1).unwrap();
    let s =
        globex(vec![Period { from: noon, to: noon.with_nanosecond(0).unwrap() + chrono::TimeDelta::hours(4) }], vec![]);
    let close = chicago_ms(2026, 10, 23, 12, 0) + 1;
    assert_eq!(s.next_weekend_gap(chicago_ms(2026, 10, 23, 9, 0)).map(|(at, _)| at), Some(close));
}

#[test]
fn a_dated_edge_the_clocks_repeat_counts_at_both_instants() {
    // Ten seconds' session at 01:30:10 on Sunday 1 November 2026, in the hour New York's clocks go back
    // through, so it runs at 05:30:10 and again at 06:30:10 UTC; with Monday closed, the gap starts as
    // the second run ends, not the first.
    let session = period("2026-11-01 01:30:10", "2026-11-01 01:30:20");
    let s = us_equities_with(vec![period("2026-11-02 00:00", "2026-11-03 00:00")], vec![session]);
    let second = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 6, 30, 20).unwrap().timestamp_millis();
    let gap = s.next_weekend_gap(ny_ms(2026, 10, 30, 15, 0));
    assert_eq!(gap, Some((second, ny_ms(2026, 11, 3, 9, 30))));
}

// ── Helpers ──

/// A local `"YYYY-MM-DD HH:MM[:SS]"` period.
fn period(from: &str, to: &str) -> Period {
    let at = |s: &str| {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M"))
            .unwrap()
    };
    Period { from: at(from), to: at(to) }
}

/// `us_equities` with dated exceptions.
fn us_equities_with(closures: Vec<Period>, special_sessions: Vec<Period>) -> Schedule {
    let Schedule::Weekly { tz, windows, closed_dates, .. } = us_equities() else {
        unreachable!("US equities keep weekly hours")
    };
    Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions }
}

/// Futures hours in Chicago: Sunday 17:00 to Friday 16:00, with a daily 16:00-17:00 break.
fn globex(closures: Vec<Period>, special_sessions: Vec<Period>) -> Schedule {
    let windows = ["Sun", "Mon", "Tue", "Wed", "Thu"]
        .iter()
        .zip(&DAYS[..5])
        .map(|(open, close)| w(&format!("{open} 17:00"), &format!("{close} 16:00")));
    Schedule::Weekly {
        tz: chrono_tz::America::Chicago,
        windows: windows.collect(),
        closed_dates: vec![],
        closures,
        special_sessions,
    }
}

fn chicago_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::America::Chicago.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}

/// Osaka Exchange hours, Tokyo time: a day session and a night session to 06:00 the next day.
fn osaka(closures: Vec<Period>) -> Schedule {
    let days = &DAYS[..5];
    let day = days.iter().map(|d| w(&format!("{d} 08:45"), &format!("{d} 15:45")));
    let night = days.iter().zip(&DAYS[1..6]).map(|(d, next)| w(&format!("{d} 17:00"), &format!("{next} 06:00")));
    Schedule::Weekly {
        tz: chrono_tz::Asia::Tokyo,
        windows: day.chain(night).collect(),
        closed_dates: vec![],
        closures,
        special_sessions: vec![],
    }
}

fn tokyo_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::Asia::Tokyo.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}
