//! Daily snapshots for a log no node published them for, written at the checkpoints its replay passes:
//! for the tests and the bench of a replay in ranges.

use std::path::Path;

use engine_core::Engine;
use engine_core::types::Millis;

use crate::daily_snapshots::{Job, Signing, publish_now};
use crate::log::{self, Checkpoint, LogEntry, SegmentedLog};
use crate::sequencer::replay::Replay;
use crate::snapshot::sealing::Encoding;

/// Replays the log directory at `path` whole from `base`'s state, and at each checkpoint at which `due`,
/// given the checkpoint and its sequenced time, says one is due, publishes the engine there into `out` as a
/// daily snapshot, signed with `signing`; how many it published.
/// Error cases:
/// - the log does not replay, or a snapshot cannot be encoded or published
pub(crate) fn snapshot_log(
    base: Engine,
    path: &Path,
    (out, signing): (&Path, &Signing),
    due: impl FnMut(&Checkpoint, Millis) -> bool,
) -> anyhow::Result<usize> {
    snapshot_log_under(Replay::default(), base, path, (out, signing), due)
}

/// `snapshot_log` with the replay `trusting` set up with its trust, as a log with withdrawal entries needs.
/// Error cases:
/// - as `snapshot_log`
pub(in crate::sequencer) fn snapshot_log_under(
    trusting: Replay,
    base: Engine,
    path: &Path,
    (out, signing): (&Path, &Signing),
    mut due: impl FnMut(&Checkpoint, Millis) -> bool,
) -> anyhow::Result<usize> {
    let mut replay = Replay { engine: base, ..trusting };
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
                let carried = Encoding::Encoded(serde_json::to_string(&replay.carried()?)?);
                let history = crate::daily_snapshots::Pending::none();
                publish_now(out, Job { offset, checkpoint, ts, engine, carried, history, signing: signing.clone() })?;
                published += 1;
            }
        }
        Ok(())
    });
    replay.settled(read)?;
    Ok(published)
}

/// Rewrites the daily snapshot at `file` in the format before the one that carries the replay state, which
/// splits a replay in ranges as one range of the log each.
pub(crate) fn without_carried(file: &Path) {
    let mut snapshot: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let object = snapshot.as_object_mut().unwrap();
    object.insert("schema".into(), 2.into());
    object.remove("carried");
    object.remove("history");
    std::fs::write(file, serde_json::to_vec(&snapshot).unwrap()).unwrap();
}
