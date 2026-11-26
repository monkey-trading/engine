//! Daily snapshots: the engine's state at the first checkpoint the node writes on each UTC day by sequenced time,
//! published beside the log's archive, so that a replay can start a range of the log there, and a member its
//! replica, rather than from genesis. A replay in ranges takes one only at a checkpoint the log holds, equal to
//! the snapshot's and signed by a signer the replay trusts, once the root of the decoded state, its
//! resting-order index rebuilt from the accounts' orders, is that checkpoint's, and accepts the range only
//! once the range before it ends in that state, by the digest of its whole encoding (`state_digest`,
//! `sequencer::replay::ranges`). The node's enclave key signs each snapshot over that digest (`signature`),
//! which a member that starts its replica from one requires, since nothing before it is replayed. Each is
//! `<seq>-<index>.json` after its checkpoint's offset in the log, as recovery caches are named, and
//! `manifest.jsonl` lists one line per snapshot. With `[archive]`, each goes to the bucket as it is published, and
//! the host keeps its copy for `local_window` (`upload`); the manifest lists it still, and a reader of one the host
//! no longer holds reads it from the bucket.

mod bucket;
pub(crate) use bucket::{SnapshotBucket, snapshots_prefix};
mod fetch;
pub(crate) use fetch::{Takes, canonical, download, manifest};
mod published;
#[cfg(test)]
pub(crate) use published::tests::serve as serve_snapshots;
pub use published::{LocalSnapshots, PublishedSnapshots};
mod history;
pub(crate) use history::{History, Pending};
mod signature;
pub(crate) use signature::{Covered, Signed, Signing, open_signed, sha256};
mod start;
#[cfg(test)]
pub(crate) use start::snapshot_start;
pub(crate) use start::{AcceptedRoots, Bounds, Fetching, NoRoots, entry_at, snapshot_start_against};
mod upload;
pub(crate) use upload::SnapshotUploader;
mod vault;
pub(crate) use vault::{VaultRoots, accepted_signers, anchor_signer};
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
use crate::sequencer::replay::Carried;

/// The manifest's file name.
pub(crate) const MANIFEST: &str = "manifest.jsonl";
/// The snapshot format this build writes: signed over its state, the replay state it carries and its history
/// anchors. A start takes only this format; a replay in ranges reads the two before it too.
pub(crate) const SCHEMA: u32 = 3;

/// How many of a snapshot's first bytes name its format, as every writer puts it first (`{"schema":N,`).
const FORMAT_BYTES: usize = 12;

/// Whether `head`, a snapshot's first bytes, names a format before the one this build writes, which carries no
/// replay state: what a start reads to pass over such a snapshot before it decodes or fetches the rest. Any other
/// head is left for the snapshot's checks to refuse.
fn earlier_format(head: &[u8]) -> bool {
    (UNSIGNED_SCHEMA..SCHEMA).any(|schema| head.starts_with(format!("{{\"schema\":{schema},").as_bytes()))
}
/// The format signed over its state alone, which carries no replay state.
const STATE_SCHEMA: u32 = 2;
/// The format before signatures.
const UNSIGNED_SCHEMA: u32 = 1;
/// The most bytes a snapshot a member starts from may hold: several times the 420 MB of one at 100,000 accounts,
/// and below the memory a member's guest holds, since the member reads it whole to check its signature first.
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 2 << 30;
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
    /// The SHA-256 of the engine's encoding, hex (`state_digest`), from the signed format on.
    #[serde(default)]
    pub state: Option<String>,
    /// The replay state the log before it folds to (`Carried`), from the format that carries it on.
    #[serde(default)]
    pub carried: Option<Carried>,
    /// The anchors of the history the node derives from the log (`History`), from the format that carries the
    /// replay state on.
    #[serde(default)]
    pub history: Option<Box<RawValue>>,
    /// The enclave key's signature over the snapshot (`signature::digest`), from the signed format on.
    #[serde(default)]
    pub signature: Option<String>,
}

/// A daily snapshot as the node writes it, with the engine's encoding as it was given.
#[derive(Serialize)]
struct Written<'a> {
    schema: u32,
    offset: Offset,
    checkpoint: &'a Checkpoint,
    engine: &'a RawValue,
    state: &'a str,
    carried: &'a RawValue,
    history: &'a RawValue,
    signature: &'a str,
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

/// Lists `listed` as the daily snapshots in `dir`, as the writer lists them.
#[cfg(test)]
pub(crate) fn write_listing(dir: &Path, listed: &[Listed]) {
    writer::write_manifest(dir, listed).expect("the listing is written");
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
/// - as `read`
pub(crate) fn load(path: &Path) -> anyhow::Result<DailySnapshot> {
    let mut snapshot = read(path)?;
    snapshot.engine.rebuild_resting();
    snapshot.engine.restore_indexes();
    Ok(snapshot)
}

/// The daily snapshot at `path` as it was written, its indexes not yet rebuilt.
/// Error cases:
/// - the file cannot be read or decoded, or is of another format, or of the signed format without its state
///   digest and signature
/// - its name, offset, checkpoint and engine name different seqs
/// - its engine predates `V27`, before which the staking references were not leaves, or custody finality,
///   before which legacy credits are replay state outside the engine
fn read(path: &Path) -> anyhow::Result<DailySnapshot> {
    let file = File::open(path).with_context(|| format!("opening daily snapshot {}", path.display()))?;
    let snapshot: DailySnapshot = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("decoding daily snapshot {}", path.display()))?;
    let DailySnapshot { schema, offset, checkpoint, state, signature, carried, history, .. } = &snapshot;
    ensure!(
        [UNSIGNED_SCHEMA, STATE_SCHEMA, SCHEMA].contains(schema),
        "daily snapshot {} is of format {schema}, not {UNSIGNED_SCHEMA} to {SCHEMA}",
        path.display()
    );
    ensure!(
        *schema == UNSIGNED_SCHEMA || (state.is_some() && signature.is_some()),
        "daily snapshot {} is of the signed format but carries no state digest or signature",
        path.display()
    );
    ensure!(
        (*schema == SCHEMA) == (carried.is_some() && history.is_some()),
        "daily snapshot {} of format {schema} carries replay state and history anchors only in format {SCHEMA}",
        path.display()
    );
    ensure_named(path, *offset, checkpoint)?;
    ensure_engine(path, &snapshot)?;
    Ok(snapshot)
}

/// Requires the daily snapshot at `path` to name `offset`, which `checkpoint` stands at.
/// Error cases:
/// - its name and offset name different offsets, or its checkpoint another seq
fn ensure_named(path: &Path, offset: Offset, checkpoint: &Checkpoint) -> anyhow::Result<()> {
    let named = path.file_name().and_then(|name| name.to_str()).and_then(offset_named);
    ensure!(named == Some(offset), "daily snapshot {} names offset {offset}", path.display());
    ensure!(
        checkpoint.seq == offset.seq,
        "daily snapshot {}: its checkpoint stands at seq {}, not its offset's",
        path.display(),
        checkpoint.seq
    );
    Ok(())
}

/// Requires the engine of `snapshot`, at `path`, to stand at its offset with what a replay from it needs.
/// Error cases:
/// - the engine stands at another seq
/// - it predates `V27`, before which the staking references were not leaves, or custody finality, before
///   which legacy credits are replay state outside the engine
fn ensure_engine(path: &Path, snapshot: &DailySnapshot) -> anyhow::Result<()> {
    let DailySnapshot { offset, checkpoint, engine, .. } = snapshot;
    ensure!(
        engine.last_seq() == Some(offset.seq),
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
    Ok(())
}

/// The format and the checkpoint the daily snapshot at `path` names, read without decoding its engine.
/// Error cases:
/// - the file cannot be read, or is not a snapshot's JSON
pub(crate) fn named(path: &Path) -> anyhow::Result<(u32, Checkpoint)> {
    /// A snapshot's format and checkpoint, its other fields read past.
    #[derive(Deserialize)]
    struct Named {
        schema: u32,
        checkpoint: Checkpoint,
    }
    let file = File::open(path).with_context(|| format!("opening daily snapshot {}", path.display()))?;
    let named: Named = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("reading daily snapshot {}'s checkpoint", path.display()))?;
    Ok((named.schema, named.checkpoint))
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

/// Publishes into `dir` a snapshot of an engine that applied ticks up to `seq`, the last stamped `ts`, signed with the
/// published mock key and carrying nothing: for a test of a read that checks only its bytes.
#[cfg(test)]
pub(crate) fn publish_ticks(dir: &Path, seq: u64, ts: Millis) -> Listed {
    use engine_core::command::{Command, Sequenced};
    let mut engine = Engine::default();
    for seq in 0..=seq {
        engine.apply(&Sequenced { seq, ts, cmd: Command::Tick }).expect("a tick applies");
    }
    let checkpoint = Checkpoint {
        parent: None,
        seq,
        root: format!("0x{}", hex::encode(engine.state_root())),
        signature: String::new(),
        signer: format!("0x{}", hex::encode([1; 20])),
        domain: None,
        deferred_withdrawals: false,
    };
    let engine = crate::snapshot::sealing::Encoding::Encoded(serde_json::to_string(&engine).expect("it encodes"));
    let job = Job::bare(Offset { seq, index: 1 }, checkpoint, ts, engine, Signing::for_tests());
    publish_now(dir, job).expect("the snapshot publishes")
}

/// No replay state and no history anchors, as a daily snapshot carries them: for a test of the writer or of a read
/// that does not look at them.
#[cfg(test)]
pub(crate) fn nothing_carried() -> (crate::snapshot::sealing::Encoding, Pending) {
    use crate::snapshot::sealing::Encoding;
    let carried = serde_json::to_string(&Carried::default()).expect("a default state encodes");
    (Encoding::Encoded(carried), Pending::none())
}

#[cfg(test)]
mod tests;
