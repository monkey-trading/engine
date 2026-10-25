//! The production genesis: it loads as a dev node's genesis, every market validates and carries the
//! launch decisions, and markets on one venue share one calendar. Sequenced as the node sequences
//! it, each market opens and closes on its venue's hours through weeks with holidays and early
//! closes. `divergence` checks the stress levels each market reaches, and `futures` the contracts
//! the index and commodity markets roll through.

#[path = "common/access.rs"]
pub mod access;
mod divergence;
mod futures;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc, Weekday};
use chrono_tz::Tz;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AssetClass, MarketId, Millis};
use engine_core::{Engine, Fixed};
use engine_node::NodeConfig;
use engine_node::config::Genesis;
use engine_node::sequencer::genesis_commands;

const DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/dev.toml");
const PRODUCTION_GENESIS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/production-genesis.toml");

const CRYPTO: [MarketId; 3] = [1, 2, 3];
const CBOE_FX: [MarketId; 4] = [10, 11, 12, 13];
const CME_EQUITY: [MarketId; 3] = [20, 21, 22];
const CME_METALS_ENERGY: [MarketId; 3] = [30, 31, 32];
const EUREX: [MarketId; 1] = [23];
const ICE_FTSE: [MarketId; 1] = [24];
const JPX: [MarketId; 1] = [25];
const ICE_BRENT: [MarketId; 1] = [33];
const US_SHARES: [MarketId; 20] = [40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59];

/// The launch catalogue: #7's majors and 20 US large-cap shares, in listing order.
const SYMBOLS: [&str; 37] = [
    "BTC-USD", "ETH-USD", "SOL-USD", "EUR-USD", "GBP-USD", "USD-JPY", "AUD-USD", "US500", "US100", "US30", "DE40",
    "UK100", "JP225", "XAU-USD", "XAG-USD", "WTI", "BRENT", "AAPL", "MSFT", "NVDA", "AMZN", "GOOGL", "META", "TSLA",
    "AVGO", "BRK.B", "JPM", "V", "MA", "LLY", "JNJ", "XOM", "WMT", "COST", "NFLX", "AMD", "ORCL",
];

const MINUTE: Millis = 60_000;
const LAUNCH_BENCHMARKS: [&str; 5] = ["SOFR", "ESTR", "SONIA", "TONA", "AONIA"];

/// Venues' markets, the venue's time zone, and its opens (`true`) and closes (`false`) in local
/// `"YYYY-MM-DD HH:MM"` time.
type Week = Vec<(&'static [MarketId], Tz, Vec<(String, bool)>)>;

#[test]
fn a_dev_node_loads_the_production_genesis_and_every_market_validates() {
    let genesis = production_genesis("validates");
    genesis.params.validate_for(ProtocolVersion::LATEST).unwrap();
    for market in &genesis.markets {
        market.validate().unwrap_or_else(|e| panic!("{}: {e}", market.symbol));
    }
    let symbols: Vec<&str> = genesis.markets.iter().map(|m| m.symbol.as_str()).collect();
    assert_eq!(symbols, SYMBOLS);
    let ids: Vec<MarketId> = genesis.markets.iter().map(|m| m.id).collect();
    let venues =
        [&CRYPTO[..], &CBOE_FX, &CME_EQUITY, &CME_METALS_ENERGY, &EUREX, &ICE_FTSE, &JPX, &ICE_BRENT, &US_SHARES]
            .concat();
    assert_eq!(ids.len(), venues.len());
    assert!(venues.iter().all(|id| ids.contains(id)), "{ids:?}");
}

#[test]
fn the_production_genesis_applies_the_launch_decisions() {
    let production = production_genesis("launch-decisions");
    let dev = NodeConfig::load(Path::new(DEV)).unwrap().genesis;
    // A 10% profit fee (#9); the other parameters, stress levels included, are dev's, except the
    // total cap floor, whose production value is not chosen yet.
    assert_eq!(production.params.profit_fee_bps, 1_000);
    let floor = dev.params.house_total_cap_floor;
    assert_eq!(ProtocolParams { house_total_cap_floor: floor, ..production.params.clone() }, dev.params);
    // The house starts with no capital and builds it from trader losses, and no benchmark rate is
    // set until the daily benchmark feed sequences one.
    assert_eq!(production.house_capital, Fixed::ZERO);
    assert!(production.benchmarks.is_empty());
    for market in &production.markets {
        let symbol = &market.symbol;
        let markup = match market.asset_class {
            AssetClass::Index | AssetClass::Commodity | AssetClass::Equity => 250,
            AssetClass::Fx => 100,
            AssetClass::Crypto => 500,
            other => panic!("{symbol}: no launch market is {other:?}"),
        };
        assert_eq!(market.financing.markup_bps, markup, "{symbol}");
        assert!(LAUNCH_BENCHMARKS.contains(&market.financing.benchmark.as_str()), "{symbol}");
        // Professional-client margin (#8): 200:1 on FX majors and major indices, less leverage on
        // everything else, and bands that rise with position notional.
        let first_band = market.margin_tiers[0].bps;
        match market.asset_class {
            AssetClass::Fx | AssetClass::Index => assert_eq!(first_band, 50, "{symbol}"),
            _ => assert!(first_band > 50, "{symbol}"),
        }
        assert!(market.margin_tiers.windows(2).all(|w| w[0].bps < w[1].bps), "{symbol}");
        // House caps are dev's for the same market or asset class, except SOL-USD's 10%.
        let class_caps: Vec<i64> =
            dev.markets.iter().filter(|d| d.asset_class == market.asset_class).map(|d| d.house_cap_bps).collect();
        match dev.markets.iter().find(|d| d.id == market.id) {
            Some(same) => assert_eq!(market.house_cap_bps, same.house_cap_bps, "{symbol}"),
            None if symbol == "SOL-USD" => assert_eq!(market.house_cap_bps, 1_000),
            None => assert!(class_caps.contains(&market.house_cap_bps), "{symbol}"),
        }
        // Size spreads are dev's for the same market, or for a dev market of the same asset class.
        match dev.markets.iter().find(|d| d.id == market.id) {
            Some(same) => assert_eq!(market.spread_tiers, same.spread_tiers, "{symbol}"),
            None => assert!(
                dev.markets
                    .iter()
                    .any(|d| d.asset_class == market.asset_class && d.spread_tiers == market.spread_tiers),
                "{symbol}"
            ),
        }
    }
}

#[test]
fn the_launch_markets_quoted_outside_usd_convert_at_their_pairs() {
    let production = production_genesis("quote-currencies");
    let quoted: BTreeMap<&str, QuoteCurrency> =
        production.markets.iter().map(|m| (m.symbol.as_str(), m.quote_currency)).collect();
    let (eur, gbp, jpy) = (
        QuoteCurrency::Eur { fx_market: 10 },
        QuoteCurrency::Gbp { fx_market: 11 },
        QuoteCurrency::Jpy { fx_market: 12 },
    );
    let expected = [("USD-JPY", jpy), ("DE40", eur), ("UK100", gbp), ("JP225", jpy)];
    for (symbol, currency) in quoted {
        let want = expected.iter().find(|(s, _)| *s == symbol).map_or(QuoteCurrency::Usd, |(_, c)| *c);
        assert_eq!(currency, want, "{symbol}");
    }
}

#[test]
fn markets_on_one_venue_share_one_calendar() {
    let genesis = production_genesis("one-calendar");
    let schedule = |id: MarketId| &genesis.markets.iter().find(|m| m.id == id).expect("listed").schedule;
    for venue in [&CBOE_FX[..], &CME_EQUITY, &CME_METALS_ENERGY, &US_SHARES] {
        assert!(venue.iter().all(|id| schedule(*id) == schedule(venue[0])), "{venue:?}");
    }
}

#[test]
fn germany_40_opens_at_01_10_in_winter_and_02_10_in_summer() {
    let genesis = production_genesis("germany-40");
    let schedule = &genesis.markets.iter().find(|m| m.id == EUREX[0]).expect("listed").schedule;
    let berlin = chrono_tz::Europe::Berlin;
    // Eurex's open is fixed to UTC: 01:10 CET in winter, 02:10 CEST in summer.
    for (day, opens_at_01_10) in
        [("2026-11-02", true), ("2027-02-15", true), ("2027-11-02", true), ("2027-06-15", false)]
    {
        assert!(!schedule.is_open(at(berlin, &format!("{day} 01:09"))), "{day}");
        assert_eq!(schedule.is_open(at(berlin, &format!("{day} 01:10"))), opens_at_01_10, "{day}");
        assert!(schedule.is_open(at(berlin, &format!("{day} 02:10"))), "{day}");
        assert!(!schedule.is_open(at(berlin, &format!("{day} 22:00"))), "{day}");
    }
    // A closed date stays shut through the winter first hour.
    assert!(!schedule.is_open(at(berlin, "2027-03-26 01:30")));
}

#[test]
fn germany_40_has_a_winter_first_hour_on_every_cet_weekday_eurex_trades() {
    let genesis = production_genesis("germany-40-winter");
    let schedule = &genesis.markets.iter().find(|m| m.id == EUREX[0]).expect("listed").schedule;
    let Schedule::Weekly { closed_dates, special_sessions, .. } = schedule else { panic!("DE40 keeps weekly hours") };
    let berlin = chrono_tz::Europe::Berlin;
    let first = NaiveDate::from_ymd_opt(2026, 10, 20).expect("a date");
    let last = NaiveDate::from_ymd_opt(2027, 12, 31).expect("a date");
    let expected: Vec<Period> = first
        .iter_days()
        .take_while(|day| *day <= last)
        .filter(|day| !matches!(day.weekday(), Weekday::Sat | Weekday::Sun) && !closed_dates.contains(day))
        // CET is UTC+1; CEST days take Eurex's 02:10 open from the weekly windows.
        .filter(|day| {
            let noon = berlin.from_local_datetime(&day.and_hms_opt(12, 0, 0).expect("a time")).unwrap();
            noon.offset().fix().local_minus_utc() == 3_600
        })
        .map(|day| Period { from: day.and_hms_opt(1, 10, 0).unwrap(), to: day.and_hms_opt(2, 10, 0).unwrap() })
        .collect();
    assert_eq!(special_sessions, &expected);
}

#[test]
fn thanksgiving_week_opens_and_closes_each_market_on_its_venue_hours() {
    assert_week("thanksgiving", utc(2026, 11, 21, 12), utc(2026, 11, 29, 12), thanksgiving_week());
}

#[test]
fn christmas_week_opens_and_closes_each_market_on_its_venue_hours() {
    assert_week("christmas", utc(2026, 12, 19, 12), utc(2026, 12, 27, 12), christmas_week());
}

#[test]
fn easter_week_2027_opens_and_closes_each_market_on_its_venue_hours() {
    assert_week("easter", utc(2027, 3, 20, 12), utc(2027, 3, 28, 12), easter_week_2027());
}

// ── Helpers ──

/// Sequences the production genesis at `start` as the node does, after client auth, the operator,
/// and the latest protocol version, which leaves the house with no capital. Then ticks once a
/// minute until `end` and checks every market opens and closes exactly as `expected` says, while
/// crypto never closes. Both ends fall on a weekend when every other venue is shut.
fn assert_week(test: &str, start: Millis, end: Millis, expected: Week) {
    let mut engine = Engine::new();
    let mut seq = 0;
    let mut apply = |engine: &mut Engine, ts, cmd| {
        let events = engine.apply(&Sequenced { seq, ts, cmd }).expect("in sequence");
        seq += 1;
        events
    };
    // The node records client auth and the operator and activates the latest protocol version
    // before genesis.
    let operator = NodeConfig::load(Path::new(DEV)).unwrap().operator().unwrap();
    let activation = [
        Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
        Command::Admin(AdminCommand::SetOperator(operator)),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }),
    ];
    for cmd in activation.into_iter().chain(genesis_commands(&production_genesis(test))) {
        let events = apply(&mut engine, start, cmd);
        assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
    }
    let open: Vec<MarketId> = engine.markets().iter().filter(|(_, m)| m.open).map(|(id, _)| *id).collect();
    assert_eq!(open, CRYPTO);
    assert_eq!(engine.house_cash(), Fixed::ZERO);

    let mut transitions: BTreeMap<MarketId, Vec<(Millis, bool)>> = BTreeMap::new();
    for ts in (start + MINUTE..=end).step_by(MINUTE as usize) {
        for event in apply(&mut engine, ts, Command::Tick) {
            match event {
                Event::MarketOpened { market } => transitions.entry(market).or_default().push((ts, true)),
                Event::MarketClosed { market } => transitions.entry(market).or_default().push((ts, false)),
                _ => {}
            }
        }
    }
    for (markets, tz, expected) in expected {
        let mut expected: Vec<(Millis, bool)> = expected.iter().map(|(local, open)| (at(tz, local), *open)).collect();
        expected.sort();
        for market in markets {
            assert_eq!(transitions.remove(market).unwrap_or_default(), expected, "market {market}");
        }
    }
    assert!(transitions.is_empty(), "{transitions:?}");
}

/// Each venue's opens (`true`) and closes (`false`) from Sat 21 to Sun 29 Nov 2026, in local time.
fn thanksgiving_week() -> Week {
    let (mon, tue, wed, thu, fri) = ("2026-11-23", "2026-11-24", "2026-11-25", "2026-11-26", "2026-11-27");
    let new_york = chrono_tz::America::New_York;
    let chicago = chrono_tz::America::Chicago;
    let cme_thanksgiving = [
        overnight(&["2026-11-22", mon, tue, wed], "17:00", "16:00"),
        listed(&[("2026-11-25 17:00", true), ("2026-11-26 12:00", false)]),
        listed(&[("2026-11-26 17:00", true), ("2026-11-27 12:00", false)]),
    ]
    .concat();
    vec![
        // Thanksgiving: closed Thursday, 13:00 close on Friday.
        (
            &US_SHARES,
            new_york,
            [sessions(&[mon, tue, wed], "09:30", "16:00"), sessions(&[fri], "09:30", "13:00")].concat(),
        ),
        // The FX week does not stop for US holidays.
        (&CBOE_FX, new_york, listed(&[("2026-11-22 17:00", true), ("2026-11-27 17:00", false)])),
        // CME halts at 12:00 CT on Thanksgiving, reopens at 17:00, and closes at 12:00 CT the day after.
        (&CME_EQUITY, chicago, cme_thanksgiving.clone()),
        (&CME_METALS_ENERGY, chicago, cme_thanksgiving),
        // Early closes at 13:30 ET on Thanksgiving and 15:00 ET the day after.
        (
            &ICE_BRENT,
            new_york,
            [
                overnight(&["2026-11-22", mon], "18:00", "18:00"),
                overnight(&[mon, tue, wed], "20:00", "18:00"),
                listed(&[("2026-11-25 20:00", true), ("2026-11-26 13:30", false)]),
                listed(&[("2026-11-26 20:00", true), ("2026-11-27 15:00", false)]),
            ]
            .concat(),
        ),
        (&ICE_FTSE, chrono_tz::Europe::London, sessions(&[mon, tue, wed, thu, fri], "01:00", "21:00")),
        // Winter: Eurex opens at 01:10 CET.
        (&EUREX, chrono_tz::Europe::Berlin, sessions(&[mon, tue, wed, thu, fri], "01:10", "22:00")),
        // Monday is shut for JPX's BCP test, day and night session.
        (
            &JPX,
            chrono_tz::Asia::Tokyo,
            [
                sessions(&[tue, wed, thu, fri], "08:45", "15:45"),
                overnight(&[tue, wed, thu, fri, "2026-11-28"], "17:00", "06:00"),
            ]
            .concat(),
        ),
    ]
}

/// Each venue's opens (`true`) and closes (`false`) from Sat 19 to Sun 27 Dec 2026, in local time.
fn christmas_week() -> Week {
    let (mon, tue, wed, thu, fri) = ("2026-12-21", "2026-12-22", "2026-12-23", "2026-12-24", "2026-12-25");
    let new_york = chrono_tz::America::New_York;
    let chicago = chrono_tz::America::Chicago;
    let cme_christmas = [
        overnight(&["2026-12-20", mon, tue, wed], "17:00", "16:00"),
        listed(&[("2026-12-23 17:00", true), ("2026-12-24 12:00", false)]),
    ]
    .concat();
    vec![
        // A 13:00 close on Christmas Eve, and closed on Christmas Day.
        (
            &US_SHARES,
            new_york,
            [sessions(&[mon, tue, wed], "09:30", "16:00"), sessions(&[thu], "09:30", "13:00")].concat(),
        ),
        // Shut from 17:00 ET on Christmas Eve, when Christmas Day's trading day would start.
        (&CBOE_FX, new_york, listed(&[("2026-12-20 17:00", true), ("2026-12-24 17:00", false)])),
        // CME closes at 12:00 CT on Christmas Eve and stays shut through Christmas Day.
        (&CME_EQUITY, chicago, cme_christmas.clone()),
        (&CME_METALS_ENERGY, chicago, cme_christmas),
        // Trading ceases at 14:00 ET on Christmas Eve and stays shut through Christmas Day.
        (
            &ICE_BRENT,
            new_york,
            [
                overnight(&["2026-12-20", mon], "18:00", "18:00"),
                overnight(&[mon, tue, wed], "20:00", "18:00"),
                listed(&[("2026-12-23 20:00", true), ("2026-12-24 14:00", false)]),
            ]
            .concat(),
        ),
        // Closes at 12:30 on Christmas Eve with the London market, and shut on Christmas Day.
        (
            &ICE_FTSE,
            chrono_tz::Europe::London,
            [sessions(&[mon, tue, wed], "01:00", "21:00"), sessions(&[thu], "01:00", "12:30")].concat(),
        ),
        // Closed on Christmas Eve and Christmas Day; the winter open is 01:10 CET.
        (&EUREX, chrono_tz::Europe::Berlin, sessions(&[mon, tue, wed], "01:10", "22:00")),
        // No Japanese holiday this week.
        (
            &JPX,
            chrono_tz::Asia::Tokyo,
            [
                sessions(&[mon, tue, wed, thu, fri], "08:45", "15:45"),
                overnight(&[mon, tue, wed, thu, fri, "2026-12-26"], "17:00", "06:00"),
            ]
            .concat(),
        ),
    ]
}

/// Each venue's opens (`true`) and closes (`false`) from Sat 20 to Sun 28 Mar 2027, in local time.
fn easter_week_2027() -> Week {
    let (mon, tue, wed, thu, fri) = ("2027-03-22", "2027-03-23", "2027-03-24", "2027-03-25", "2027-03-26");
    let new_york = chrono_tz::America::New_York;
    let chicago = chrono_tz::America::Chicago;
    let cme_easter = overnight(&["2027-03-21", mon, tue, wed, thu], "17:00", "16:00");
    vec![
        // Closed on Good Friday.
        (&US_SHARES, new_york, sessions(&[mon, tue, wed, thu], "09:30", "16:00")),
        // Cboe FX publishes no Good Friday closure.
        (&CBOE_FX, new_york, listed(&[("2027-03-21 17:00", true), ("2027-03-26 17:00", false)])),
        // CME is shut on Good Friday, so Thursday evening's session does not open.
        (&CME_EQUITY, chicago, cme_easter.clone()),
        (&CME_METALS_ENERGY, chicago, cme_easter),
        // Closed on Good Friday, so Thursday evening's session does not open.
        (
            &ICE_BRENT,
            new_york,
            [overnight(&["2027-03-21", mon], "18:00", "18:00"), overnight(&[mon, tue, wed, thu], "20:00", "18:00")]
                .concat(),
        ),
        // Shut on Good Friday, an England and Wales bank holiday.
        (&ICE_FTSE, chrono_tz::Europe::London, sessions(&[mon, tue, wed, thu], "01:00", "21:00")),
        // The winter 01:10 CET open, and closed on Good Friday.
        (&EUREX, chrono_tz::Europe::Berlin, sessions(&[mon, tue, wed, thu], "01:10", "22:00")),
        // Monday's holiday is a JPX holiday-trading day.
        (
            &JPX,
            chrono_tz::Asia::Tokyo,
            [
                sessions(&[mon, tue, wed, thu, fri], "08:45", "15:45"),
                overnight(&[mon, tue, wed, thu, fri, "2027-03-27"], "17:00", "06:00"),
            ]
            .concat(),
        ),
    ]
}

/// An open at `open` and a close at `close` on each day.
fn sessions(days: &[&str], open: &str, close: &str) -> Vec<(String, bool)> {
    days.iter().flat_map(|d| [(format!("{d} {open}"), true), (format!("{d} {close}"), false)]).collect()
}

/// Sessions that open on each listed day at `open` and close on the next listed day at `close`.
fn overnight(days: &[&str], open: &str, close: &str) -> Vec<(String, bool)> {
    days.windows(2).flat_map(|w| [(format!("{} {open}", w[0]), true), (format!("{} {close}", w[1]), false)]).collect()
}

/// Opens and closes written out.
fn listed(transitions: &[(&str, bool)]) -> Vec<(String, bool)> {
    transitions.iter().map(|(local, open)| (local.to_string(), *open)).collect()
}

/// The production genesis as a dev node loads it, through a config file named for `test` so tests
/// running at once never read each other's half-written file.
fn production_genesis(test: &str) -> Genesis {
    dev_node_on_production_genesis(test).genesis
}

/// `config/dev.toml`'s node sections with the production genesis in place of its own, loaded and
/// validated by the node's loader.
fn dev_node_on_production_genesis(test: &str) -> NodeConfig {
    let dev = fs::read_to_string(DEV).unwrap();
    let (node_sections, _) = dev.split_once("\n[genesis]\n").expect("dev.toml ends with its [genesis] section");
    let production = fs::read_to_string(PRODUCTION_GENESIS).unwrap();
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dev-on-production-genesis-{test}.toml"));
    fs::write(&path, format!("{node_sections}\n{production}")).unwrap();
    NodeConfig::load(&path).unwrap()
}

/// A local wall-clock time `"YYYY-MM-DD HH:MM"` in `tz`, as epoch millis.
fn at(tz: Tz, local: &str) -> Millis {
    let naive = NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M").unwrap();
    tz.from_local_datetime(&naive).single().unwrap().timestamp_millis()
}

/// A UTC time on the hour, as epoch millis.
fn utc(y: i32, mo: u32, d: u32, h: u32) -> Millis {
    Utc.with_ymd_and_hms(y, mo, d, h, 0, 0).unwrap().timestamp_millis()
}
