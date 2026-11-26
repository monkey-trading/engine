//! The live write's cost per sample, and a frozen copy's cost per row to write, read back for its root and import.
//! Not run in CI: `cargo test -p engine-node --lib marks::store::tests::bench -- --ignored --nocapture`.

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

/// The best of `ROUNDS` runs of `BATCHES` batches against fresh stores holding `retentions`.
fn measure(label: &str, retentions: Retentions) {
    let mut best = f64::MAX;
    for round in 0..ROUNDS {
        let mut store = fresh_store(&format!("marks-bench-{label}-{round}"), retentions);
        let batches: Vec<_> = (0..BATCHES).map(batch).collect();
        let started = Instant::now();
        for samples in &batches {
            store.observe(samples).unwrap();
        }
        let micros = started.elapsed().as_secs_f64() * 1e6 / (BATCHES * BATCH) as f64;
        println!("{label} round {round}: {micros:.1} us per sample");
        best = best.min(micros);
    }
    println!("{label}: best of {ROUNDS}, {} samples per round: {best:.1} us per sample", BATCHES * BATCH);
}

#[test]
#[ignore = "a timing comparison, run by hand"]
fn observe_cost_per_sample() {
    measure("observe", Retentions::defaults());
}

/// The same run with retentions shorter than it, so every flush re-floors the pairs it prunes.
#[test]
#[ignore = "a timing comparison, run by hand"]
fn observe_cost_per_sample_while_pruning() {
    measure("observe-pruning", Retentions { s1: 60_000, m1: 600_000, h1: 3_600_000, d1: None });
}

/// A cache of 34 markets priced every second for `FROZEN_SECONDS`, frozen, then imported on the copy's root under
/// another key and on its tags under the key that wrote it: each step's time per row, the freeze's copy apart from
/// its read back for the root.
#[test]
#[ignore = "a timing measure, run by hand"]
fn frozen_copy_cost_per_row() {
    const FROZEN_MARKETS: u32 = 34;
    const FROZEN_SECONDS: i64 = 20_000;
    let (path, mut store) = fresh_store_at("marks-bench-frozen", Retentions::defaults());
    let base = 1_792_489_860_000;
    let samples: Vec<_> = (0..FROZEN_SECONDS)
        .flat_map(|second| (0..FROZEN_MARKETS).map(move |market| (market, base + second * 1_000, second)))
        .map(|(market, ts, second)| (market, ts, fixed(&format!("{}", 100 + second % 17))))
        .collect();
    for samples in samples.chunks(crate::history::marks::run::BATCH * 8) {
        store.observe(samples).unwrap();
    }
    let rows: i64 = store.conn.query_row("SELECT COUNT(*) FROM marks", [], |row| row.get(0)).unwrap();
    let per_row = |started: Instant| started.elapsed().as_secs_f64() * 1e6 / rows as f64;

    let copied = path.with_file_name("copied.sqlite");
    let started = Instant::now();
    store.conn.execute("VACUUM INTO ?1", [copied.to_str().unwrap()]).unwrap();
    let copy = per_row(started);
    let offset = crate::log::Offset { seq: 9, index: 1 };
    let started = Instant::now();
    let frozen = store.freeze(offset).unwrap();
    let freeze = per_row(started);
    let frozen_copy = crate::history::marks::store::frozen_path(path.parent().unwrap(), offset);
    let mut other =
        Store::open(&scratch_dir("marks-bench-frozen-other").join(MARKS_FILE), Retentions::defaults(), [0x24; 32])
            .unwrap();
    let started = Instant::now();
    other.import_frozen(&frozen_copy, &frozen).unwrap();
    let on_root = per_row(started);
    let mut same = fresh_store("marks-bench-frozen-same", Retentions::defaults());
    let started = Instant::now();
    same.import_frozen(&frozen_copy, &crate::history::marks::store::Frozen { root: None, ..frozen }).unwrap();
    let on_tags = per_row(started);
    println!(
        "{rows} rows: copy {copy:.2} us, freeze {freeze:.2} us (read back {:.2} us), import on the root {on_root:.2} us, on tags {on_tags:.2} us per row",
        freeze - copy
    );
}
