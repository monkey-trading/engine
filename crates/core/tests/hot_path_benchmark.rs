//! Optimized dev-profile command latency; fixture construction is outside timed samples.

mod common;

use std::time::Instant;

use common::{BTC, ETH, Harness, acct, f};
use engine_core::Engine;
use engine_core::command::{Command, OrderKind, Sequenced};
use engine_core::price::SourceQuote;
use engine_core::types::{AccountId, Side};
use serde_json::json;

const SAMPLES: usize = 100;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn command_latency() {
    for (name, count, concentrated, resting, rollover) in [
        ("dispersed_1k", 1_000, false, false, false),
        ("dispersed_100k", 100_000, false, false, false),
        ("concentrated_100k", 100_000, true, false, false),
        ("resting_100k", 100_000, false, true, false),
        ("rollover_100k", 100_000, true, false, true),
    ] {
        let mut engine = fixture(count, concentrated, resting);
        let start = engine.now();
        // Initialize transient indexes before taking the steady-state samples.
        apply(&mut engine, start, Command::Tick);
        let balance_before = engine.accounts().values().next().unwrap().balance;
        let mut times = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            let ts = start + if rollover { (sample as i64 + 1) * 86_400_000 } else { sample as i64 + 1 };
            let command = if rollover {
                Command::Tick
            } else {
                Command::Prices {
                    market: BTC,
                    quotes: (0..2)
                        .map(|venue| SourceQuote {
                            source: format!("venue{venue}"),
                            mid: f(if sample % 2 == 0 { "100.0001" } else { "100" }),
                            ts,
                        })
                        .collect(),
                }
            };
            let began = Instant::now();
            apply(&mut engine, ts, command);
            times.push(began.elapsed().as_micros());
        }
        times.sort_unstable();
        println!("LATENCY {name} accounts={count} samples={SAMPLES} p50_us={} p99_us={}", times[49], times[98]);
        let budget_us = match name {
            "concentrated_100k" => 250_000,
            "rollover_100k" => 500_000,
            _ => 200,
        };
        assert!(times[98] <= budget_us, "{name} p99 {}us exceeds {budget_us}us", times[98]);
        if rollover {
            assert!(
                engine.accounts().values().next().unwrap().balance < balance_before,
                "rollover fixture must charge financing"
            );
        }
        assert_eq!(engine.accounts().len(), count);
    }
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn period_finalisation_latency() {
    // Every account lost in the open emission period, so the tick at its end allocates to each.
    let count = 100_000;
    let mut h = Harness::emitting(1_000_000_000_000_000_000_000);
    h.deposit(acct(1), "1000");
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let template = state["accounts"].as_object().unwrap().values().next().unwrap().clone();
    let (mut accounts, mut results) = (serde_json::Map::new(), serde_json::Map::new());
    for index in 0..count {
        let mut address = [0; 20];
        address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let key = serde_json::to_value(AccountId(address)).unwrap().as_str().unwrap().to_owned();
        accounts.insert(key.clone(), template.clone());
        results.insert(key, json!({ "realised": f(&format!("-{}", index % 997 + 1)), "written_off": "0" }));
    }
    state["accounts"] = accounts.into();
    state["period_results"] = results.into();
    let mut engine: Engine = serde_json::from_value(state).expect("benchmark fixture");
    let end = engine.emissions().unwrap().schedule.period_end(0);
    // Rebuild the transient indexes inside period 0, outside the timed finalisation.
    apply(&mut engine, end - 1, Command::Tick);
    let began = Instant::now();
    apply(&mut engine, end, Command::Tick);
    let elapsed = began.elapsed().as_micros();
    println!("LATENCY period_finalisation accounts={count} us={elapsed}");
    assert!(elapsed <= 500_000, "finalising {count} accounts took {elapsed}us");
    assert!(engine.period_results().is_empty());
    let emissions = engine.emissions().unwrap();
    assert_eq!(emissions.entitled.0 + emissions.cancelled.0, emissions.schedule.budget(0));
    assert!(emissions.entitled.0 > 0);
}

fn apply(engine: &mut Engine, ts: i64, cmd: Command) {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    engine.apply(&Sequenced { seq, ts, cmd }).expect("valid benchmark sequence");
}

fn fixture(count: usize, concentrated: bool, resting: bool) -> Engine {
    let mut h = Harness::v17();
    let account = acct(1);
    h.deposit(account, "1000000");
    h.price(BTC, "100");
    h.price(ETH, "100");
    if resting {
        h.order(account, BTC, Side::Buy, "1", OrderKind::Stop { price: f("110") });
    } else {
        h.market_order(account, BTC, Side::Buy, "1", "100");
    }
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let template = state["accounts"].as_object().unwrap().values().next().unwrap().clone();
    let order = template["orders"].as_object().unwrap().values().next().cloned();
    let mut accounts = serde_json::Map::new();
    let mut book = serde_json::Map::new();
    for index in 0..count {
        let mut address = [0; 20];
        address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let account = AccountId(address);
        let key = serde_json::to_value(account).unwrap().as_str().unwrap().to_owned();
        let mut value = template.clone();
        if resting {
            let id = (index + 1).to_string();
            let mut order = order.clone().expect("resting fixture");
            order["order_id"] = json!(index + 1);
            value["orders"] = json!({id.clone(): order});
            book.insert(id, json!(account));
        } else if !concentrated && index != 0 {
            let position = value["positions"].as_object_mut().unwrap().remove(&BTC.to_string()).unwrap();
            value["positions"] = json!({ETH.to_string(): position});
        }
        accounts.insert(key, value);
    }
    state["accounts"] = accounts.into();
    state["benchmarks"] = json!({"SOFR": 430});
    state["markouts"] = json!([]);
    state["next_order_id"] = json!(count + 1);
    for market in [BTC, ETH] {
        let held = if resting {
            0
        } else if concentrated {
            if market == BTC { count } else { 0 }
        } else if market == BTC {
            1
        } else {
            count - 1
        };
        state["markets"][market.to_string()]["client_qty"] = json!(f(&held.to_string()));
        state["markets"][market.to_string()]["client_cost"] = json!(f(&(held * 100).to_string()));
    }
    if resting {
        state["markets"][BTC.to_string()]["resting"] = book.into();
    }
    serde_json::from_value(state).expect("benchmark fixture")
}
