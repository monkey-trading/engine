//! The session's `V21` part: quotes from outside a market's hours, and a market priced from futures
//! that rolls onto its next contract.

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc, Weekday};
use chrono_tz::America::New_York;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Millis, Side};

use crate::common::{AAPL, Harness, market, quote};

/// US 500, priced from E-mini futures.
const US500: MarketId = 30;

/// `V21` activates. AAPL closes, and a quote after the close is refused; so is one 100ms before
/// the next open, which then fills nothing, until a quote after the open prices it. US 500 lists on
/// December E-minis with March next: it takes both, records March 40 higher, and refuses June's
/// quote and one naming no contract. At March's roll time it rolls on that gap without its index
/// moving; December's next quotes are dropped without a rejection, and `account` buys once March
/// has moved the index 10 higher.
pub(crate) fn session(h: &mut Harness, account: AccountId) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V21 });

    let close = next_weekday_at(h.ts, 16, 30);
    h.advance_to(close);
    h.cmd(Command::Tick);
    let after_hours = h.price(AAPL, "231");
    let refused = after_hours.iter().filter(|e| matches!(e, Event::PricesRejected { .. })).count();
    assert_eq!(refused, 2, "{after_hours:?}");
    let open = next_weekday_at(close, 9, 30);
    h.advance_to(open - 100);
    h.price(AAPL, "232");
    h.advance(100);
    let opened = h.cmd(Command::Tick);
    assert!(opened.contains(&Event::MarketOpened { market: AAPL }), "{opened:?}");
    h.advance(10);
    h.price(AAPL, "233");
    assert_eq!(h.engine.markets()[&AAPL].mark, Some("233".parse().unwrap()));

    let roll_at = h.ts + 3_600_000;
    let contracts = vec![
        contract("ESZ6", "2026-09-10T13:30:00Z".parse().unwrap()),
        contract("ESH7", utc(roll_at)),
        contract("ESM7", utc(roll_at + 86_400_000)),
    ];
    let spec = MarketSpec { contracts, ..market(US500, "US500", AssetClass::Index, Schedule::AlwaysOpen, 500) };
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    h.cmd(prices(h.ts, Some("ESZ6"), &["6000", "6001"]));
    h.cmd(prices(h.ts, Some("ESH7"), &["6040", "6041"]));
    h.cmd(prices(h.ts, Some("ESM7"), &["6080"]));
    h.cmd(prices(h.ts, None, &["6000"]));
    h.advance_to(roll_at);
    h.cmd(prices(h.ts, Some("ESZ6"), &["6000", "6001"]));
    let rolled = h.cmd(prices(h.ts, Some("ESH7"), &["6040", "6041"]));
    assert!(rolled.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{rolled:?}");
    h.advance(100);
    h.cmd(prices(h.ts, Some("ESZ6"), &["6100", "6101"]));
    h.cmd(prices(h.ts, Some("ESH7"), &["6050", "6051"]));
    let bought = h.market_order(account, US500, Side::Buy, "1", "6010.5");
    assert!(
        bought.iter().any(|e| matches!(e, Event::Fill(fill) if fill.index == "6010.5".parse().unwrap())),
        "{bought:?}"
    );
}

/// The first weekday time `hour:minute` in New York after `ts`.
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

/// `ts` as a UTC time.
fn utc(ts: Millis) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ts).expect("in range")
}

/// A contract rolling in at `from`.
fn contract(name: &str, from: DateTime<Utc>) -> FuturesContract {
    FuturesContract { contract: name.into(), from }
}

/// One venue per mid quoting `contract` for US 500, stamped `ts`.
fn prices(ts: Millis, contract: Option<&str>, mids: &[&str]) -> Command {
    let quotes = mids
        .iter()
        .enumerate()
        .map(|(i, mid)| SourceQuote { contract: contract.map(Into::into), ..quote(&format!("venue{i}"), mid, ts) })
        .collect();
    Command::Prices { market: US500, quotes }
}
