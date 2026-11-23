//! A carried genesis, from the node's side (`engine_core::carry`): the carry file `engine carry` builds from a
//! source log's published checkpoint, what the first start of a log naming `[genesis.carry]` checks on chain
//! before it sequences one (`boot`), and `engine replay --carry-source`'s check of a carried log against its
//! source (`replay`).
//!
//! The file is not pinned anywhere: the engine proves every value it carries against the source root, and the
//! start requires that root to be the one the source vault last accepted, once the guardian has retired its
//! signer. So the file can be built after the launch release is approved, from the source's final checkpoint.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, ensure};
use clap::Args;
use engine_core::carry::{CarryPart, CarrySource, StateCarried};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::log::{self, LogEntry, SegmentedLog};

pub(crate) mod boot;
mod chain;
mod replay;
pub use replay::{CarriedFrom, check_carried};

/// The largest carry file built or read: three quarters of the largest archive file a member fetches, since the
/// genesis's carry parts all fall in the log's first segment, which members and standbys fetch whole. A source
/// whose carry is larger is refused, rather than open a log no member could follow.
const MAX_FILE_BYTES: u64 = crate::standby::MAX_ARCHIVE_BYTES / 4 * 3;

/// `engine carry`'s arguments.
#[derive(Args)]
pub struct CarryArgs {
    /// The source's published log: a log directory, a downloaded archive, or a single log file.
    #[arg(short, long)]
    log: PathBuf,
    /// The checkpoint to carry from: the one the source vault last accepted. The log's last checkpoint when
    /// absent.
    #[arg(long)]
    seq: Option<u64>,
    /// Where to write the carry file, which must not exist yet; standard output when absent.
    #[arg(short, long, conflicts_with = "check")]
    out: Option<PathBuf>,
    /// Only check that the checkpoint can be carried: write nothing, and fail naming the first refusal.
    #[arg(long)]
    check: bool,
}

/// Runs `engine carry`: builds the carry file and writes it to `--out` or standard output, printing what it
/// carries and its SHA-256 to standard error; with `--check`, only builds it and prints what it would carry.
/// Error cases:
/// - see `build` and `write`
pub fn run(args: CarryArgs) -> anyhow::Result<()> {
    let file = build(&args.log, args.seq)?;
    let totals = file.totals;
    let summary = format!(
        "seq {}, root {}, {} accounts, {} entitled, {} staked, {} awards",
        totals.source.seq, totals.source.root, totals.accounts, totals.entitled, totals.staked, totals.awards
    );
    if args.check {
        encode(&file)?;
        eprintln!("carriable: {summary}");
        return Ok(());
    }
    let sha256 = match &args.out {
        Some(out) => write(&file, out)?,
        None => {
            let bytes = encode(&file)?;
            std::io::stdout().write_all(&bytes).context("writing the carry file")?;
            Sha256::digest(&bytes).into()
        }
    };
    eprintln!("carried: {summary}; sha256 {}", hex::encode(sha256));
    Ok(())
}

/// Runs `engine replay --carry-source`'s check of the carried log at `carried`, whose replay recorded
/// `replayed` as its carried source, against `source`, printing it.
/// Error cases:
/// - the replay recorded no carried source, or another than its parts name: a part was refused
/// - see `check_carried`
pub fn report_carried(carried: &Path, replayed: Option<CarrySource>, source: &Path) -> anyhow::Result<()> {
    let from = check_carried(carried, source)?;
    ensure!(
        replayed == Some(from.source),
        "the log's carry parts name {:?}, but its replay carried from {replayed:?}: a part was refused",
        from.source
    );
    println!(
        "carried from: chain {}, vault {}, seq {}, root {}, signed by {}; {} accounts and {} awards match the source",
        from.source.chain_id,
        from.source.vault,
        from.source.seq,
        from.source.root,
        from.signer,
        from.accounts,
        from.awards
    );
    Ok(())
}

/// What `engine carry` writes: the parts of a genesis carrying a source log's state at one of its checkpoints,
/// in order, with what they carry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarryFile {
    pub source: CarrySource,
    pub totals: StateCarried,
    pub parts: Vec<CarryPart>,
}

/// The carry file of the published log at `path` at its checkpoint `seq`, or its last checkpoint without one.
/// The log's commands must reproduce that checkpoint's root; signatures are not checked, since the new log's
/// first start requires the root to be the one the source vault accepted.
/// Error cases:
/// - the log cannot be read, holds no checkpoint, or none at `seq`, or its commands do not reproduce its root
/// - the source's state cannot be carried (`Engine::carry_parts`): before `V90`, results in the open emission
///   period, a staking frontier that misses a block it mirrored, rotation on, or an award proposal waiting
pub fn build(path: &Path, seq: Option<u64>) -> anyhow::Result<CarryFile> {
    let seq = match seq {
        Some(seq) => seq,
        None => last_checkpoint(path)?.with_context(|| format!("{} holds no checkpoint", path.display()))?,
    };
    let engine = crate::exit::engine_at_checkpoint(path, seq)?;
    let (parts, totals) =
        engine.carry_parts().map_err(|refused| anyhow!("the log's state at seq {seq} cannot be carried: {refused}"))?;
    Ok(CarryFile { source: totals.source, totals, parts })
}

/// The seq of the last checkpoint the published log at `path` holds, if any.
/// Error cases:
/// - the log cannot be read or fails its manifest check
fn last_checkpoint(path: &Path) -> anyhow::Result<Option<u64>> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut last = None;
    let mut visit = |entry: LogEntry| {
        if let LogEntry::Checkpoint(checkpoint) = entry {
            last = Some(checkpoint.seq);
        }
        Ok(())
    };
    if found.is_dir() {
        SegmentedLog::open(path, &mut visit).map(drop)?;
    } else {
        log::read_file(path, &mut visit)?;
    }
    Ok(last)
}

/// `file` as JSON.
/// Error cases:
/// - it is longer than `MAX_FILE_BYTES`
fn encode(file: &CarryFile) -> anyhow::Result<Vec<u8>> {
    let bytes = serde_json::to_vec(file).context("encoding the carry file")?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        "the carry is {} bytes, past the {MAX_FILE_BYTES} the launch log's first segment can hold for its members",
        bytes.len()
    );
    Ok(bytes)
}

/// Writes `file` as JSON to `out`, which must not exist yet, and returns the SHA-256 of the bytes written.
/// Error cases:
/// - `file` is longer than `MAX_FILE_BYTES`, or `out` exists or cannot be written
pub fn write(file: &CarryFile, out: &Path) -> anyhow::Result<[u8; 32]> {
    let bytes = encode(file)?;
    let mut created = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .with_context(|| format!("creating {}, which must not exist yet", out.display()))?;
    created
        .write_all(&bytes)
        .and_then(|()| created.sync_all())
        .with_context(|| format!("writing {}", out.display()))?;
    Ok(Sha256::digest(&bytes).into())
}

/// The carry file at `path`, with the SHA-256 of its bytes.
/// Error cases:
/// - the file cannot be read, is longer than `MAX_FILE_BYTES`, or is not a carry file
/// - its source is not the one its parts begin with, or its parts do not end with `End`
pub fn read(path: &Path) -> anyhow::Result<(CarryFile, [u8; 32])> {
    let length = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.len();
    ensure!(length <= MAX_FILE_BYTES, "{} is longer than a carry file may be", path.display());
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file: CarryFile =
        serde_json::from_slice(&bytes).with_context(|| format!("{} is not a carry file", path.display()))?;
    let begins = matches!(file.parts.first(), Some(CarryPart::Begin { source, .. }) if *source == file.source);
    ensure!(begins && file.totals.source == file.source, "{}'s parts do not begin with its source", path.display());
    ensure!(file.parts.last() == Some(&CarryPart::End), "{}'s parts do not end the carry", path.display());
    Ok((file, Sha256::digest(&bytes).into()))
}

#[cfg(test)]
mod tests;
