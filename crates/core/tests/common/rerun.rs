//! A latency guard's second chance (#1085) and its bar scaled to the runner's speed (#1119). On a
//! runner whose figures are all several times their usual size a guard breaks on unchanged code; a
//! real regression breaks it again, and breaks it past a bar scaled to that runner.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::OnceLock;
use std::time::Instant;

use engine_core::Engine;
use engine_core::command::{Command, Sequenced};
use engine_core::price::SourceQuote;
use engine_core::types::{AccountId, Side};

use super::{BTC, ETH, Harness, acct, f};

/// Set to `1` by CI's benchmark job: only there are bars scaled. Locally and on the production host,
/// #904's absolute bars apply as written.
pub const SCALE_ENV: &str = "ENGINE_BENCH_SCALE_TO_RUNNER";
/// What `reference_workload` took on a fast runner, in microseconds: the best of its `REPS` on an
/// ubuntu-24.04 hosted runner of the kind that ran `portfolio_price_execution_latency` in 312 ms
/// (#1119's run of 20 November 2026 measured it 55 to 68 ms across this job's steps). A runner twice
/// as slow takes twice this.
const REFERENCE_US: u128 = 56_000;
/// The scale's range in thousandths: never below 1 (a bar is never tighter than #904's), never above
/// 2.5, so a runner slower than that still fails.
const MIN_PERMILLE: u128 = 1_000;
const MAX_PERMILLE: u128 = 2_500;
/// The reference workload's book and commands: BTC-USD prices over a book every account holds, which
/// visit every holder, and daily rollovers, which finance and check every position.
const ACCOUNTS: usize = 20_000;
const PRICES: usize = 20;
const ROLLOVERS: usize = 3;
const REPS: usize = 3;

/// The scale of a runner that took `measured_us` on the reference workload against `reference_us` on
/// the fast runner, in thousandths, held to the range.
pub fn scale_permille(measured_us: u128, reference_us: u128) -> u128 {
    (measured_us * 1_000 / reference_us.max(1)).clamp(MIN_PERMILLE, MAX_PERMILLE)
}

/// `bar_us` at a scale in thousandths, rounded up.
pub fn scaled_with(bar_us: u128, permille: u128) -> u128 {
    (bar_us * permille).div_ceil(1_000)
}

/// This process's scale in thousandths: 1,000 unless `SCALE_ENV` is set, else the reference workload's
/// time against `REFERENCE_US`, measured once and printed.
pub fn runner_permille() -> u128 {
    static SCALE: OnceLock<u128> = OnceLock::new();
    *SCALE.get_or_init(|| {
        if std::env::var_os(SCALE_ENV).is_none() {
            return MIN_PERMILLE;
        }
        let measured = reference_workload();
        let permille = scale_permille(measured, REFERENCE_US);
        println!("LATENCY runner reference_us={measured} fast_runner_us={REFERENCE_US} scale={}", show(permille));
        permille
    })
}

/// `bar_us` scaled to this runner.
pub fn scaled(bar_us: u128) -> u128 {
    scaled_with(bar_us, runner_permille())
}

/// This process's scale as `1.234`.
pub fn scale() -> String {
    show(runner_permille())
}

/// A scale as `1.234`.
fn show(permille: u128) -> String {
    format!("{}.{:03}", permille / 1_000, permille % 1_000)
}

/// Prints `us` against `bar_us` scaled to this runner with the scale, and whether it is within.
pub fn verdict(name: &str, us: u128, bar_us: u128) -> bool {
    verdict_at(name, us, scaled(bar_us), bar_us)
}

/// As `verdict` for a bar the caller scaled, `bar_us` before the scale being what it names.
pub fn verdict_at(name: &str, us: u128, scaled_us: u128, bar_us: u128) -> bool {
    let within = us <= scaled_us;
    let word = if within { "within" } else { "OVER" };
    let scale = scale();
    println!("LATENCY bar {name} us={us} bar_us={bar_us} scale={scale} scaled_bar_us={scaled_us} {word}");
    within
}

/// Holds `us` to `bar_us` scaled to this runner, printing both.
pub fn hold(name: &str, us: u128, bar_us: u128) {
    let scaled_us = scaled(bar_us);
    assert!(verdict_at(name, us, scaled_us, bar_us), "{name} took {us}us against {scaled_us}us (bar {bar_us}us)");
}

/// Holds `us` to `bar_us` where the caller has scaled the part of it that a runner's speed moves.
pub fn hold_at(name: &str, us: u128, bar_us: u128) {
    assert!(verdict_at(name, us, bar_us, bar_us), "{name} took {us}us against {bar_us}us");
}

/// Microseconds the best of `REPS` runs of the benchmarks' own shape takes: commands over a book of
/// `ACCOUNTS` accounts that all hold one market, a price visiting each, then daily rollovers. Building
/// and loading the book are outside the timed region.
fn reference_workload() -> u128 {
    let mut h = Harness::v17();
    let account = acct(1);
    h.deposit(account, "1000000");
    h.price(BTC, "100");
    h.price(ETH, "100");
    h.market_order(account, BTC, Side::Buy, "1", "100");
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let template = state["accounts"].as_object().unwrap().values().next().unwrap().clone();
    let mut accounts = serde_json::Map::new();
    for index in 0..ACCOUNTS {
        let mut address = [0; 20];
        address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
        let key = serde_json::to_value(AccountId(address)).unwrap().as_str().unwrap().to_owned();
        accounts.insert(key, template.clone());
    }
    state["accounts"] = accounts.into();
    state["benchmarks"] = serde_json::json!({"SOFR": 430});
    state["markouts"] = serde_json::json!([]);
    state["next_order_id"] = serde_json::json!(ACCOUNTS + 1);
    state["markets"][BTC.to_string()]["client_qty"] = serde_json::json!(f(&ACCOUNTS.to_string()));
    state["markets"][BTC.to_string()]["client_cost"] = serde_json::json!(f(&(ACCOUNTS * 100).to_string()));
    state["markets"][ETH.to_string()]["client_qty"] = serde_json::json!(f("0"));
    state["markets"][ETH.to_string()]["client_cost"] = serde_json::json!(f("0"));
    let bytes = serde_json::to_vec(&state).expect("fixture encoding");
    drop((state, h));
    (0..REPS)
        .map(|_| {
            let mut engine: Engine = serde_json::from_slice(&bytes).expect("reference fixture");
            let start = engine.now();
            apply(&mut engine, start, Command::Tick);
            let began = Instant::now();
            for sample in 0..PRICES {
                let ts = start + sample as i64 + 1;
                let mid = f(if sample % 2 == 0 { "100.0001" } else { "100" });
                let quotes = (0..2)
                    .map(|venue| SourceQuote { source: format!("venue{venue}"), mid, ts, contract: None })
                    .collect();
                apply(&mut engine, ts, Command::Prices { market: BTC, quotes });
            }
            for day in 1..=ROLLOVERS {
                apply(&mut engine, start + day as i64 * 86_400_000, Command::Tick);
            }
            began.elapsed().as_micros()
        })
        .min()
        .expect("a rep")
}

fn apply(engine: &mut Engine, ts: i64, cmd: Command) {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    engine.apply(&Sequenced { seq, ts, cmd }).expect("valid reference sequence");
}

/// Runs `measure`, a benchmark's measurement and its bars' assertions. If it panics, prints that, runs
/// it once more and fails only if the second run panics too, with the second run's panic. Each run
/// prints its own figures, so both are in the log; the bars are the measurement's own, scaled by
/// `scaled` or `hold` where CI sets `SCALE_ENV`.
pub fn rerun_once<R>(name: &str, mut measure: impl FnMut() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(&mut measure)) {
        Ok(done) => done,
        Err(first) => {
            println!("LATENCY rerun {name}: the first run broke a bar ({}); measuring it once more", message(&*first));
            match catch_unwind(AssertUnwindSafe(&mut measure)) {
                Ok(done) => {
                    println!("LATENCY rerun {name}: the second run held every bar");
                    done
                }
                Err(second) => {
                    println!("LATENCY rerun {name}: the second run broke a bar too ({})", message(&*second));
                    resume_unwind(second)
                }
            }
        }
    }
}

/// What a panic said.
fn message(panic: &(dyn Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}
