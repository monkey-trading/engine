//! The sequencer-thread cost of an account read with its close-outs, for comparison with
//! `hot_path_benchmark`'s 200 µs p99 command budget, for an account holding every launch market.
//! Run it in a quiet CPU window:
//! `cargo test -p engine-node --lib stats::tests::closeout_cost -- --ignored --nocapture`.

use std::time::Instant;

use engine_core::market::QuoteCurrency;

use super::*;
use crate::stats::closeout::{ReachKind, account_closeouts};

const SAMPLES: usize = 2_000;

/// An engine whose `TRADER` holds one position in each market of the production genesis, 100000 of
/// notional at a mark of 100 (quoted at 1.08, 1.27 and 150 against the dollar for the other
/// currencies), long and short alternately, on the equity that leaves each 30000 from its close-out.
fn engine_with_every_launch_market() -> Engine {
    let production = include_str!("../../../../../config/production-genesis.toml");
    let genesis: toml::Table = toml::from_str(production).unwrap();
    let specs = genesis["genesis"]["markets"].as_array().unwrap();
    let mut state = serde_json::to_value(&Rig::new().engine).unwrap();
    let template = state["markets"]["1"].clone();
    let mut positions = serde_json::Map::new();
    for (index, listing) in specs.iter().enumerate() {
        let spec: engine_core::market::MarketSpec = listing.clone().try_into().unwrap();
        let id = spec.id;
        let mut market = template.clone();
        market["spec"] = serde_json::to_value(&spec).unwrap();
        market["mark"] = serde_json::json!("100");
        market["fx_mark"] = match spec.quote_currency {
            QuoteCurrency::Usd => serde_json::Value::Null,
            QuoteCurrency::Eur { .. } => serde_json::json!("1.08"),
            QuoteCurrency::Gbp { .. } => serde_json::json!("1.27"),
            QuoteCurrency::Jpy { .. } => serde_json::json!("150"),
        };
        state["markets"][id.to_string()] = market;
        let qty = if index % 2 == 0 { "1000" } else { "-1000" };
        positions.insert(id.to_string(), serde_json::json!({ "qty": qty, "entry": "100" }));
    }
    state["accounts"][TRADER.to_string()]["positions"] = positions.into();
    state["accounts"][TRADER.to_string()]["balance"] = serde_json::json!("0");
    let flat: Engine = serde_json::from_value(state.clone()).expect("the fixture's markets and positions");
    let (_, margin) = flat.equity_and_margin(&flat.accounts()[&TRADER], None);
    let level = flat.params().expect("dev genesis sets parameters").closeout_level_bps;
    let balance = margin.mul_bps(level) + Fixed::from_int(30_000);
    state["accounts"][TRADER.to_string()]["balance"] = serde_json::json!(balance);
    serde_json::from_value(state).expect("the fixture")
}

#[test]
#[ignore = "cost benchmark: run in an isolated CPU window"]
fn the_close_outs_of_an_account_holding_every_launch_market_fit_the_command_budget() {
    let engine = engine_with_every_launch_market();
    let acct = &engine.accounts()[&TRADER];
    let standing = engine.equity_and_margin(acct, None);
    let found = account_closeouts(&engine, acct, standing);
    let at = found.iter().filter(|c| c.reach == ReachKind::At).count();
    eprintln!("{} positions, {at} with a price", found.len());
    assert!(at * 2 > found.len(), "the fixture prices most positions: {found:?}");
    let mut closeouts = Vec::new();
    let mut whole = Vec::new();
    for _ in 0..SAMPLES {
        let start = Instant::now();
        let standing = engine.equity_and_margin(acct, None);
        let base = start.elapsed();
        let found = account_closeouts(&engine, acct, standing);
        let end = start.elapsed();
        std::hint::black_box((&found, acct.positions.clone(), acct.orders.clone()));
        closeouts.push((end - base).as_nanos());
        whole.push(start.elapsed().as_nanos());
    }
    for (name, times) in [("close-outs", &mut closeouts), ("account read", &mut whole)] {
        times.sort_unstable();
        let at = |per_cent: usize| times[(times.len() * per_cent / 100).min(times.len() - 1)] / 1_000;
        eprintln!("{name}: p50 {} us, p99 {} us", at(50), at(99));
    }
    assert!(whole[whole.len() * 99 / 100] < 200_000, "the read fits the 200 us p99 command budget");
}
