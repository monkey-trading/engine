//! The session's `V33` part: a relisting that raises `max_age_ms` cannot bring a stale held quote
//! back to pricing, the first command after a long gap checks held quotes for a closure at the
//! gap's two ends only, and a stamp far past the engine's schedule catches up a bounded batch of
//! rollovers and defers its command.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc, Weekday};
use chrono_tz::America::New_York;
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::MAX_CATCH_UP_BOUNDARIES;
use engine_core::event::Event;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AssetClass, MarketId, Millis};

use crate::common::{Harness, market, quote, us_equities};

/// A share with US hours and quotes fresh for an hour.
const HALT: MarketId = 31;

/// `V33` activates. A quote 75 minutes old arrives under the one-hour window and is held; a
/// relisting that doubles the window drops it rather than letting it price. The first command after
/// a three-day gap keeps the now-stale quotes, which cannot price. A stamp 400 days on catches up
/// `MAX_CATCH_UP_BOUNDARIES` boundaries and is deferred, and the next command continues.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V33 });
    let spec = market(HALT, "HALT", AssetClass::Equity, us_equities(), 2_000);
    let price_rules = PriceRules { max_age_ms: 3_600_000, ..spec.price_rules };
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..spec }));
    h.advance_to(next_weekday_at(h.ts, 11, 0));

    let stamped = h.ts - 75 * 60_000;
    h.cmd(Command::Prices {
        market: HALT,
        quotes: vec![quote("venue0", "100", stamped), quote("venue1", "100", stamped)],
    });
    assert_eq!(h.engine.markets()[&HALT].quotes.len(), 2);
    let spec = h.engine.markets()[&HALT].spec.clone();
    let price_rules = PriceRules { max_age_ms: 7_200_000, ..spec.price_rules };
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..spec }));
    assert!(h.engine.markets()[&HALT].quotes.is_empty());
    assert!(h.engine.markets()[&HALT].index(h.ts).is_err());

    h.price(HALT, "101");
    assert!(h.engine.markets()[&HALT].index(h.ts).is_ok());
    // The first command after a gap of days ends at an open session, so the stale quotes stay,
    // unpriceable, without the minute-by-minute closure walk.
    h.advance_to(next_weekday_at(h.ts + 3 * 86_400_000, 10, 0));
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markets()[&HALT].quotes.len(), 2);
    assert!(h.engine.markets()[&HALT].index(h.ts).is_err());
    h.price(HALT, "102");
    assert!(h.engine.markets()[&HALT].index(h.ts).is_ok());

    // A stamp far past the schedule is deferred after a bounded catch-up; the next command goes on.
    h.advance(400 * 86_400_000);
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. })), "{events:?}");
    let due = h.engine.next_rollover().expect("a rollover is scheduled");
    assert!(due < h.ts, "{MAX_CATCH_UP_BOUNDARIES} boundaries leave later ones waiting");
    h.cmd(Command::Tick);
}

/// The next weekday `hour`:`minute` New York time strictly after `ts`.
fn next_weekday_at(ts: Millis, hour: u32, minute: u32) -> Millis {
    let mut day = DateTime::<Utc>::from_timestamp_millis(ts).expect("in range").with_timezone(&New_York).date_naive();
    loop {
        let at = New_York
            .from_local_datetime(&day.and_hms_opt(hour, minute, 0).expect("a time"))
            .unwrap()
            .timestamp_millis();
        if at > ts && !matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
            return at;
        }
        day += Duration::days(1);
    }
}
