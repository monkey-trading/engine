//! The log-sync thread writes the log: while a sync runs, the sequencer's thread makes no call on the
//! log file, across a segment rotation too, so a filesystem that holds writes during a sync cannot
//! hold the sequencer. An inline sync waits for the jobs in flight, so the file keeps their order.

use std::path::Path;
use std::time::Duration;

use engine_core::command::Command;

use crate::log::SegmentedLog;
use crate::sequencer::Sequencer;
use crate::sequencer::pipeline::Batch;
use crate::sequencer::pipeline::hold_tests::held_sequencer;
use crate::sequencer::pipeline::tests::Verdict;
use crate::test_support::{SEGMENT_COMMANDS, scratch_path};

/// How long, in real time, a test waits for the sync thread to start a sync.
const SYNC_WAIT: Duration = Duration::from_secs(10);

#[test]
fn the_sequencers_thread_writes_nothing_to_the_log_while_a_sync_runs() {
    let label = "pipeline-writes-on-the-sync-thread";
    let (mut seq, permits, entered) = held_sequencer(label);
    let dir = scratch_path(label);
    let before = log_bytes(&dir);
    // The first batch's sync is held before it writes anything; the batches behind it cross a
    // segment boundary, so rotation's seal and the new segment's entries wait behind it too.
    stage_ticks(&mut seq, 1);
    entered.recv_timeout(SYNC_WAIT).expect("the thread starts the first sync");
    stage_ticks(&mut seq, 2 * SEGMENT_COMMANDS as usize);
    assert_eq!(log_bytes(&dir), before, "the sequencer's thread wrote to the log while a sync ran");
    // An inline write now would land ahead of the held jobs' entries, so the writer refuses it.
    let refused = seq.log.flush().expect_err("an inline write while jobs hold entries");
    assert!(format!("{refused:#}").contains("sync jobs hold unwritten entries"), "{refused:#}");
    // Enough verdicts for every job's sync; the thread takes one per sync it runs.
    for _ in 0..32 {
        permits.send(Verdict::Pass).unwrap();
    }
    // An inline sync settles the jobs in flight first, then writes what is left, in order.
    seq.sequence(Command::Tick).unwrap().unwrap();
    seq.sync_log(true).unwrap();
    assert!(log_bytes(&dir) > before, "the sync thread wrote the held batches");
    assert_eq!(seq.log.published().flushed, seq.log.last_offset(), "everything is durable and published");
    let reopened = SegmentedLog::open(&dir, |_| Ok(())).expect("the log opens, its entries in order");
    assert_eq!(reopened.last_seq(), seq.engine.last_seq(), "every command reached the log");
}

// ── Helpers ──

/// Stages `batches` one-tick batches, as `run` would.
fn stage_ticks(seq: &mut Sequencer, batches: usize) {
    for _ in 0..batches {
        seq.sequence(Command::Tick).unwrap().unwrap();
        let release = seq.stage(Batch::default()).unwrap();
        seq.submit(release).unwrap();
    }
}

/// The bytes of every segment in `dir`, staged ones included.
fn log_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".jsonl"))
        .map(|entry| entry.metadata().unwrap().len())
        .sum()
}
