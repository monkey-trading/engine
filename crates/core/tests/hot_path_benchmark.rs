//! Optimized dev-profile command latency; fixture construction is outside timed samples.

mod common;

use std::time::Instant;

use chrono::NaiveDateTime;
use common::{BTC, ETH, Harness, acct, f, market, ny, us_equities};
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::price::SourceQuote;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AccountId, AssetClass, MAX_STAMP_MS, MarketId, Side};
use serde_json::json;

const SAMPLES: usize = 100;
const HALTED: MarketId = 9;

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
                            contract: None,
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

#[test]
#[ignore = "benchmark: run in an isolated CPU window"]
fn first_command_after_a_long_gap_latency() {
    // A market holding quotes across a year-long sequencer gap that ends in an open session: from
    // `V33` the held-quote closure check covers only the freshness window after the last command and
    // the gap's end, so the first command back does not walk every minute of the gap.
    let mut h = halted();
    let start = h.ts;
    apply(&mut h.engine, start, Command::Tick);
    let began = Instant::now();
    apply(&mut h.engine, ny(2027, 10, 19, 10, 29), Command::Tick);
    let elapsed = began.elapsed().as_micros();
    println!("LATENCY first_command_after_long_gap us={elapsed}");
    assert!(elapsed <= 5_000, "the first command after a year-long gap took {elapsed}us");
    assert_eq!(h.engine.markets()[&HALTED].quotes.len(), 2);
}

#[test]
#[ignore = "benchmark: run in an isolated CPU window"]
fn first_command_after_a_far_past_stamp_latency() {
    // A stamp 2.9 million days past the last rollover, the farthest the engine accepts: from `V33`
    // the command catches up on a bounded batch of rollovers and is deferred.
    let mut h = halted();
    let start = h.ts;
    let began = Instant::now();
    apply(&mut h.engine, MAX_STAMP_MS, Command::Tick);
    let elapsed = began.elapsed().as_micros();
    println!("LATENCY first_command_after_far_past_stamp us={elapsed}");
    assert!(elapsed <= 5_000, "the first command at the last accepted stamp took {elapsed}us");
    assert!(h.engine.next_rollover().is_some_and(|due| due < MAX_STAMP_MS && due > start));
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

/// `Harness::v33` with `HALTED` listed (US hours plus a special session open from 20 Oct 2026 to
/// the end of 2027, so a minute-by-minute walk over the gap finds no closure to stop at; quotes fresh
/// for an hour) and holding two quotes for it.
fn halted() -> Harness {
    let mut h = Harness::v33();
    let at = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").expect("a local time");
    let Schedule::Weekly { tz, windows, closed_dates, .. } = us_equities() else { unreachable!("weekly") };
    let special_sessions = vec![Period { from: at("2026-10-20 00:00"), to: at("2028-01-01 00:00") }];
    let schedule = Schedule::Weekly { tz, windows, closed_dates, closures: vec![], special_sessions };
    let spec = market(HALTED, "HALT", AssetClass::Equity, schedule, 2_000);
    let price_rules = PriceRules { max_age_ms: 3_600_000, ..spec.price_rules };
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..spec }));
    h.advance_to(ny(2026, 10, 20, 14, 30));
    h.price(HALTED, "100");
    h
}
