//! The replay's cost on a log of launch flow kept between runs, for profiling: `REPLAY_BENCH_DIR` holds
//! the book's encoding and the log, written on the first run and replayed on each, its seals checked at
//! the end of each run, as `engine replay` checks them, or with `REPLAY_BENCH_SEALS=every` each in full
//! ahead of the replay by `RECOVERY_REPLAY_WORKERS` threads (one per core). Each run prints its wall
//! time and the CPU time the process spent on it, on every thread.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine_core::types::AccountId;

use crate::common::launch_book::{NamedCounts, build};
use crate::log::{ReplayWorkers, SealChecks};
use crate::sequencer::recovery_scale::flow::Flow;
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::paired::{Paired, node_config};
use crate::sequencer::recovery_scale::restart::replay;
use crate::sequencer::recovery_scale::{ACCOUNTS, Base, TRADERS, cpu_secs, env, mix, rates};
use crate::tee::EnclaveKey;

#[test]
#[ignore = "the replay's cost on a kept log, for profiling"]
fn replay_bench() {
    let dir = std::path::PathBuf::from(std::env::var("REPLAY_BENCH_DIR").expect("REPLAY_BENCH_DIR"));
    let key = Arc::new(EnclaveKey::test_key());
    let signer = AccountId(key.address());
    if !dir.join("base.json").exists() {
        write(&dir, key);
    }
    let started = Instant::now();
    let base = Base { encoded: std::fs::read_to_string(dir.join("base.json")).expect("the book's encoding") };
    println!("BENCH base_read_s={:.1}", started.elapsed().as_secs_f64());
    let cores = std::thread::available_parallelism().expect("the cores this process may use");
    let workers = ReplayWorkers::from(env("RECOVERY_REPLAY_WORKERS", cores));
    let every = std::env::var("REPLAY_BENCH_SEALS").is_ok_and(|seals| seals == "every");
    let seals = if every { SealChecks::Every(workers) } else { SealChecks::RunEnds };
    for _ in 0..env("REPLAY_BENCH_RUNS", 1) {
        println!("BENCH replay_start pid={} seals={seals:?}", std::process::id());
        let cpu = cpu_secs();
        // The kept log's launch leg is not known: the whole log is timed.
        let replayed = replay(&base, &dir.join("log"), signer, seals, u64::MAX);
        let cpu = cpu_secs() - cpu;
        println!(
            "BENCH replay commands={} checkpoints={} s={:.1} commands_per_s={:.0} cpu_s={cpu:.1} cpu_us_per_command={:.1}",
            replayed.commands,
            replayed.checkpoints,
            replayed.took.as_secs_f64(),
            replayed.rate(),
            cpu * 1e6 / replayed.commands as f64
        );
    }
}

#[test]
#[ignore = "the replay's cost in ranges from snapshots of a kept log, for profiling"]
fn ranges_bench() {
    use crate::sequencer::replay::ranges::fixture::snapshot_log;
    use crate::sequencer::replay::ranges::{Trust, replay_ranges_from};

    let dir = std::path::PathBuf::from(std::env::var("REPLAY_BENCH_DIR").expect("REPLAY_BENCH_DIR"));
    let signer = AccountId(EnclaveKey::test_key().address());
    let base = Base { encoded: std::fs::read_to_string(dir.join("base.json")).expect("the book's encoding") };
    // A snapshot every `RANGES_BENCH_MINUTES` of sequenced time, where a node takes one a day, written once.
    let snapshots = dir.join(format!("snapshots-{}m", env("RANGES_BENCH_MINUTES", 10)));
    if !snapshots.exists() {
        let every = env("RANGES_BENCH_MINUTES", 10i64) * 60_000;
        let mut last = None;
        let started = Instant::now();
        let signing = crate::daily_snapshots::Signing::for_tests();
        let written = snapshot_log(base.engine(), &dir.join("log"), (&snapshots, &signing), |_, ts| {
            let due = last.is_none_or(|last| ts.div_euclid(every) > last);
            last = due.then_some(ts.div_euclid(every)).or(last);
            due
        })
        .expect("the log snapshots");
        println!("BENCH snapshots written={written} s={:.1}", started.elapsed().as_secs_f64());
    }
    // What starting a range costs: a snapshot's load, with its resting-order index rebuilt, and its root
    // computed over every key.
    let (_, last) = crate::daily_snapshots::list(&snapshots).expect("the snapshots").pop().expect("a snapshot");
    let (started, cpu) = (Instant::now(), cpu_secs());
    let mut start = crate::daily_snapshots::load(&last).expect("a snapshot loads");
    let loaded = started.elapsed();
    start.engine.state_root();
    println!(
        "BENCH snapshot bytes={} load_s={:.2} root_s={:.2} cpu_s={:.2}",
        std::fs::metadata(&last).expect("the snapshot").len(),
        loaded.as_secs_f64(),
        (started.elapsed() - loaded).as_secs_f64(),
        cpu_secs() - cpu
    );
    drop(start);
    // What a replay with `--custody-config` adds before its ranges: a scan of the log for its checkpoints, its
    // segments one per core at a time (`log::uncommanded`), beside a whole read on one thread for scale.
    let cores = std::thread::available_parallelism().expect("the cores this process may use");
    let started = Instant::now();
    let found = crate::log::uncommanded(&dir.join("log"), cores, Some).expect("the log scans").len();
    println!("BENCH custody_scan entries={found} threads={cores} s={:.1}", started.elapsed().as_secs_f64());
    let started = Instant::now();
    crate::log::SegmentedLog::open(&dir.join("log"), &mut |_| Ok(())).expect("the log reads");
    println!("BENCH whole_read s={:.1}", started.elapsed().as_secs_f64());
    let default = crate::sequencer::replay::ranges::default_jobs();
    let jobs = std::num::NonZeroUsize::new(env("RANGES_BENCH_JOBS", default.get())).expect("jobs");
    for _ in 0..env("REPLAY_BENCH_RUNS", 1) {
        let (started, cpu) = (Instant::now(), cpu_secs());
        let trust = Trust::Pinned { domain: None, signer: Some(signer) };
        let audited = replay_ranges_from(&|| base.engine(), &dir.join("log"), &snapshots, trust, jobs)
            .expect("the log replays in ranges");
        let (took, cpu) = (started.elapsed(), cpu_secs() - cpu);
        println!(
            "BENCH ranges ranges={} jobs={jobs} commands={} checkpoints={} s={:.1} commands_per_s={:.0} cpu_s={cpu:.1} \
             cpu_us_per_command={:.1}",
            audited.ranges,
            audited.commands,
            audited.checkpoints,
            took.as_secs_f64(),
            audited.commands as f64 / took.as_secs_f64(),
            cpu * 1e6 / audited.commands as f64
        );
    }
}

#[test]
#[ignore = "a member's start from a daily snapshot against a whole replay, on a kept log, for profiling"]
fn member_start_bench() {
    use crate::daily_snapshots::{Signing, open_signed};
    use crate::log::{LogEntry, Offset, SegmentedLog, Stop, offset_of};
    use crate::sequencer::recovery_scale::flow::SEGMENT_COMMANDS;
    use crate::sequencer::recovery_scale::handover::copy_log;
    use crate::sequencer::recovery_scale::restart::DOMAIN;
    use crate::sequencer::replay::Before;
    use crate::sequencer::replay::ranges::fixture::snapshot_log;
    use crate::standby::Follower;

    let dir = std::path::PathBuf::from(std::env::var("REPLAY_BENCH_DIR").expect("REPLAY_BENCH_DIR"));
    let key = Arc::new(EnclaveKey::test_key());
    let signer = AccountId(key.address());
    if !dir.join("base.json").exists() {
        write(&dir, key.clone());
    }
    let base = Base { encoded: std::fs::read_to_string(dir.join("base.json")).expect("the book's encoding") };
    // The member's copy, its completed segments linked, and a snapshot `MEMBER_BENCH_PERCENT` of the way through
    // it, as a member's newest is a day into thirty, each written once.
    let copy = dir.join("member-copy");
    if !copy.exists() {
        copy_log(&dir.join("log"), &copy).expect("the member's copy");
    }
    let percent = env("MEMBER_BENCH_PERCENT", 90u64);
    let snapshots = dir.join(format!("member-snapshots-{percent}"));
    if !snapshots.exists() {
        let last = crate::log::last_checkpoint(&copy).expect("the copy reads").expect("a checkpoint").seq;
        let (target, mut taken) = (last * percent / 100, false);
        let signing = Signing { key: key.clone(), domain: DOMAIN };
        snapshot_log(base.engine(), &copy, (&snapshots, &signing), |checkpoint, _| {
            let due = !taken && checkpoint.seq >= target;
            taken |= due;
            due
        })
        .expect("the snapshot is written");
    }
    let cores = std::thread::available_parallelism().expect("the cores this process may use");
    let workers = ReplayWorkers::from(env("RECOVERY_REPLAY_WORKERS", cores));
    let open = |snapshot| {
        let ids = (DOMAIN, signer, [signer].into());
        let follower = Follower::open_seeded(
            (&copy, SEGMENT_COMMANDS, workers),
            ids,
            (None, None, None, None),
            (|| base.engine(), snapshot),
            |_| Ok(()),
        );
        follower.expect("the replica builds")
    };
    let (started, cpu) = (Instant::now(), cpu_secs());
    let whole = open(None);
    let (took, cpu) = (started.elapsed(), cpu_secs() - cpu);
    let (commands, head) = (whole.replica().last_seq().expect("a command") + 1, whole.replica().head_link());
    println!(
        "BENCH member_whole commands={commands} s={:.1} commands_per_s={:.0} cpu_s={cpu:.1}",
        took.as_secs_f64(),
        commands as f64 / took.as_secs_f64()
    );
    drop(whole);

    let (started, cpu) = (Instant::now(), cpu_secs());
    let (_, newest) = crate::daily_snapshots::list(&snapshots).expect("the snapshots").pop().expect("a snapshot");
    let signed = open_signed(&newest, DOMAIN, &[signer].into()).expect("a usable snapshot");
    let (at, loaded) = (signed.snapshot.offset, started.elapsed());
    let from = open(Some(signed));
    let (took, cpu) = (started.elapsed(), cpu_secs() - cpu);
    assert_eq!((from.replays(), from.replica().head_link()), (0, head), "the replica from the snapshot");
    drop(from);
    println!(
        "BENCH member_snapshot snapshot_seq={} load_s={:.1} s={:.1} cpu_s={cpu:.1}",
        at.seq,
        loaded.as_secs_f64(),
        took.as_secs_f64()
    );

    // The pass alone, over the copy before the snapshot's checkpoint.
    let snapshot = crate::daily_snapshots::load(&snapshots.join(crate::snapshot::name(at))).expect("the snapshot");
    let mut pass = Before::new(DOMAIN, [signer].into(), &snapshot.engine, &[]);
    drop(snapshot);
    pass.check_seals_ahead(&copy, workers);
    let (started, cpu, mut previous, mut passed) = (Instant::now(), cpu_secs(), None::<Offset>, 0u64);
    SegmentedLog::read_span_ahead(&copy, 0, |entry: LogEntry| {
        let offset = offset_of(previous, &entry)?;
        previous = Some(offset);
        if offset == at {
            return Err(Stop.into());
        }
        passed += u64::from(matches!(entry, LogEntry::Command(_)));
        pass.entry(&entry)
    })
    .expect("the pass");
    let (took, cpu) = (started.elapsed(), cpu_secs() - cpu);
    println!(
        "BENCH member_pass commands={passed} s={:.1} commands_per_s={:.0} cpu_s={cpu:.1} cpu_us_per_command={:.1}",
        took.as_secs_f64(),
        passed as f64 / took.as_secs_f64(),
        cpu * 1e6 / passed as f64
    );
}

/// Writes the book's encoding and `RECOVERY_LAUNCH_SECS` of launch flow on it into `dir`, through a node
/// started as the scenario starts one, which captures no cache in that time.
fn write(dir: &Path, key: Arc<EnclaveKey>) {
    std::fs::create_dir_all(dir).expect("the bench's directory");
    let (accounts, launch_secs) = (env("RECOVERY_ACCOUNTS", ACCOUNTS), env("RECOVERY_LAUNCH_SECS", 3_600));
    let counts = NamedCounts { adders: 1, openers: env("RECOVERY_TRADERS", TRADERS), closeouts: 0 };
    let mut book = build(&mix(accounts, 0, launch_secs), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let base = Base::new(&engine).expect("the book encodes");
    let rates = rates(&engine);
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop((book, engine));
    let signer = AccountId(key.address());
    let cfg = node_config(dir, Duration::from_secs(2 * launch_secs), (signer, ReplayWorkers::available()));
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        let mut paired = Paired::start(base.engine(), &cfg, key).expect("the node takes over the book");
        let mut flow = Flow::new(traders, rates);
        let started = Instant::now();
        flow.run(&mut paired.node, Duration::from_secs(launch_secs), 1.0, |_, _, _| {});
        let last = paired.node.checkpoint_now();
        println!("BENCH wrote seq={} s={:.0}", last.seq, started.elapsed().as_secs_f64());
        paired.stop().await.expect("the node stops");
    });
    // Written last, so a run stopped before it writes the log again.
    std::fs::write(dir.join("base.json"), &base.encoded).expect("the book's encoding is written");
}

/// The kind of a command, for the apply bench's breakdown.
fn kind(cmd: &engine_core::command::Command) -> &'static str {
    use engine_core::command::Command;
    match cmd {
        Command::Prices { .. } => "prices",
        Command::Tick => "tick",
        _ => "other",
    }
}

#[test]
#[ignore = "the apply thread's parts on a kept log, for profiling"]
fn apply_bench() {
    use std::collections::BTreeMap;

    use crate::log::command_seal::command_link;
    use crate::log::{LogEntry, SegmentedLog};

    let dir = std::path::PathBuf::from(std::env::var("REPLAY_BENCH_DIR").expect("REPLAY_BENCH_DIR"));
    let base = Base { encoded: std::fs::read_to_string(dir.join("base.json")).expect("the book's encoding") };
    let mut engine = base.engine();
    let mut first_root = true;
    let mut tree = None;
    let (mut applied, mut rooted, mut sealed, mut linked) =
        (BTreeMap::<&str, (u64, Duration)>::new(), Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut roots, mut leaves, mut nodes) = (0u64, 0usize, 0usize);
    let mut link = engine_core::eip712::B256::ZERO;
    let started = Instant::now();
    SegmentedLog::open(&dir.join("log"), |entry| {
        match entry {
            LogEntry::Command(c) => {
                let at = Instant::now();
                link = command_link(link, &c).expect("a link");
                linked += at.elapsed();
                if let Some(seal) = c.seal.as_ref().filter(|_| sealed < Duration::from_secs(5)) {
                    let at = Instant::now();
                    seal.verify(link).expect("a seal");
                    sealed += at.elapsed();
                    applied.entry("seal").or_default().0 += 1;
                }
                let at = Instant::now();
                let events = engine.apply(&c.sequenced).expect("applies");
                let took = at.elapsed();
                let slot = applied.entry(kind(&c.sequenced.cmd)).or_default();
                slot.0 += 1;
                slot.1 += took;
                assert_eq!(c.events.as_ref(), Some(&events));
            }
            LogEntry::Checkpoint(cp) => {
                link = crate::log::command_seal::checkpoint_link(&cp).expect("a link");
                let at = Instant::now();
                let root = if first_root {
                    first_root = false;
                    let root = engine.state_root();
                    tree = engine.detach_tree();
                    engine_core::eip712::B256::from(root)
                } else {
                    let tree = tree.as_mut().expect("detached");
                    let (root, work) = engine.commit_inputs().apply(tree);
                    tree.release(cp.seq, root);
                    tree.collect(usize::MAX);
                    roots += 1;
                    leaves += work.leaves;
                    nodes += work.nodes;
                    root
                };
                rooted += at.elapsed();
                assert_eq!(format!("{root}"), cp.root, "the root at seq {}", cp.seq);
            }
            LogEntry::Withdrawal(_) => {}
        }
        Ok(())
    })
    .expect("the log reads");
    let wall = started.elapsed();
    let commands: u64 = applied.iter().filter(|(k, _)| **k != "seal").map(|(_, (n, _))| n).sum();
    let apply: Duration = applied.values().map(|(_, d)| *d).sum();
    println!(
        "BENCH apply_bench commands={commands} wall_s={:.1} apply_s={:.1} root_s={:.1} link_s={:.1} rest_s={:.1}",
        wall.as_secs_f64(),
        apply.as_secs_f64(),
        rooted.as_secs_f64(),
        linked.as_secs_f64(),
        (wall - apply - rooted - linked - sealed).as_secs_f64()
    );
    for (kind, (n, took)) in &applied {
        if *kind == "seal" {
            println!("BENCH seal n={n} us_each={:.1}", sealed.as_secs_f64() * 1e6 / *n as f64);
        } else {
            println!("BENCH apply kind={kind} n={n} us_each={:.1}", took.as_secs_f64() * 1e6 / *n as f64);
        }
    }
    println!(
        "BENCH roots n={roots} ms_each={:.2} leaves_each={:.0} nodes_each={:.0} us_per_command={:.1}",
        rooted.as_secs_f64() * 1e3 / roots.max(1) as f64,
        leaves as f64 / roots.max(1) as f64,
        nodes as f64 / roots.max(1) as f64,
        rooted.as_secs_f64() * 1e6 / commands as f64
    );
}
