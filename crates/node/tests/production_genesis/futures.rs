//! The index and commodity markets' futures contracts, against the roll rules the owner set on
//! 2026-10-25 (#32), with each roll time recomputed from its exchange's calendar; and the engine
//! rolling each market onto each contract at its first command after that time.

use std::collections::BTreeMap;
use std::iter::successors;
use std::path::Path;

use chrono::{DateTime, Datelike, Days, Months, NaiveDate, TimeZone, Utc, Weekday};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use engine_node::NodeConfig;
use engine_node::config::Genesis;
use engine_node::sequencer::genesis_commands;

use crate::{DEV, production_genesis};

/// How a market's contracts roll off.
#[derive(Clone, Copy)]
enum Rule {
    /// 8 calendar days before expiry on the `friday`th Friday of the delivery month, at `at` UTC.
    Index { friday: u8, at: (u32, u32) },
    /// 2 CME business days before first notice, the last business day of the month before delivery.
    FirstNotice,
    /// 2 CME business days before WTI's last trade: 3 business days before the 25th of the month
    /// before delivery, or 4 when the 25th is not a business day.
    WtiLastTrade,
    /// 2 ICE business days before Brent's last trade, the last business day of the second month
    /// before delivery.
    BrentLastTrade,
}

const QUARTERLY: &[u32] = &[3, 6, 9, 12];
const MONTHLY: &[u32] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
const THIRD_FRIDAY: Rule = Rule::Index { friday: 3, at: (14, 30) };
/// CME's yen Nikkei expires when Osaka sets its final settlement, on the second Friday; 01:00 UTC
/// is in Osaka's day session.
const SECOND_FRIDAY: Rule = Rule::Index { friday: 2, at: (1, 0) };

/// Each futures market: its contracts' exchange root, the delivery months it rolls between, and
/// its rule.
const FUTURES: [(MarketId, &str, &[u32], Rule); 10] = [
    (20, "ES", QUARTERLY, THIRD_FRIDAY),
    (21, "NQ", QUARTERLY, THIRD_FRIDAY),
    (22, "YM", QUARTERLY, THIRD_FRIDAY),
    (23, "FDAX", QUARTERLY, THIRD_FRIDAY),
    (24, "Z", QUARTERLY, THIRD_FRIDAY),
    (25, "NIY", QUARTERLY, SECOND_FRIDAY),
    (30, "GC", &[2, 4, 6, 8, 10, 12], Rule::FirstNotice),
    (31, "SI", &[3, 5, 7, 9, 12], Rule::FirstNotice),
    (32, "CL", MONTHLY, Rule::WtiLastTrade),
    (33, "BRN", MONTHLY, Rule::BrentLastTrade),
];

const MONTH_CODES: &[u8; 12] = b"FGHJKMNQUVXZ";

/// The US holidays on which CME settles nothing, from the first roll to the last:
/// https://www.cmegroup.com/trading-hours.html
const CME_HOLIDAYS: &[&str] = &[
    "2026-09-07",
    "2026-11-26",
    "2026-12-25",
    "2027-01-01",
    "2027-01-18",
    "2027-02-15",
    "2027-03-26",
    "2027-05-31",
    "2027-06-18",
    "2027-07-05",
    "2027-09-06",
    "2027-11-25",
    "2027-12-24",
    "2028-01-17",
    "2028-02-21",
];

/// The weekdays ICE's Brent expiry calendar does not count as business days: England's bank
/// holidays (https://www.gov.uk/bank-holidays.json), and 24 and 31 December, as February 2027 and
/// 2028 Brent expire on 30 December: https://www.ice.com/products/219/Brent-Crude-Futures/expiry
const ICE_NON_BUSINESS_DAYS: &[&str] = &[
    "2026-08-31",
    "2026-12-24",
    "2026-12-25",
    "2026-12-28",
    "2026-12-31",
    "2027-01-01",
    "2027-03-26",
    "2027-03-29",
    "2027-05-03",
    "2027-05-31",
    "2027-08-30",
    "2027-12-24",
    "2027-12-27",
    "2027-12-28",
    "2027-12-31",
    "2028-01-03",
];

#[test]
fn each_futures_market_rolls_through_its_active_months_by_the_owners_rules() {
    let genesis = production_genesis("futures-rolls");
    let decided = Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap();
    for (id, root, months, rule) in FUTURES {
        let market = listing(&genesis, id);
        let symbol = &market.symbol;
        let cycle: Vec<(i32, u32)> = (2026..=2028).flat_map(|year| months.iter().map(move |m| (year, *m))).collect();
        let delivery: Vec<(i32, u32)> = market.contracts.iter().map(|c| delivery_month(&c.contract, root)).collect();
        let first = cycle.iter().position(|month| *month == delivery[0]).expect("an active month");
        assert_eq!(delivery, cycle[first..first + delivery.len()], "{symbol}: consecutive active months");
        // Each contract is rolled onto as the one before it in the cycle rolls off, in market hours.
        for (contract, before) in market.contracts.iter().zip(&cycle[first - 1..]) {
            assert_eq!(contract.from, roll_off(rule, *before), "{symbol} {}", contract.contract);
            let open = market.schedule.is_open(contract.from.timestamp_millis());
            assert!(open, "{symbol} rolls onto {} while shut", contract.contract);
        }
        // The first contract was the front when the rules were set, and the last is rolled onto in
        // December 2027 or later, past the calendars' last month.
        assert!(market.contracts[0].from < decided && decided < market.contracts[1].from, "{symbol}");
        let last = market.contracts.last().expect("contracts").from;
        assert!(last >= Utc.with_ymd_and_hms(2027, 12, 1, 0, 0, 0).unwrap(), "{symbol}");
    }
    let futures: Vec<MarketId> = FUTURES.iter().map(|(id, ..)| *id).collect();
    assert!(genesis.markets.iter().filter(|m| !futures.contains(&m.id)).all(|m| m.contracts.is_empty()));
    // Anchors read off the exchanges' calendars.
    for (id, contract, from) in [
        // ES December expires Fri 18 Dec 2026.
        (20, "ESH7", "2026-12-10T14:30:00Z"),
        // NIY December settles Fri 11 Dec 2026.
        (25, "NIYH7", "2026-12-03T01:00:00Z"),
        // GC December's first notice is Mon 30 Nov 2026; Thanksgiving, 26 Nov, is skipped.
        (30, "GCG7", "2026-11-25T14:30:00Z"),
        // CL December's last trade is Fri 20 Nov 2026.
        (32, "CLF7", "2026-11-18T14:30:00Z"),
        // BRN February's last trade is Wed 30 Dec 2026; 24, 25 and 28 Dec are skipped.
        (33, "BRNH7", "2026-12-23T14:30:00Z"),
    ] {
        let listed = listing(&genesis, id).contracts.iter().find(|c| c.contract == contract).expect("listed");
        assert_eq!(listed.from, from.parse::<DateTime<Utc>>().unwrap(), "{contract}");
    }
}

#[test]
fn the_engine_rolls_each_futures_market_onto_each_contract_at_its_roll_time() {
    let genesis = production_genesis("futures-engine-rolls");
    let mut engine = sequenced(&genesis, Utc.with_ymd_and_hms(2026, 10, 25, 0, 0, 0).unwrap().timestamp_millis());
    // Each roll: the market, the contracts it rolls from and onto, and how many rolls it has made.
    let mut rolls: BTreeMap<Millis, Vec<(MarketId, &str, &str, i64)>> = BTreeMap::new();
    for (id, ..) in FUTURES {
        for (made, pair) in (1..).zip(listing(&genesis, id).contracts.windows(2)) {
            let roll = (id, pair[0].contract.as_str(), pair[1].contract.as_str(), made);
            rolls.entry(pair[1].from.timestamp_millis()).or_default().push(roll);
        }
    }
    for (at, markets) in rolls {
        // Two sources quote the front contract at 100, then the next at 101, just before the roll
        // time, so each roll's gap is -1.
        for (market, front, _, _) in &markets {
            sequence(&mut engine, at - 500, prices(*market, front, 100, at - 500));
        }
        for (market, _, next, _) in &markets {
            sequence(&mut engine, at - 400, prices(*market, next, 101, at - 400));
        }
        let mut rolled = sequence(&mut engine, at + 100, Command::Tick);
        rolled.retain(|event| matches!(event, Event::MarketRolled { .. }));
        let expected: Vec<Event> = markets
            .iter()
            .map(|(market, _, next, made)| Event::MarketRolled {
                market: *market,
                contract: (*next).into(),
                adjustment: Fixed::from_int(-made),
            })
            .collect();
        assert_eq!(rolled, expected, "at {at}");
    }
}

// ── Helpers ──

/// Genesis market `id`'s listing.
fn listing(genesis: &Genesis, id: MarketId) -> &MarketSpec {
    genesis.markets.iter().find(|market| market.id == id).expect("listed")
}

/// The delivery month of a contract named `root`, a month code, and the year's last digit.
fn delivery_month(contract: &str, root: &str) -> (i32, u32) {
    let code = contract.strip_prefix(root).unwrap_or_else(|| panic!("{contract} is not a {root} contract"));
    let &[month, year] = code.as_bytes() else { panic!("{contract}: not a month code and a year") };
    let month = MONTH_CODES.iter().position(|c| *c == month).expect("a month code");
    assert!(year.is_ascii_digit(), "{contract}");
    (2020 + i32::from(year - b'0'), u32::try_from(month).unwrap() + 1)
}

/// When the index rolls off the contract for delivery in `month`, under `rule`.
fn roll_off(rule: Rule, month: (i32, u32)) -> DateTime<Utc> {
    let (day, (hour, minute)) = match rule {
        Rule::Index { friday, at } => {
            let expiry = NaiveDate::from_weekday_of_month_opt(month.0, month.1, Weekday::Fri, friday).unwrap();
            (expiry.checked_sub_days(Days::new(8)).unwrap(), at)
        }
        Rule::FirstNotice => {
            let first_notice = last_business_day(months_before(month, 1), CME_HOLIDAYS);
            (business_days_before(first_notice, 2, CME_HOLIDAYS), (14, 30))
        }
        Rule::WtiLastTrade => {
            let (year, before) = months_before(month, 1);
            let the_25th = NaiveDate::from_ymd_opt(year, before, 25).unwrap();
            let back = if business_day(the_25th, CME_HOLIDAYS) { 3 } else { 4 };
            let last_trade = business_days_before(the_25th, back, CME_HOLIDAYS);
            (business_days_before(last_trade, 2, CME_HOLIDAYS), (14, 30))
        }
        Rule::BrentLastTrade => {
            let last_trade = last_business_day(months_before(month, 2), ICE_NON_BUSINESS_DAYS);
            (business_days_before(last_trade, 2, ICE_NON_BUSINESS_DAYS), (14, 30))
        }
    };
    day.and_hms_opt(hour, minute, 0).unwrap().and_utc()
}

/// The month `n` months before `(year, month)`.
fn months_before((year, month): (i32, u32), n: u32) -> (i32, u32) {
    let day = NaiveDate::from_ymd_opt(year, month, 1).unwrap().checked_sub_months(Months::new(n)).unwrap();
    (day.year(), day.month())
}

/// Whether `day` is a weekday not in `holidays`.
fn business_day(day: NaiveDate, holidays: &[&str]) -> bool {
    !matches!(day.weekday(), Weekday::Sat | Weekday::Sun) && !holidays.contains(&day.to_string().as_str())
}

/// The `n`th business day before `day`.
fn business_days_before(day: NaiveDate, n: usize, holidays: &[&str]) -> NaiveDate {
    let earlier = successors(day.pred_opt(), |day| day.pred_opt());
    earlier.filter(|day| business_day(*day, holidays)).nth(n - 1).expect("a business day")
}

/// The last business day of `(year, month)`.
fn last_business_day((year, month): (i32, u32), holidays: &[&str]) -> NaiveDate {
    let next = NaiveDate::from_ymd_opt(year, month, 1).unwrap().checked_add_months(Months::new(1)).unwrap();
    business_days_before(next, 1, holidays)
}

/// An engine that sequenced `genesis` at `start` as a node does: after client auth, the operator,
/// and the latest protocol version.
fn sequenced(genesis: &Genesis, start: Millis) -> Engine {
    let operator = NodeConfig::load(Path::new(DEV)).unwrap().operator().unwrap();
    let activation = [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(operator),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST },
    ];
    let mut engine = Engine::new();
    for cmd in activation.into_iter().map(Command::Admin).chain(genesis_commands(genesis)) {
        let events = sequence(&mut engine, start, cmd);
        assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
    }
    engine
}

/// Applies `cmd` as the engine's next seq at `ts`; panics if the engine refuses a quote.
fn sequence(engine: &mut Engine, ts: Millis, cmd: Command) -> Vec<Event> {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    let events = engine.apply(&Sequenced { seq, ts, cmd }).expect("in sequence");
    assert!(!events.iter().any(|e| matches!(e, Event::PricesRejected { .. })), "{events:?}");
    events
}

/// Two sources' quotes for `market`'s `contract` at `mid`, stamped at `ts`.
fn prices(market: MarketId, contract: &str, mid: i64, ts: Millis) -> Command {
    let quote = |source: &str| SourceQuote {
        source: source.into(),
        mid: Fixed::from_int(mid),
        ts,
        contract: Some(contract.into()),
    };
    Command::Prices { market, quotes: vec![quote("a"), quote("b")] }
}
