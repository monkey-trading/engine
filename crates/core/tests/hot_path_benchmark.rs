//! Optimized dev-profile command latency; fixture construction is outside timed samples.

mod common;

use std::time::Instant;

use chrono::NaiveDateTime;
use common::batch::{cancel_all, close_all, funded, market_order};
use common::close_spread::rest;
use common::session_keys::fresh_key;
use common::{BTC, Client, ETH, Harness, acct, f, market, ny, us_equities};
use engine_core::Engine;
use engine_core::command::{AdminCommand, CloseEntry, Command, OrderKind, Sequenced};
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::params::DEFAULT_MAX_RESTING_ORDERS;
use engine_core::price::SourceQuote;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AccountId, AssetClass, MAX_STAMP_MS, MarketId, Side};
use serde_json::json;

const SAMPLES: usize = 100;
const HALTED: MarketId = 9;
/// The markets the production genesis lists, which a close-all across every market names.
const LAUNCH_MARKETS: usize = 37;
/// The p99 budget for a signed batch command at 100k accounts. The 200 us of the unsigned commands
/// above cannot hold for any signed one, whose signature recovery alone takes about that long
/// (`empty_cancel_all_100k`); a batch must add little on top, and a close-all across every market
/// measured about 270 us above that floor.
const BATCH_BUDGET_US: u128 = 2_000;

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

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn batch_command_latency() {
    // Each sample is its own account, holding a position in every launch market and a full set of
    // resting orders, among 100k accounts. A signed, screened cancel-all or close-all is built
    // outside the timed region, so the timing is the engine's apply of it, signature check included.
    let (mut h, client, _) = funded();
    h.price(ETH, "3000");
    market_order(&mut h, &client, BTC, Side::Buy, "0.5", "100000");
    market_order(&mut h, &client, ETH, Side::Sell, "5", "3000");
    rest(&mut h, &client, BTC, Side::Buy, "0.1", OrderKind::Limit { price: f("90000") });
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let template =
        state["accounts"].as_object().unwrap().iter().find(|(_, v)| !v["orders"].as_object().unwrap().is_empty());
    let (_, template) = template.map(|(k, v)| (k.clone(), v.clone())).expect("the client rests an order");
    let order = template["orders"].as_object().unwrap().values().next().cloned().unwrap();
    let position = template["positions"][ETH.to_string()].clone();
    let market_template = state["markets"][ETH.to_string()].clone();
    // The launch markets: the harness's three, and copies of ETH under further ids.
    let ids: Vec<MarketId> =
        [BTC, ETH, common::AAPL].into_iter().chain(100..100 + (LAUNCH_MARKETS - 3) as MarketId).collect();
    for &id in ids.iter().filter(|id| ![BTC, ETH, common::AAPL].contains(id)) {
        let mut copy = market_template.clone();
        copy["spec"]["id"] = json!(id);
        copy["spec"]["symbol"] = json!(format!("COPY-{id}"));
        state["markets"][id.to_string()] = copy;
    }
    let samples: Vec<Client> = (0..SAMPLES as u8).map(fresh_key).collect();
    let mut next_order = state["next_order_id"].as_u64().unwrap();
    let mut accounts = state["accounts"].take().as_object().unwrap().clone();
    for sample in &samples {
        let mut value = template.clone();
        // BTC long and ETH short as the harness holds them, and ETH's position in every copy;
        // AAPL is unpriced, so its close-all entry is a rejection.
        value["positions"] = json!({});
        for id in ids.iter().filter(|id| **id != common::AAPL) {
            let held = if *id == BTC { template["positions"][BTC.to_string()].clone() } else { position.clone() };
            value["positions"][id.to_string()] = held;
        }
        value["orders"] = json!({});
        for index in 0..DEFAULT_MAX_RESTING_ORDERS {
            let market = ids[index % ids.len()];
            let mut resting = order.clone();
            resting["order_id"] = json!(next_order);
            resting["market"] = json!(market);
            value["orders"][next_order.to_string()] = resting;
            state["markets"][market.to_string()]["resting"][next_order.to_string()] = json!(sample.account);
            next_order += 1;
        }
        accounts.insert(serde_json::to_value(sample.account).unwrap().as_str().unwrap().to_owned(), value);
    }
    // Filler accounts up to 100k, and each market's aggregates matching the positions held.
    for index in 0..100_000usize.saturating_sub(accounts.len()) {
        let mut address = [0xee; 20];
        address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let key = serde_json::to_value(AccountId(address)).unwrap().as_str().unwrap().to_owned();
        let mut filler = template.clone();
        filler["positions"] = json!({});
        filler["orders"] = json!({});
        accounts.insert(key, filler);
    }
    // The aggregates of each market match the positions held: the harness client's and every sample's.
    let (long, short) = (f("0.5"), f("-5"));
    for id in &ids {
        let holders = SAMPLES as i128 + i128::from(*id == BTC || *id == ETH);
        let (qty, entry) = match *id {
            BTC => (long.raw() * holders, f("100000")),
            common::AAPL => (0, f("0")),
            _ => (short.raw() * holders, f("3000")),
        };
        let market = &mut state["markets"][id.to_string()];
        market["client_qty"] = json!(engine_core::Fixed::from_raw(qty));
        market["client_cost"] = json!(engine_core::Fixed::from_raw(qty * entry.raw() / 1_000_000_000));
    }
    // Each sample has made the declaration the harness client has.
    let declared = state["eligibility"][client.account.to_string()].clone();
    for sample in &samples {
        state["eligibility"][sample.account.to_string()] = declared.clone();
    }
    state["accounts"] = accounts.into();
    state["next_order_id"] = json!(next_order);
    state["markouts"] = json!([]);
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    apply(&mut h.engine, h.ts, Command::Tick);

    let mut cancel_times = Vec::with_capacity(SAMPLES);
    let mut floor_times = Vec::with_capacity(SAMPLES);
    let mut close_times = Vec::with_capacity(SAMPLES);
    for (index, sample) in samples.iter().enumerate() {
        let ts = h.ts + 1 + index as i64;
        let cancel = cancel_all(sample, sample.account, 0);
        let began = Instant::now();
        let events = apply_events(&mut h.engine, ts, cancel);
        cancel_times.push(began.elapsed().as_micros());
        assert_eq!(events.len(), DEFAULT_MAX_RESTING_ORDERS, "every resting order is cancelled");
        assert!(h.engine.accounts()[&sample.account].orders.is_empty());

        // A cancel-all with nothing resting: the signature check and the per-command work every
        // command pays, which the close-all's fills add to.
        let empty = cancel_all(sample, sample.account, 1);
        let began = Instant::now();
        apply_events(&mut h.engine, ts, empty);
        floor_times.push(began.elapsed().as_micros());

        let entries: Vec<CloseEntry> = ids
            .iter()
            .map(|&market| CloseEntry { market, quoted: if market == BTC { f("99000") } else { f("3100") } })
            .collect();
        let close = close_all(&h, sample, sample.account, 2, entries, None);
        let began = Instant::now();
        let events = apply_events(&mut h.engine, ts, close);
        close_times.push(began.elapsed().as_micros());
        let filled = events.iter().filter(|e| matches!(e, engine_core::event::Event::Fill(_))).count();
        let rejected = events.iter().filter(|e| matches!(e, engine_core::event::Event::OrderRejected { .. })).count();
        // Every market but the unpriced AAPL closes.
        assert_eq!((filled, rejected), (LAUNCH_MARKETS - 1, 1), "the close-all fills each priced market: {events:?}");
    }
    for (name, times) in [
        ("empty_cancel_all_100k", &mut floor_times),
        ("cancel_all_at_cap_100k", &mut cancel_times),
        ("close_all_all_markets_100k", &mut close_times),
    ] {
        times.sort_unstable();
        println!(
            "LATENCY {name} accounts=100000 samples={SAMPLES} p50_us={} p99_us={}",
            times[SAMPLES / 2 - 1],
            times[SAMPLES - 2]
        );
        assert!(
            times[SAMPLES - 2] <= BATCH_BUDGET_US,
            "{name} p99 {}us exceeds {BATCH_BUDGET_US}us",
            times[SAMPLES - 2]
        );
    }
}

fn apply_events(engine: &mut Engine, ts: i64, cmd: Command) -> Vec<engine_core::event::Event> {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    engine.apply(&Sequenced { seq, ts, cmd }).expect("valid benchmark sequence")
}
