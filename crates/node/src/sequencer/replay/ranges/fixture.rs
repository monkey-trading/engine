//! Daily snapshots for a log no node published them for, written at the checkpoints its replay passes:
//! for the tests and the bench of a replay in ranges.

use std::path::Path;

use engine_core::Engine;
use engine_core::types::Millis;

use crate::daily_snapshots::{Job, publish_now};
use crate::log::{self, Checkpoint, LogEntry, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::snapshot::sealing::Encoding;

/// Replays the log directory at `path` whole from `base`'s state, and at each checkpoint at which `due`,
/// given the checkpoint and its sequenced time, says one is due, publishes the engine there into `out` as a
/// daily snapshot; how many it published.
/// Error cases:
/// - the log does not replay, or a snapshot cannot be encoded or published
pub(crate) fn snapshot_log(
    base: Engine,
    path: &Path,
    out: &Path,
    mut due: impl FnMut(&Checkpoint, Millis) -> bool,
) -> anyhow::Result<usize> {
    let mut replay = Replay::default();
    replay.engine = base;
    replay.check_run_ends();
    replay.check_roots_ahead();
    let (mut at, mut published) = (None, 0);
    let read = SegmentedLog::open_ahead(path, |entry| {
        let offset = log::offset_of(at, &entry)?;
        at = Some(offset);
        replay.apply(&entry)?;
        if let LogEntry::Checkpoint(checkpoint) = entry {
            let ts = replay.engine.now();
            if due(&checkpoint, ts) {
                let engine = Encoding::Encoded(serde_json::to_string(&replay.engine)?);
                publish_now(out, Job { offset, checkpoint, ts, engine })?;
                published += 1;
            }
        }
        Ok(())
    });
    replay.settled(read)?;
    Ok(published)
}
