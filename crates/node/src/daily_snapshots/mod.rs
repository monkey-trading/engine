//! Daily snapshots: the engine's state at the first checkpoint of each UTC day by sequenced time,
//! published beside the log's archive, so that a replay can start a range of the log there rather than
//! from genesis. Nothing signs a snapshot: a replay takes one only at a checkpoint the log holds, equal to
//! the snapshot's and signed by a signer the replay trusts, once the root of the decoded state, its
//! resting-order index rebuilt from the accounts' orders, is that checkpoint's, and accepts the range only
//! once the range before it ends in that state, by the digest of its whole encoding (`state_digest`,
//! `sequencer::replay::ranges`). Each is `<seq>-<index>.json` after its checkpoint's offset in the log, as
//! recovery caches are named, and `manifest.jsonl` lists one line per snapshot.

mod writer;
#[cfg(test)]
pub(crate) use writer::publish_now;
pub(crate) use writer::{Job, Writer};

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use sha2::Digest;

use crate::log::{Checkpoint, Offset};

/// The manifest's file name.
pub(crate) const MANIFEST: &str = "manifest.jsonl";
/// The snapshot format this build writes and reads.
const SCHEMA: u32 = 1;
/// A UTC day, in sequenced milliseconds.
const DAY_MS: Millis = 86_400_000;

/// A daily snapshot as a replay reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DailySnapshot {
    pub schema: u32,
    /// The offset of its checkpoint's entry in the log.
    pub offset: Offset,
    /// The checkpoint as the log holds it.
    pub checkpoint: Checkpoint,
    /// The engine's state after the command the checkpoint covers.
    pub engine: Engine,
}

/// A daily snapshot as the node writes it, with the engine's encoding as it was given.
#[derive(Serialize)]
struct Written<'a> {
    schema: u32,
    offset: Offset,
    checkpoint: &'a Checkpoint,
    engine: &'a RawValue,
}

/// A snapshot's line in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Listed {
    pub file: String,
    pub seq: u64,
    pub index: u32,
    /// The checkpoint's sequenced time: the stamp of the command it covers.
    pub ts: Millis,
    pub root: String,
    pub bytes: u64,
    pub sha256: String,
}

/// The UTC day `ts` falls on, counted from 1970-01-01.
pub(crate) fn day(ts: Millis) -> i64 {
    ts.div_euclid(DAY_MS)
}

/// The offset a snapshot's file name gives, or `None` for a name that is not one's.
pub(crate) fn offset_named(file: &str) -> Option<Offset> {
    let (seq, index) = file.strip_suffix(".json")?.split_once('-')?;
    let digits = |part: &str, len| part.len() == len && part.bytes().all(|byte| byte.is_ascii_digit());
    if !(digits(seq, 20) && digits(index, 10)) {
        return None;
    }
    Some(Offset { seq: seq.parse().ok()?, index: index.parse().ok()? })
}

/// The snapshot files in `dir`, by the offsets their names give, in log order.
/// Error cases:
/// - `dir` cannot be listed
pub(crate) fn list(dir: &Path) -> anyhow::Result<Vec<(Offset, PathBuf)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing daily snapshots in {}", dir.display()))? {
        let entry = entry.with_context(|| format!("listing daily snapshots in {}", dir.display()))?;
        if let Some(offset) = entry.file_name().to_str().and_then(offset_named) {
            found.push((offset, entry.path()));
        }
    }
    found.sort_unstable_by_key(|(offset, _)| (offset.seq, offset.index));
    Ok(found)
}

/// The manifest in `dir`, or no listing when it has none.
/// Error cases:
/// - the manifest cannot be read, or a line is not a listing
pub(crate) fn read_manifest(dir: &Path) -> anyhow::Result<Vec<Listed>> {
    let path = dir.join(MANIFEST);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| serde_json::from_str(line).with_context(|| format!("{} line {}", path.display(), n + 1)))
        .collect()
}

/// The daily snapshot at `path`, as a range's start state: the checkpoint it names, and the engine at it
/// with its resting-order index rebuilt from its accounts' orders (`Engine::rebuild_resting`) and its
/// transient indexes restored. Nothing else is checked here: a replay checks its root against the checkpoint
/// the log holds at its offset, which it requires to be this one, and its state against the state the range
/// before it ends in.
/// Error cases:
/// - the file cannot be read or decoded, or is of another format
/// - its name, offset, checkpoint and engine name different seqs
/// - its engine predates `V27`, before which the staking references were not leaves, or custody finality,
///   before which legacy credits are replay state outside the engine
pub(crate) fn load(path: &Path) -> anyhow::Result<DailySnapshot> {
    let file = File::open(path).with_context(|| format!("opening daily snapshot {}", path.display()))?;
    let mut snapshot: DailySnapshot = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("decoding daily snapshot {}", path.display()))?;
    let DailySnapshot { schema, offset, checkpoint, engine } = &mut snapshot;
    let named = path.file_name().and_then(|name| name.to_str()).and_then(offset_named);
    ensure!(*schema == SCHEMA, "daily snapshot {} is of format {schema}, not {SCHEMA}", path.display());
    ensure!(named == Some(*offset), "daily snapshot {} names offset {offset}", path.display());
    ensure!(
        checkpoint.seq == offset.seq && engine.last_seq() == Some(offset.seq),
        "daily snapshot {}: its checkpoint and engine stand at seqs {} and {:?}, not its offset's",
        path.display(),
        checkpoint.seq,
        engine.last_seq()
    );
    ensure!(
        engine.protocol_version() >= ProtocolVersion::V27 && engine.deposit_finality_enabled(),
        "daily snapshot {} predates V27 or custody finality, so its state holds less than a replay needs",
        path.display()
    );
    engine.rebuild_resting();
    engine.restore_indexes();
    Ok(snapshot)
}

/// The SHA-256 of `engine`'s encoding, which binds every part of its state, those its root leaves out
/// included: a replay in ranges compares it where one range ends and the next starts.
/// Error cases:
/// - the engine cannot be encoded
pub(crate) fn state_digest(engine: &Engine) -> anyhow::Result<[u8; 32]> {
    let mut hashed = BufWriter::with_capacity(1 << 16, writer::Hashed::new(std::io::sink()));
    serde_json::to_writer(&mut hashed, engine).context("encoding the engine")?;
    let hashed = hashed.into_inner().map_err(|e| e.into_error()).context("hashing the engine's encoding")?;
    Ok(hashed.hash.finalize().into())
}

#[cfg(test)]
mod tests;
