//! Shared harness for the `V59` weekend margin tests: a log that activates `V59` before its genesis,
//! as a fresh node's does, with an oil market on CME Globex hours carrying a weekend margin, and an
//! index quoted in yen with one.

use chrono::TimeZone;
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command, WithdrawalRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::market::{MarketSpec, QuoteCurrency, Tier};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AssetClass, MarketId, Millis};

use crate::common::close_spread::{attest, ceilings, evidence, fund, source_key};
use crate::common::spreads::tenths;
use crate::common::toxicity::v54_params;
use crate::common::{AAPL, BTC, CHAIN_ID, Client, ETH, Harness, USDJPY, VAULT, f, market, operator, us_equities};

/// The oil market, listed with WTI's id.
pub const OIL: MarketId = 32;
/// The gold market, a second market on the same hours.
pub const GOLD: MarketId = 30;
/// WTI's launch weekend margin: 7.5%.
pub const OIL_WEEKEND_BPS: i64 = 750;
/// An index quoted in yen, converted at USD-JPY's index, listed with Japan 225's id.
pub const NIKKEI: MarketId = 25;
/// Japan 225's launch weekend margin: 1.5%.
pub const NIKKEI_WEEKEND_BPS: i64 = 150;

/// CME Globex hours in Chicago: Sunday 17:00 to Friday 16:00, with a daily 16:00-17:00 break, and
/// `closures`.
pub fn globex(closures: Vec<Period>) -> Schedule {
    let windows: Vec<_> = [("Sun", "Mon"), ("Mon", "Tue"), ("Tue", "Wed"), ("Wed", "Thu"), ("Thu", "Fri")]
        .iter()
        .map(|(open, close)| serde_json::json!({ "open": format!("{open} 17:00"), "close": format!("{close} 16:00") }))
        .collect();
    let schedule = serde_json::json!({ "kind": "weekly", "tz": "America/Chicago", "windows": windows });
    let Schedule::Weekly { tz, windows, closed_dates, .. } = serde_json::from_value(schedule).unwrap() else {
        unreachable!("a weekly schedule")
    };
    Schedule::Weekly { tz, windows, closed_dates, closures, special_sessions: vec![] }
}

/// A commodity on `schedule` at 1% margin to 2m and 5% above, 2bp, 4bp and 8bp spread bands, a 1bp
/// markout floor, and `weekend_margin_bps`.
pub fn commodity(id: MarketId, symbol: &str, schedule: Schedule, weekend_margin_bps: i64) -> MarketSpec {
    MarketSpec {
        margin_tiers: vec![Tier { up_to: Some(f("2000000")), bps: 100 }, Tier { up_to: None, bps: 500 }],
        spread_tiers: tenths(&[(Some(2_000_000), 20), (Some(10_000_000), 40), (None, 80)]),
        markout_floor_bps: Some(f("1")),
        weekend_margin_bps,
        ..market(id, symbol, AssetClass::Commodity, schedule, 100)
    }
}

/// `genesis_at` `V59`.
pub fn genesis_v59(schedule: Schedule, oil_weekend_bps: i64, gold_weekend_bps: i64) -> Harness {
    genesis_at(ProtocolVersion::V59, schedule, oil_weekend_bps, gold_weekend_bps)
}

/// A log that activates `version` before its genesis: client auth, the operator, `version`, the harness
/// parameters in the form `V54` takes, a 4.3% SOFR, BTC, ETH and AAPL as `genesis_v52` lists them, oil
/// on `schedule` with `oil_weekend_bps`, gold on Globex hours with `gold_weekend_bps`, USD-JPY around
/// the clock and an index on Globex hours quoted in yen with `NIKKEI_WEEKEND_BPS`, 100m of house
/// capital, the ceiling registry and the access policy, at the harness start: Tuesday 20 October
/// 2026, 09:00 in Chicago.
pub fn genesis_at(
    version: ProtocolVersion,
    schedule: Schedule,
    oil_weekend_bps: i64,
    gold_weekend_bps: i64,
) -> Harness {
    let mut h = Harness::empty();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.admin(AdminCommand::SetParams(v54_params()));
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });
    for (id, symbol, class, schedule, margin) in [
        (BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000),
        (ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000),
        (AAPL, "AAPL", AssetClass::Equity, us_equities(), 2_000),
    ] {
        let spread_tiers = tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 50)]);
        let markout_floor_bps = Some(f("1"));
        let spec = MarketSpec { spread_tiers, markout_floor_bps, ..market(id, symbol, class, schedule, margin) };
        h.admin(AdminCommand::UpsertMarket(spec));
    }
    h.admin(AdminCommand::UpsertMarket(commodity(OIL, "WTI", schedule, oil_weekend_bps)));
    h.admin(AdminCommand::UpsertMarket(commodity(GOLD, "XAU-USD", globex(vec![]), gold_weekend_bps)));
    let yen = QuoteCurrency::Jpy { fx_market: USDJPY };
    let usdjpy = MarketSpec {
        quote_currency: yen,
        spread_tiers: tenths(&[(None, 10)]),
        markout_floor_bps: Some(f("1")),
        ..market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200)
    };
    h.admin(AdminCommand::UpsertMarket(usdjpy));
    let nikkei = commodity(NIKKEI, "JP225", globex(vec![]), NIKKEI_WEEKEND_BPS);
    h.admin(AdminCommand::UpsertMarket(MarketSpec { quote_currency: yen, asset_class: AssetClass::Index, ..nikkei }));
    h.cmd(Command::HouseCapital { amount: h.house_capital, chain_ref: "genesis".into() });
    let mut entries = ceilings();
    entries.extend([OIL, GOLD, USDJPY, NIKKEI].map(|market| CloseSpreadCeiling { market, max_close_spread_bps: 25 }));
    h.admin(AdminCommand::SetCloseSpreadCeilings { ceilings: entries });
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h
}

/// Client `seed`, declared and holding `amount`.
pub fn client(h: &mut Harness, seed: u8, amount: &str) -> Client {
    let client = Client::new(seed);
    attest(h, &client);
    fund(h, &client, amount);
    client
}

/// `client`'s signed, screened withdrawal of `amount`.
pub fn withdraw(h: &mut Harness, client: &Client, amount: &str) -> Vec<Event> {
    let (key, domain) = (client.key.clone(), client.domain.clone());
    let client = Client { key, account: client.account, domain, expires_at: h.ts + 60_000 };
    let Command::Withdraw { account, amount, auth } =
        client.withdraw(h.engine.next_request_nonce(client.account), amount)
    else {
        unreachable!("a withdrawal")
    };
    let request = WithdrawalRequest { account, amount, auth };
    let signed = request.auth.as_ref().expect("signed");
    let hash = eip712::withdrawal_hash(&client.domain, account, amount, signed.nonce, signed.expires_at);
    let ev = evidence(h, hash, Some(account), None);
    h.cmd(Command::AccessWithdraw { request, evidence: ev, payout_available: true })
}

/// A dated period in Chicago local time, `"YYYY-MM-DD HH:MM"`.
pub fn period(from: &str, to: &str) -> Period {
    let at = |s: &str| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
    Period { from: at(from), to: at(to) }
}

/// A Chicago wall-clock time as epoch millis.
pub fn chicago(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::America::Chicago.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}
