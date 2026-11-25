//! A rollover's rows derived again from the newest daily snapshot before them, and the history index's last command
//! read where a start from a snapshot observes none of the log before it: the log's segments before the snapshot
//! may be gone from this host.

use std::path::Path;

use super::*;
use crate::daily_snapshots::{Job, Signing, publish_now};
use crate::history::HistoryPlan;
use crate::log::{NotLocal, Offset};
use crate::sequencer::history_rebuild::Rederive;
use crate::snapshot::sealing::Encoding;

/// A command's rows gone from memory and the store are derived again from the newest daily snapshot before it, read
/// only from there on, so the segments before it may be gone; a replay from a snapshot counts no full replay. With no
/// snapshot the replay reads the log from its start, which is refused, naming the archive, once those segments are
/// gone.
#[test]
fn a_commands_rows_are_derived_again_from_the_newest_snapshot_before_it() {
    let (rig, at, kept) = rolled("snapshot-rows");
    let entries = log_entries(&rig.dir);
    let snapshots = scratch_dir("snapshot-rows-daily");
    let [.., older, newer] = checkpoints_before(&entries, at)[..] else {
        panic!("two checkpoints before the rollover")
    };
    for index in [older, newer] {
        publish(&snapshots, &entries, index);
    }
    let command = command_at(&entries, at);
    remove_segments_before(&rig.dir, seq_of(&entries, newer));
    let gone = || watch::channel(DerivedRows::default()).1;
    let (history, replays) = (scratch_dir("snapshot-rows-history"), Counter::default());
    let store = Arc::new(RowsStore::new(&history, RETENTION.as_millis() as i64));
    let mut deriver = Deriver::new(gone(), Some((rig.dir.clone(), rig.manifest.clone())))
        .storing(store.clone(), replays.clone())
        .from_snapshots(Some(snapshots.clone()));
    assert_eq!(deriver.rows(&command).unwrap(), Some(kept.clone()), "the rows from the newer snapshot");
    assert_eq!(replays.get(), 0, "no replay from the log's start");
    assert_eq!(store.derived(&command).unwrap().map(|derived| derived.rows), Some(kept), "and stored");
    let error = Deriver::new(gone(), Some((rig.dir.clone(), rig.manifest.clone()))).rows(&command).unwrap_err();
    assert!(error.downcast_ref::<NotLocal>().is_some(), "from the log's start: {error:#}");
}

/// A repair derives the rows of a rollover its store lacks from the newest daily snapshot before it, reading the log
/// only from there on, and stores them; a store that holds them gives them with no read of the log.
#[test]
fn a_repair_derives_a_rollovers_rows_from_the_newest_snapshot_before_it() {
    let (rig, at, kept) = rolled("snapshot-repair");
    let entries = log_entries(&rig.dir);
    let snapshots = scratch_dir("snapshot-repair-daily");
    let newer = *checkpoints_before(&entries, at).last().expect("a checkpoint before the rollover");
    publish(&snapshots, &entries, newer);
    let command = command_at(&entries, at);
    remove_segments_before(&rig.dir, seq_of(&entries, newer));
    let history = scratch_dir("snapshot-repair-history");
    let store = RowsStore::new(&history, RETENTION.as_millis() as i64).from_snapshots(Some(snapshots));
    let running = AtomicBool::new(false);
    assert_eq!(Rederive::new(&rig.dir, &store).rows(&command, &running).unwrap(), Some(kept.clone()));
    assert_eq!(store.derived(&command).unwrap().map(|derived| derived.rows), Some(kept.clone()), "stored");
    let empty = scratch_dir("snapshot-repair-empty");
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(Rederive::new(&empty, &store).rows(&command, &running).unwrap(), Some(kept), "from the store");
    let bare = RowsStore::new(&scratch_dir("snapshot-repair-bare"), RETENTION.as_millis() as i64);
    let error = Rederive::new(&rig.dir, &bare).rows(&command, &running).unwrap_err();
    assert!(error.downcast_ref::<NotLocal>().is_some(), "from the log's start: {error:#}");
}

/// A replica's derivation of the rows before its snapshot starts from the newest daily snapshot stamped more than the
/// store's retention before the snapshot's command, reading the log only from there on; one stamped within the
/// retention is passed over for genesis, as is every snapshot without a directory of them.
#[test]
fn the_rows_before_a_replicas_snapshot_are_derived_from_a_snapshot_older_than_the_retention() {
    let (mut rig, at, _) = rolled("snapshot-through");
    sequenced(&mut rig.seq, Command::Tick);
    let through = rig.seq.engine.last_seq().expect("a command");
    rig.seq.flush().unwrap();
    let entries = log_entries(&rig.dir);
    let snapshots = scratch_dir("snapshot-through-daily");
    let index = *checkpoints_before(&entries, at).last().expect("a checkpoint before the rollover");
    let stamped = publish(&snapshots, &entries, index).ts;
    let through_ts = command_at(&entries, through).sequenced.ts;
    remove_segments_before(&rig.dir, seq_of(&entries, index));
    let running = AtomicBool::new(false);
    let store = |name: &str, retention: Millis, daily: Option<PathBuf>| {
        RowsStore::new(&scratch_dir(name), retention).from_snapshots(daily)
    };
    let older = store("snapshot-through-older", through_ts - stamped - 1, Some(snapshots.clone()));
    assert_eq!(older.derive_through(&rig.dir, through, &running).unwrap(), Some(1), "the rollover's rows");
    assert!(older.derived(&command_at(&entries, at)).unwrap().is_some(), "kept");
    let within = store("snapshot-through-within", through_ts - stamped, Some(snapshots));
    let error = within.derive_through(&rig.dir, through, &running).unwrap_err();
    assert!(error.downcast_ref::<NotLocal>().is_some(), "a snapshot within the retention: {error:#}");
    let none = store("snapshot-through-none", through_ts - stamped - 1, None);
    let error = none.derive_through(&rig.dir, through, &running).unwrap_err();
    assert!(error.downcast_ref::<NotLocal>().is_some(), "no snapshots: {error:#}");
}

/// A start from a daily snapshot observes none of the log before it: the history index whose last command is at or
/// before the snapshot's checkpoint resumes once that one entry is read, unread it is discarded and the log taken
/// from its start again, and so it is when the entry cannot be read, its segment only in the archive.
#[tokio::test]
async fn an_index_whose_last_command_is_before_a_snapshot_resumes_from_one_read() {
    let (mut rig, at, _) = rolled("snapshot-index");
    let [read, unread, archived] = [
        index(&rig, "snapshot-index-read").await,
        index(&rig, "snapshot-index-unread").await,
        index(&rig, "snapshot-index-archived").await,
    ];
    // The log goes on past the index, through segments after the one holding its last command.
    for _ in 0..3 * crate::test_support::SEGMENT_COMMANDS {
        sequenced(&mut rig.seq, Command::Tick);
    }
    rig.seq.flush().unwrap();
    let snapshot = rig.seq.engine.last_seq().expect("a command");
    let resumed = |cfg: &HistorySection, observe: bool| {
        let mut plan = HistoryPlan::open(cfg).unwrap();
        if observe {
            plan.observe_before(&rig.dir, snapshot);
        }
        plan.finish().unwrap().resume_at()
    };
    assert_eq!(resumed(&read, true), Offset::start_of(at + 1), "the index resumes after its last command");
    assert_eq!(resumed(&unread, false), Offset::start_of(0), "unread, the last command is not the log's");
    remove_segments_before(&rig.dir, at + crate::test_support::SEGMENT_COMMANDS);
    assert_eq!(resumed(&archived, true), Offset::start_of(0), "its segment is only in the archive");
}

// ── Helpers ──

/// A rig whose log holds a rollover at the seq returned, after checkpoints, with the rows the sequencer kept for it.
fn rolled(name: &str) -> (Rig, u64, crate::history::derived::Rows) {
    let mut rig = Rig::new(name);
    rig.open(2);
    let due = rig.seq.engine.next_rollover().expect("a rollover is due");
    rig.move_to(due + 100);
    sequenced(&mut rig.seq, Command::Tick);
    let at = rig.seq.engine.last_seq().expect("a command");
    let kept = rig.seq.derived.borrow().get(at).map(|derived| derived.rows.clone()).expect("kept");
    rig.seq.flush().unwrap();
    (rig, at, kept)
}

/// A legacy history index in a directory named for `name` that has taken every command `rig`'s log holds.
async fn index(rig: &Rig, name: &str) -> HistorySection {
    let cfg = HistorySection { dir: scratch_dir(name), retention: RETENTION, marks: None };
    let mut history = open_history(&cfg, &rig.dir).unwrap();
    history.catch_up_to(rig.seq.engine.last_seq());
    let (live, subscriber) = broadcast::channel(1);
    drop(live);
    let manifest = published_manifest(&rig.dir);
    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let feed = Feed::new(subscriber, rig.dir.clone(), manifest.clone(), backfills, Some(history.resume_at()));
    history.run(feed, Deriver::new(rig.seq.derived_rows(), Some((rig.dir.clone(), manifest)))).await.unwrap();
    cfg
}

/// The indices among `entries` of the checkpoints before the command at `seq`.
fn checkpoints_before(entries: &[LogEntry], seq: u64) -> Vec<usize> {
    let checkpoints = entries.iter().enumerate().filter_map(|(index, entry)| match entry {
        LogEntry::Checkpoint(checkpoint) if checkpoint.seq < seq => Some(index),
        _ => None,
    });
    checkpoints.collect()
}

/// The seq of the checkpoint at `entries[index]`.
fn seq_of(entries: &[LogEntry], index: usize) -> u64 {
    let LogEntry::Checkpoint(checkpoint) = &entries[index] else { panic!("no checkpoint at entry {index}") };
    checkpoint.seq
}

/// Publishes a daily snapshot in `dir` of the checkpoint at `entries[index]`, its engine a replay of the entries
/// through it, and lists it.
fn publish(dir: &Path, entries: &[LogEntry], index: usize) -> crate::daily_snapshots::Listed {
    let LogEntry::Checkpoint(checkpoint) = entries[index].clone() else { panic!("no checkpoint at entry {index}") };
    let offset = entries[..=index].iter().fold(None, |last, entry| crate::log::offset_of(last, entry).ok()).unwrap();
    let engine = crate::sequencer::replay(&entries[..=index]).unwrap().engine;
    assert!(engine.deposit_finality_enabled(), "a snapshot a replay can start from");
    let encoded = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    publish_now(dir, Job::bare(offset, checkpoint, engine.now(), encoded, Signing::for_tests())).unwrap()
}

/// Removes the log's segment files in `dir` before the one holding the command at `seq`, as a host that keeps only
/// the archive's copy of them does.
fn remove_segments_before(dir: &Path, seq: u64) {
    let mut segments: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            let first = crate::log::segment_seq(path.file_name()?.to_str()?)?;
            Some((first, path))
        })
        .collect();
    segments.sort();
    let holding = segments.iter().rposition(|(first, _)| *first <= seq).expect("a segment holds the seq");
    assert!(holding > 0, "a segment before the one holding seq {seq}");
    segments[..holding].iter().for_each(|(_, path)| std::fs::remove_file(path).unwrap());
}
