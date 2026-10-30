//! Engine::apply agreement vectors for the research-only closing-price model, at `V17` and at `V21`,
//! the latest standalone harness without access screening. `Engine::price` has no version gate other
//! than the `V10` rounding. This checks the engine half; `tests/test_execution.py` checks the model.

mod common;

use common::events::only_fill;
use common::{EURUSD, Harness, USDJPY, acct, f, market, params};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::event::Event;
use engine_core::market::{QuoteCurrency, Tier};
use engine_core::params::{ProtocolParams, StressLevel, ToxicityTier};
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Side};
use serde::Deserialize;

const WIDE: u32 = 90;

#[derive(Debug, Deserialize)]
struct Vector {
    name: String,
    mid: Fixed,
    opening_mid: Fixed,
    current: Fixed,
    quantity: Fixed,
    buy: bool,
    currency: String,
    fx: Option<Fixed>,
    minimum_bps: i64,
    toxicity_bps: i64,
    multiplier_bps: i64,
    tiers: Vec<(Option<Fixed>, i64)>,
    reduce_only: bool,
    liquidation: bool,
    deposit: Fixed,
    expected_price_raw: i128,
    expected_quantity_raw: i128,
    expected_spread_bps: i64,
    #[serde(default)]
    expected_bad_debt_raw: i128,
}

#[test]
fn research_model_prices_agree_with_fills() {
    let json = include_str!("../../../research/close-spread-calibration/fixtures/engine-v17.json");
    for (version, harness) in [("V17", Harness::v17 as fn() -> Harness), ("V21", Harness::v21)] {
        let vectors: Vec<Vector> = serde_json::from_str(json).expect("valid pinned research vectors");
        assert_eq!(vectors.len(), 13, "all shared research scenarios must be present");
        for vector in vectors {
            check(version, harness(), vector);
        }
    }
}

fn check(version: &str, mut h: Harness, vector: Vector) {
    let mut p = ProtocolParams {
        stress_levels: vec![StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None }],
        ..params()
    };
    p.toxicity.tiers.clear();
    h.operator_admin(AdminCommand::SetParams(p.clone()));
    h.list_fx();
    let mut spec = market(WIDE, "RESEARCH", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    spec.max_slippage_bps = 10_000;
    spec.quote_currency = match vector.currency.as_str() {
        "USD" => QuoteCurrency::Usd,
        "EUR" => QuoteCurrency::Eur { fx_market: EURUSD },
        "JPY" => QuoteCurrency::Jpy { fx_market: USDJPY },
        _ => panic!("unsupported vector currency"),
    };
    spec.spread_tiers = vec![Tier { up_to: None, bps: 0 }];
    h.operator_admin(AdminCommand::UpsertMarket(spec.clone()));
    if let Some(fx) = vector.fx {
        let fx_market = if vector.currency == "JPY" { USDJPY } else { EURUSD };
        h.price(fx_market, &fx.to_string());
    }
    h.price(WIDE, &vector.opening_mid.to_string());
    let a = acct(1);
    h.deposit(a, &vector.deposit.to_string());
    if !vector.current.is_zero() {
        let side = if vector.current.is_positive() { Side::Buy } else { Side::Sell };
        let events = h.market_order(a, WIDE, side, &vector.current.abs().to_string(), &vector.opening_mid.to_string());
        assert_eq!(only_fill(&events).position, vector.current, "{version} {} opening", vector.name);
    }
    spec.spread_tiers = vector.tiers.iter().map(|&(up_to, bps)| Tier { up_to, bps }).collect();
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    p.stress_levels[0].min_spread_bps = vector.minimum_bps;
    p.stress_levels[0].size_spread_multiplier_bps = vector.multiplier_bps;
    p.toxicity.tiers = vec![ToxicityTier { min_score_bps: 0, extra_spread_bps: vector.toxicity_bps }];
    h.operator_admin(AdminCommand::SetParams(p));
    let prices = h.price(WIDE, &vector.mid.to_string());
    let events = if vector.liquidation {
        prices
    } else {
        assert!(!prices.iter().any(|event| matches!(event, Event::Fill(_))));
        h.cmd(Command::PlaceOrder(OrderRequest {
            account: a,
            client_id: 1,
            market: WIDE,
            side: if vector.buy { Side::Buy } else { Side::Sell },
            qty: vector.quantity,
            kind: OrderKind::Market { quoted: vector.mid, max_slippage_bps: None },
            reduce_only: vector.reduce_only,
            auth: None,
        }))
    };
    let fill = only_fill(&events);
    assert_eq!(fill.liquidation, vector.liquidation, "{version} {} liquidation", vector.name);
    assert_eq!(fill.price.raw(), vector.expected_price_raw, "{version} {} price", vector.name);
    assert_eq!(fill.qty.raw(), vector.expected_quantity_raw, "{version} {} quantity", vector.name);
    assert_eq!(fill.spread_bps, vector.expected_spread_bps, "{version} {} spread", vector.name);
    assert_eq!(fill.index, vector.mid, "{version} {} index", vector.name);
    let bad_debt: Fixed = events
        .iter()
        .filter_map(|event| match event {
            Event::BadDebt { account, amount } if *account == a => Some(*amount),
            _ => None,
        })
        .sum();
    assert_eq!(bad_debt.raw(), vector.expected_bad_debt_raw, "{version} {} bad debt", vector.name);
    h.assert_conserved(f("0"));
}
