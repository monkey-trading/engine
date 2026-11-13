//! `V52` agreement vectors for the research model's closing prices, through the access-screened
//! order path. From `V52` a listed market's bands move only after a listing notice and its stress
//! parameters only after a parameter notice, so each vector's parameters, listing and ceiling are set
//! before `V52` activates; activation writes the bands in tenths of a bp.

use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::market::QuoteCurrency;
use engine_core::params::{ProtocolParams, StressLevel, ToxicityTier};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId, Side};
use serde::Deserialize;

use crate::common::close_spread::{attest, fund, order, source_key};
use crate::common::events::{only_fill, rejection};
use crate::common::session_keys::HOUR_MS;
use crate::common::{CHAIN_ID, Client, EURUSD, Harness, USDJPY, VAULT, f, market, params};

const WIDE: MarketId = 90;
/// The ceiling every other listed market takes: the most a ceiling may be, so it never binds.
const UNBOUNDED_BPS: i64 = 9_999;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    name: String,
    mid: Fixed,
    opening_mid: Fixed,
    current: Fixed,
    quantity: Fixed,
    buy: bool,
    currency: String,
    fx: Option<Fixed>,
    asset_class: AssetClass,
    minimum_bps: i64,
    toxicity_bps: i64,
    multiplier_bps: i64,
    /// `(up_to, rate)` with the rate in tenths of a bp.
    tiers_tenth_bps: Vec<(Option<Fixed>, i64)>,
    /// Whether the market is listed in whole bps (`rate / 10`) before `V52` converts it.
    listed_in_bps: bool,
    ceiling_bps: i64,
    reduce_only: bool,
    liquidation: bool,
    deposit: Fixed,
    #[serde(default)]
    expected_price_raw: Option<i128>,
    #[serde(default)]
    expected_quantity_raw: Option<i128>,
    #[serde(default)]
    expected_spread_bps: Option<i64>,
    #[serde(default)]
    expected_bad_debt_raw: i128,
    /// `close_spread_ceiling` when the engine refuses the order as a reversal over the ceiling.
    #[serde(default)]
    expected_refusal: Option<String>,
}

#[test]
fn research_model_prices_agree_with_v52_fills() {
    let json = include_str!("../../../../research/close-spread-calibration/fixtures/engine-v52.json");
    let vectors: Vec<Vector> = serde_json::from_str(json).expect("valid pinned research vectors");
    assert_eq!(vectors.len(), 26, "all shared research scenarios must be present");
    for vector in vectors {
        check(vector);
    }
}

/// The vector's spread bands as listed before `V52`, and as `V52` reads them.
fn bands(vector: &Vector) -> (Vec<SpreadTier>, Vec<SpreadTier>) {
    let tenths: Vec<_> = vector.tiers_tenth_bps.iter().map(|&(up_to, rate)| SpreadTier::tenths(up_to, rate)).collect();
    if !vector.listed_in_bps {
        return (tenths.clone(), tenths);
    }
    let listed = vector
        .tiers_tenth_bps
        .iter()
        .map(|&(up_to, rate)| {
            assert_eq!(rate % 10, 0, "{}: a band listed in bps is whole", vector.name);
            SpreadTier::bps(up_to, rate / 10)
        })
        .collect();
    (listed, tenths)
}

/// `V8` with the vector's parameters, the FX markets, the vector's market and the ceiling registry,
/// then access rules and `V52`.
fn harness(vector: &Vector) -> Harness {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    let level = StressLevel {
        size_spread_multiplier_bps: vector.multiplier_bps,
        min_spread_bps: vector.minimum_bps,
        trigger: None,
    };
    let mut p = ProtocolParams { stress_levels: vec![level], ..params() };
    p.toxicity.tiers = vec![ToxicityTier { min_score_bps: 0, extra_spread_bps: vector.toxicity_bps }];
    h.operator_admin(AdminCommand::SetParams(p));
    h.list_fx();
    let mut spec = market(WIDE, "RESEARCH", vector.asset_class, Schedule::AlwaysOpen, 1_000);
    spec.max_slippage_bps = 10_000;
    spec.quote_currency = match vector.currency.as_str() {
        "USD" => QuoteCurrency::Usd,
        "EUR" => QuoteCurrency::Eur { fx_market: EURUSD },
        "JPY" => QuoteCurrency::Jpy { fx_market: USDJPY },
        _ => panic!("unsupported vector currency"),
    };
    let (listed, read) = bands(vector);
    spec.spread_tiers = listed;
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let ceilings = h
        .engine
        .markets()
        .keys()
        .map(|&market| CloseSpreadCeiling {
            market,
            max_close_spread_bps: if market == WIDE { vector.ceiling_bps } else { UNBOUNDED_BPS },
        })
        .collect();
    let events = h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings }));
    assert!(events.contains(&Event::AdminApplied), "{}: ceilings refused: {events:?}", vector.name);
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V52 });
    assert_eq!(h.engine.markets()[&WIDE].spec.spread_tiers, read, "{}: bands at V52", vector.name);
    h
}

fn check(vector: Vector) {
    let name = vector.name.as_str();
    let mut h = harness(&vector);
    let client = Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client, &vector.deposit.to_string());
    if let Some(fx) = vector.fx {
        let fx_market = if vector.currency == "JPY" { USDJPY } else { EURUSD };
        h.price(fx_market, &fx.to_string());
    }
    h.price(WIDE, &vector.opening_mid.to_string());
    if !vector.current.is_zero() {
        let side = if vector.current.is_positive() { Side::Buy } else { Side::Sell };
        let (qty, quoted) = (vector.current.abs().to_string(), vector.opening_mid.to_string());
        let events = order(&mut h, &client, WIDE, side, &qty, &quoted, false);
        assert_eq!(only_fill(&events).position, vector.current, "{name} opening");
    }
    let prices = h.price(WIDE, &vector.mid.to_string());
    let events = if vector.liquidation {
        prices
    } else {
        assert!(!prices.iter().any(|event| matches!(event, Event::Fill(_))), "{name}: {prices:?}");
        let side = if vector.buy { Side::Buy } else { Side::Sell };
        let (qty, quoted) = (vector.quantity.to_string(), vector.mid.to_string());
        order(&mut h, &client, WIDE, side, &qty, &quoted, vector.reduce_only)
    };
    if let Some(refusal) = &vector.expected_refusal {
        assert_eq!(refusal, "close_spread_ceiling", "{name}");
        assert_eq!(rejection(&events), &Reject::CloseSpreadCeiling { ceiling_bps: vector.ceiling_bps }, "{name}");
        assert_eq!(h.engine.accounts()[&client.account].position(WIDE).qty, vector.current, "{name} position");
    } else {
        let fill = only_fill(&events);
        assert_eq!(fill.liquidation, vector.liquidation, "{name} liquidation");
        assert_eq!(Some(fill.price.raw()), vector.expected_price_raw, "{name} price");
        assert_eq!(Some(fill.qty.raw()), vector.expected_quantity_raw, "{name} quantity");
        assert_eq!(Some(fill.spread_bps), vector.expected_spread_bps, "{name} spread");
        assert_eq!(fill.index, vector.mid, "{name} index");
    }
    let bad_debt: Fixed = events
        .iter()
        .filter_map(|event| match event {
            Event::BadDebt { account, amount } if *account == client.account => Some(*amount),
            _ => None,
        })
        .sum();
    assert_eq!(bad_debt.raw(), vector.expected_bad_debt_raw, "{name} bad debt");
    h.assert_conserved(f("0"));
}
