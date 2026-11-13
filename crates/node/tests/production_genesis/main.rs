//! The production genesis: it loads as a dev node's genesis, every market validates and carries the
//! launch decisions, and markets on one venue share one calendar. Sequenced as the node sequences
//! it, each market opens and closes on its venue's hours through weeks with holidays and early
//! closes. `divergence` checks the stress levels each market reaches, `futures` the contracts the
//! index and commodity markets roll through, and `spreads` their size spreads.

#[path = "../common/access.rs"]
pub mod access;
mod divergence;
mod futures;
mod spreads;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::{NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::params::{NotionalWeighting, ProtocolParams, StressLevel, StressTrigger, ToxicityParams};
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::{HORIZON_MS, launch_score_tiers};
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
const JPX: [MarketId; 1] = [25];
const US_SHARES: [MarketId; 20] = [40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59];

/// The launch catalogue: #7's majors without the Eurex and ICE markets (#673), and 20 US
/// large-cap shares, in listing order.
const SYMBOLS: [&str; 34] = [
    "BTC-USD", "ETH-USD", "SOL-USD", "EUR-USD", "GBP-USD", "USD-JPY", "AUD-USD", "US500", "US100", "US30", "JP225",
    "XAU-USD", "XAG-USD", "WTI", "AAPL", "MSFT", "NVDA", "AMZN", "GOOGL", "META", "TSLA", "AVGO", "BRK.B", "JPM", "V",
    "MA", "LLY", "JNJ", "XOM", "WMT", "COST", "NFLX", "AMD", "ORCL",
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
    let venues = [&CRYPTO[..], &CBOE_FX, &CME_EQUITY, &CME_METALS_ENERGY, &JPX, &US_SHARES].concat();
    assert_eq!(ids.len(), venues.len());
    assert!(venues.iter().all(|id| ids.contains(id)), "{ids:?}");
}

#[test]
fn the_production_genesis_applies_the_launch_decisions() {
    let production = production_genesis("launch-decisions");
    let dev = NodeConfig::load(Path::new(DEV)).unwrap().genesis;
    // A 10% profit fee (#9); the other parameters are dev's, except the total cap floor, whose
    // production value is not chosen yet, and the stress triggers, which #133 calibrates from
    // data for production (research/cap-calibration's stress stage; the values in
    // results/stress_choice.json): divergence shares whose stale-fill loss at fire covers each
    // level's spread on the tightest bound (share 6001 is the lowest the engine's truncated-share
    // arithmetic fires at 4 bps on the 5-bps markets), fast moves under a minute-scale noise
    // bound, and utilisation levels under a 2% flow-weighted fire rate in the Monte Carlo.
    // The toxicity parameters: from `V54` the score's tiers are #614's decision D1, scores of 4 and
    // 8 adding 2 and 5 bps, over the protocol's 5 s horizon; the notional weighting #130
    // calibrated applies before `V54`.
    assert_eq!(production.params.profit_fee_bps, 1_000);
    let floor = dev.params.house_total_cap_floor;
    let level = |size_spread_multiplier_bps, min_spread_bps, trigger| StressLevel {
        size_spread_multiplier_bps,
        min_spread_bps,
        trigger,
    };
    let trigger = |divergence_share_bps, move_bps, utilisation_bps| {
        Some(StressTrigger {
            divergence_share_bps: Some(divergence_share_bps),
            move_bps: Some(move_bps),
            utilisation_bps: Some(utilisation_bps),
            divergence_bps: None,
        })
    };
    assert_eq!(
        production.params.stress_levels,
        [
            level(10_000, 0, None),
            level(20_000, 2, trigger(6_001, 75, 7_000)),
            level(40_000, 10, trigger(9_500, 200, 9_500)),
        ]
    );
    let toxicity = dev.params.toxicity.clone();
    assert_eq!(
        production.params.toxicity,
        ToxicityParams {
            horizon_ms: HORIZON_MS,
            ewma_weight_bps: 1_000,
            notional: Some(NotionalWeighting {
                prior: Fixed::from_int(5_629_518),
                memory: Fixed::from_int(11_259_036)
            }),
            tiers: vec![],
            score_tiers: launch_score_tiers(),
        }
    );
    assert_eq!(
        ProtocolParams {
            house_total_cap_floor: floor,
            stress_levels: dev.params.stress_levels.clone(),
            toxicity,
            ..production.params.clone()
        },
        dev.params
    );
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
    }
}

#[test]
fn the_launch_markets_quoted_outside_usd_convert_at_their_pairs() {
    let production = production_genesis("quote-currencies");
    let quoted: BTreeMap<&str, QuoteCurrency> =
        production.markets.iter().map(|m| (m.symbol.as_str(), m.quote_currency)).collect();
    let jpy = QuoteCurrency::Jpy { fx_market: 12 };
    let expected = [("USD-JPY", jpy), ("JP225", jpy)];
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
