//! The sequencer-thread cost of the stats record, for comparison with `hot_path_benchmark`'s 200 µs
//! p99 command budget: what each applied command adds (`observe`, which scans no account), and what a
//! scan of every account adds once per `api.stats_interval`, at 100k accounts each holding a
//! position. Run it in a quiet CPU window:
//! `cargo test -p engine-node --lib stats::bench -- --ignored --nocapture`.

use std::time::Instant;

use engine_core::types::{AccountId, Side};
use engine_core::{Engine, Fixed};

use crate::stats::snapshot::{Grid, Layout, scan};
use crate::stats::tests::{BTC, Rig, T0, TRADER};

const ACCOUNTS: usize = 100_000;
const SAMPLES: usize = 30;

/// The rig's engine with `ACCOUNTS` accounts, each a copy of the long trader's, half of them short.
fn engine_with_accounts() -> (Rig, Engine) {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    let mut state = serde_json::to_value(&rig.engine).expect("fixture encoding");
    let template = state["accounts"][TRADER.to_string()].clone();
    let mut accounts = serde_json::Map::new();
    for index in 0..ACCOUNTS {
        let mut address = [0; 20];
        address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let key = serde_json::to_value(AccountId(address)).unwrap().as_str().unwrap().to_owned();
        let mut account = template.clone();
        if index % 2 == 1 {
            let position = account["positions"][BTC.to_string()].clone();
            let short = Fixed::from_int(-1);
            account["positions"][BTC.to_string()] = serde_json::json!({ "qty": short, "entry": position["entry"] });
        }
        accounts.insert(key, account);
    }
    state["accounts"] = accounts.into();
    let engine: Engine = serde_json::from_value(state).expect("benchmark fixture");
    (rig, engine)
}

fn percentile(times: &mut [u128], per_cent: usize) -> u128 {
    times.sort_unstable();
    times[(times.len() * per_cent / 100).min(times.len() - 1)]
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn scan_and_observe_cost() {
    let (rig, engine) = engine_with_accounts();
    assert_eq!(engine.accounts().len(), ACCOUNTS);
    let layout = Layout { closeout: Grid::new(2_000, 100), profit: Grid::new(5_000, 500) };
    let mut scans = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let began = Instant::now();
        let snapshot = scan(&engine, 0, T0, layout);
        scans.push(began.elapsed().as_micros());
        assert!(snapshot.open_positions >= ACCOUNTS as u64);
    }
    println!(
        "STATS scan accounts={ACCOUNTS} samples={SAMPLES} p50_us={} p99_us={}",
        percentile(&mut scans.clone(), 50),
        percentile(&mut scans, 99)
    );
    // What every command pays: the record's `observe` with no scan due.
    let mut observed = Vec::with_capacity(1000);
    for step in 0..1000 {
        let began = Instant::now();
        rig.stats.observe(&engine, T0 + 2_000 + step, &[]);
        observed.push(began.elapsed().as_micros());
    }
    println!(
        "STATS observe accounts={ACCOUNTS} samples=1000 p50_us={} p99_us={}",
        percentile(&mut observed.clone(), 50),
        percentile(&mut observed, 99)
    );
}
