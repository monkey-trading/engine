//! A node's start from its newest daily snapshot and the log after it, which `scripts/dev-snapshot-start.sh` runs, on
//! deterministic time with isolated temporary state: a node writing launch flow on a small book across two UTC
//! midnights publishes a daily snapshot at its first checkpoint and at the first past each midnight, each carrying the
//! replay state its stats worker folded; with the segments before the newest snapshot's checkpoint moved out of its
//! log, it starts from that snapshot, reading only the log after it, to what a whole replay of the full log gives, and
//! sequences; a member with an empty copy builds its replica from the snapshot with no whole replay and takes the
//! node's turn through the tail handover; a snapshot whose carried state was altered is passed over with Warning
//! `snapshot_start_refused`; and a replay of the whole log in ranges from the snapshots agrees with its whole replay,
//! each snapshot's carried state checked against the fold of the range before it, and refuses the altered one.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::types::AccountId;

use crate::clock::Clock;
use crate::common::launch_book::{NamedCounts, build};
use crate::config::HistorySection;
use crate::daily_snapshots::{self, Bounds, NoRoots, snapshot_start, snapshot_start_against};
use crate::history::derived::RowsStore;
use crate::log::{LogEntry, Offset, ReplayWorkers, SealChecks, SegmentedLog};
use crate::sequencer::Sequencer;
use crate::sequencer::recovery_scale::flow::{Flow, Node};
use crate::sequencer::recovery_scale::handover::{
    build_replica, copy_log, hand_over, member_config, start_from_replica,
};
use crate::sequencer::recovery_scale::member_snapshot::serve;
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::restart::DOMAIN;
use crate::sequencer::recovery_scale::{Base, mix};
use crate::sequencer::replay::ranges::{Trust, replay_ranges_from};
use crate::sequencer::replay::{Audited, remove_segments_before, replay_path_from};
use crate::standby::archive::{archive_client, bootstrap_archive};
use crate::tee::EnclaveKey;
use crate::test_support::{capture_alerts, scratch_dir};

/// A UTC day, and how long before each midnight the flow starts and how long it runs.
const DAY_MS: i64 = 86_400_000;
const BEFORE_MIDNIGHT: Duration = Duration::from_secs(600);
const FLOW: Duration = Duration::from_secs(1_200);

#[test]
#[ignore = "a dev scenario: scripts/dev-snapshot-start.sh runs it"]
fn dev_scenario_snapshot_start() {
    let counts = NamedCounts { adders: 1, openers: 8, closeouts: 0 };
    let mut book = build(&mix(200, 0, 1_200), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let base = Base::new(&engine).expect("the book encodes");
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop((book, engine));
    let run = scratch_dir("snapshot-start");
    let (log, snapshots) = (run.join("log"), run.join("snapshots"));
    let (archive, copy) = (run.join("archive"), run.join("copy"));
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());
    let signers = [signer].into();

    // The node publishes a snapshot at its first checkpoint, and at its first past each of two midnights.
    let mut node = Node::publishing(base.engine(), &log, key.clone(), Some(&snapshots));
    let mut flow = Flow::new(traders, BTreeMap::from([(1, 11.0)]));
    for _ in 0..2 {
        let to_midnight = DAY_MS - node.clock.now_ms().rem_euclid(DAY_MS);
        node.clock.advance(Duration::from_millis(u64::try_from(to_midnight).unwrap()) - BEFORE_MIDNIGHT);
        flow.run(&mut node, FLOW, 1.0, |_, _, _| {});
    }
    node.checkpoint_now();
    // The writer publishes the last day's snapshot once the stats worker has encoded what it carries, which the
    // node's drop would wait for only so long.
    let published = std::time::Instant::now() + Duration::from_secs(120);
    let listed = || daily_snapshots::read_manifest(&snapshots).expect("the manifest");
    while listed().len() < 3 && std::time::Instant::now() < published {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(node);
    let listed = listed();
    assert_eq!(listed.len(), 3, "a snapshot at the first checkpoint and past each midnight: {listed:?}");
    let newest = listed[2].clone();
    let at = Offset { seq: newest.seq, index: newest.index };
    print_sizes(&snapshots.join(&newest.file));

    // The whole log, as the archive keeps it, and what its whole replay gives.
    copy_log(&log, &archive).expect("the archive's copy");
    let whole = replay_path_from(base.engine(), &archive, signer, SealChecks::RunEnds).expect("the whole log replays");
    let after = entries_after(&archive, at);
    let total = entries_after(&archive, Offset::start_of(0)) + 1;

    // The segments before the newest snapshot's checkpoint leave the node's log.
    let removed = remove_segments_before(&log, newest.seq);
    assert!(removed > 0, "the log held segments before the snapshot");

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("a runtime");
    runtime.block_on(async {
        // The node starts from the newest snapshot, reading only the entries after its checkpoint, to the whole
        // replay's state, and sequences.
        let bounds = Bounds { finalized: u64::MAX, before: None, keep_from: None };
        let start = snapshot_start_against(None, (&snapshots, &log), (DOMAIN, &signers), bounds, &NoRoots)
            .await
            .expect("the newest snapshot is usable");
        assert_eq!(start.snapshot.offset, at, "the start takes the newest snapshot");
        let mut read = 0;
        let trust = (Some(DOMAIN), Some(signer), None);
        let workers = ReplayWorkers::available();
        let observe = |_: &LogEntry| read += 1;
        let started = std::time::Instant::now();
        let recovered =
            Sequencer::read_log_starting(start, &log, trust, Default::default(), observe, None, None, None, workers)
                .expect("the node starts from the snapshot");
        let took = started.elapsed();
        assert_eq!(recovered.snapshot_sequence(), Some(newest.seq), "a start from the snapshot");
        assert_eq!(read, after, "the start read only the entries after the snapshot's checkpoint");
        let state = |engine: &engine_core::Engine| serde_json::to_value(engine).expect("the engine encodes");
        assert_eq!(state(&recovered.replayed.engine), state(&whole.engine), "the whole replay's state");
        assert_eq!(recovered.replayed.command_prefix, whole.command_prefix, "the whole replay's command prefix");
        println!(
            "SNAPSHOT_START entries_read={read} log_entries={total} removed_segments={removed} start_s={:.2}",
            took.as_secs_f64()
        );
        let mut node = Node::recovered(recovered, &log, key.clone(), None).expect("the node recovers");
        node.first_command().expect("the node sequences after its start");

        // A member with an empty copy builds its replica from the snapshot, with no whole replay, and takes the
        // node's turn through the tail handover.
        let origins = [serve(&archive, &snapshots).await];
        let client = archive_client(false).expect("an archive client");
        bootstrap_archive(&client, &origins, &copy).await.expect("the archive backfills the copy");
        let held = crate::log::last_checkpoint(&copy).expect("the copy reads").expect("a checkpoint").seq;
        let member_snapshots = run.join("member-snapshots");
        let signed = snapshot_start(&client, &origins, (&member_snapshots, &copy), DOMAIN, &signers, (held, None))
            .await
            .expect("the snapshot is usable");
        assert_eq!(signed.snapshot.offset, at, "the member took the newest snapshot");
        let history = HistorySection { dir: run.join("member-history"), ..crate::test_support::dev_config().history };
        let rows = Arc::new(RowsStore::new(&run.join("history"), history.retention_ms().expect("a retention")));
        let (mut follower, stats, _) = build_replica(&base, (&copy, Some(signed)), (signer, workers), (rows, &history));
        assert_eq!(follower.replays(), 0, "the member replayed nothing whole");
        let cfg = member_config(&run);
        let peak = flow.peak_commands_per_sec() * 600.0;
        let switch = hand_over((&mut node, &mut flow, &key), (&mut follower, &cfg, 1), (64 << 10, peak), &log).await;
        assert!(switch.confirmed, "the node confirmed the handover");
        let (member, _) = start_from_replica(follower, stats, &copy, key.clone());
        assert!(member.seq.last_seq() > Some(held), "the member sequences past the node's head");

        // A snapshot whose carried state was altered is passed over, and paged: the older ones are of checkpoints
        // the node's log no longer holds, so no snapshot starts it.
        let path = snapshots.join(&newest.file);
        let written = std::fs::read(&path).expect("the snapshot");
        edit(&path, &written, |snapshot| {
            let commands = snapshot["carried"]["commands"].as_u64().expect("the carried command count");
            snapshot["carried"]["commands"] = (commands + 1).into();
        });
        let (_guard, mut alerts) = capture_alerts();
        let refused = snapshot_start_against(None, (&snapshots, &log), (DOMAIN, &signers), bounds, &NoRoots).await;
        assert!(refused.is_none(), "an altered snapshot is not used");
        let alert = alerts.try_recv().expect("the refusal is paged");
        assert_eq!((alert["alert"].as_str(), alert["level"].as_str()), ("snapshot_start_refused", "WARN"));

        // The whole log replays in ranges from the snapshots to what it replays whole, each snapshot's carried
        // state checked against the fold of the range before it, and the altered one refused.
        let trust = || Trust::Pinned { domain: None, signer: Some(signer) };
        let jobs = NonZeroUsize::new(2).expect("two jobs");
        let error = replay_ranges_from(&|| base.engine(), &archive, &snapshots, trust(), jobs).err();
        let error = format!("{:#}", error.expect("the altered snapshot is refused"));
        assert!(error.contains(&format!("daily snapshot {} does not match the log", path.display())), "{error}");
        std::fs::write(&path, &written).expect("the snapshot restored");
        let in_ranges = replay_ranges_from(&|| base.engine(), &archive, &snapshots, trust(), jobs).expect("ranges");
        let whole = Audited::from(whole);
        assert_eq!(in_ranges.ranges, 3, "a range from genesis and one from each later snapshot");
        assert_eq!((in_ranges.commands, in_ranges.checkpoints), (whole.commands, whole.checkpoints));
        assert_eq!(state(&in_ranges.engine), state(&whole.engine), "the ranges end in the whole log's state");
    });
}

// ── Helpers ──

/// How many entries the log in `dir` holds after the one at `at`.
fn entries_after(dir: &Path, at: Offset) -> usize {
    let mut count = 0;
    let mut last = None;
    SegmentedLog::open(dir, |entry| {
        let offset = crate::log::offset_of(last, &entry)?;
        last = Some(offset);
        count += usize::from(offset > at);
        Ok(())
    })
    .expect("the log reads");
    count
}

/// Prints the bytes of the snapshot at `path` and of the engine, the replay state and the history it holds.
fn print_sizes(path: &Path) {
    let text = std::fs::read_to_string(path).expect("the snapshot");
    let written: serde_json::Value = serde_json::from_str(&text).expect("the snapshot's JSON");
    let bytes = |field: &str| serde_json::to_string(&written[field]).expect("a field encodes").len();
    println!(
        "SNAPSHOT_SIZES file_bytes={} engine_bytes={} carried_bytes={} history_bytes={}",
        text.len(),
        bytes("engine"),
        bytes("carried"),
        bytes("history")
    );
}

/// Rewrites the daily snapshot at `path` as `written`, its bytes as published, with `alter` applied, its format
/// first as the writer puts it.
fn edit(path: &Path, written: &[u8], alter: impl FnOnce(&mut serde_json::Value)) {
    let mut snapshot: serde_json::Value = serde_json::from_slice(written).expect("the snapshot's JSON");
    alter(&mut snapshot);
    let schema = snapshot.as_object_mut().expect("the snapshot's fields").remove("schema").expect("its format");
    let rest = serde_json::to_string(&snapshot).expect("the snapshot encodes");
    std::fs::write(path, format!("{{\"schema\":{schema},{}", &rest[1..])).expect("the snapshot rewritten");
}
