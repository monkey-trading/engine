//! The live write's cost per sample. Not run in CI:
//! `cargo test -p engine-node --lib marks::store::tests::bench -- --ignored --nocapture`.

use std::time::Instant;

use super::*;

const MARKETS: u32 = 8;
const BATCHES: usize = 40;
const BATCH: usize = crate::history::marks::run::BATCH;
const ROUNDS: usize = 3;

/// A batch of `BATCH` samples spread over `MARKETS` markets, 250 ms apart, so a fresh 1s bucket
/// opens every fourth sample per market.
fn batch(index: usize) -> Vec<(u32, i64, Fixed)> {
    let base = 1_792_489_860_000 + (index * BATCH) as i64 * 250;
    (0..BATCH)
        .map(|n| {
            let mark = fixed(&format!("{}", 100 + (index * BATCH + n) % 17));
            (n as u32 % MARKETS, base + n as i64 * 250, mark)
        })
        .collect()
}

#[test]
#[ignore = "a timing comparison, run by hand"]
fn observe_cost_per_sample() {
    let mut best = f64::MAX;
    for round in 0..ROUNDS {
        let mut store = fresh_store(&format!("marks-bench-{round}"), Retentions::defaults());
        let batches: Vec<_> = (0..BATCHES).map(batch).collect();
        let started = Instant::now();
        for samples in &batches {
            store.observe(samples).unwrap();
        }
        let micros = started.elapsed().as_secs_f64() * 1e6 / (BATCHES * BATCH) as f64;
        println!("round {round}: {micros:.1} us per sample");
        best = best.min(micros);
    }
    println!("observe: best of {ROUNDS}, {} samples per round: {best:.1} us per sample", BATCHES * BATCH);
}
