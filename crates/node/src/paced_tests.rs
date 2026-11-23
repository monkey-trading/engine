use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{Paced, STEP};
use crate::test_support::scratch_dir;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect()
}

#[test]
fn a_large_write_goes_in_steps_and_the_file_holds_every_byte() {
    let dir = scratch_dir("paced-steps");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("paced");
    let bytes = pattern(2 * STEP + STEP / 2 + 1);
    let mut file = File::create(&path).unwrap();
    let mut paced = Paced::new(&mut file);

    assert_eq!(paced.write(&bytes).unwrap(), STEP, "a write stops at the step's end, where the data is synced");
    assert_eq!(paced.since_sync, 0);
    assert_eq!(paced.write(&bytes[STEP..STEP + 10]).unwrap(), 10);
    assert_eq!(paced.since_sync, 10);
    paced.write_all(&bytes[STEP + 10..]).unwrap();
    assert_eq!(paced.since_sync, (bytes.len() - STEP) % STEP);
    drop(paced);
    file.sync_all().unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_small_write_is_taken_whole() {
    let dir = scratch_dir("paced-small");
    std::fs::create_dir_all(&dir).unwrap();
    let file = File::create(dir.join("small")).unwrap();
    let mut paced = Paced::new(file);
    assert_eq!(paced.write(b"abc").unwrap(), 3);
    assert_eq!(paced.since_sync, 3);
    assert_eq!(paced.get_ref().metadata().unwrap().len(), 3);
}

/// The log's `fdatasync` tail beside a derived store's large write on one volume: with no write, with the
/// write unpaced (one `write_all`, then `sync_all`, as a recovery cache was staged) and with it paced.
/// `PACED_BENCH_DIR` names the volume (default the temp directory), `PACED_BENCH_MIB` the write (1024) and
/// `PACED_BENCH_ROUNDS` how many times it is made (4).
/// `cargo test -p engine-node --lib paced::tests::log_sync_tail -- --ignored --nocapture`
#[test]
#[ignore = "a measurement: writes several GiB to the disk it runs on"]
fn log_sync_tail_beside_a_large_derived_write() {
    let env = |name: &str, default: usize| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let (mib, rounds) = (env("PACED_BENCH_MIB", 1024), env("PACED_BENCH_ROUNDS", 4));
    let dir = match std::env::var_os("PACED_BENCH_DIR") {
        Some(base) => Path::new(&base).join(format!("paced-bench-{}", std::process::id())),
        None => scratch_dir("paced-bench"),
    };
    std::fs::create_dir_all(&dir).unwrap();
    let bytes = pattern(mib << 20);
    let unpaced = |path: &Path| {
        let mut file = File::create(path).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    };
    let paced = |path: &Path| {
        let mut file = File::create(path).unwrap();
        Paced::new(&mut file).write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    };
    let cases: [(&str, Option<&(dyn Fn(&Path) + Sync)>); 3] =
        [("none", None), ("unpaced", Some(&unpaced)), ("paced", Some(&paced))];
    for (name, write) in cases {
        let done = AtomicBool::new(false);
        let mut taken = std::thread::scope(|scope| {
            let log = scope.spawn(|| {
                let mut log = File::create(dir.join(format!("log-{name}"))).unwrap();
                let (record, mut taken) = (pattern(16 << 10), Vec::new());
                while !done.load(Ordering::Acquire) {
                    log.write_all(&record).unwrap();
                    let started = Instant::now();
                    log.sync_data().unwrap();
                    taken.push(started.elapsed());
                    std::thread::sleep(Duration::from_millis(2));
                }
                taken
            });
            std::thread::sleep(Duration::from_millis(500));
            match write {
                Some(write) => {
                    for round in 0..rounds {
                        let path = dir.join(format!("derived-{name}-{round}"));
                        write(&path);
                        std::fs::remove_file(path).unwrap();
                    }
                }
                None => std::thread::sleep(Duration::from_secs(5)),
            }
            done.store(true, Ordering::Release);
            log.join().unwrap()
        });
        taken.sort();
        let at = |q: f64| taken[((taken.len() as f64 * q) as usize).min(taken.len() - 1)].as_secs_f64() * 1e3;
        println!(
            "{name:>8}: {} syncs, p50 {:.2} ms, p99 {:.2} ms, p99.9 {:.2} ms, max {:.2} ms",
            taken.len(),
            at(0.5),
            at(0.99),
            at(0.999),
            at(1.0)
        );
    }
    if std::env::var_os("PACED_BENCH_DIR").is_some() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
