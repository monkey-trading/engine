//! The public event log: one JSON object per line, split into segment files. Command entries are
//! the engine's entire input, with the events each produced. Withdrawal entries carry the enclave's
//! signature on each approved withdrawal. Checkpoint entries carry the state root after a given seq,
//! signed by the enclave key. The same entries stream live over `/v1/stream`, each at an
//! [`Offset`].

mod append_integrity;
pub(crate) mod command_seal;
pub use command_seal::CommandSeal;
mod checked;
mod commitment;
pub(crate) use commitment::{command_digest, extend_command_prefix};
mod truncate;
pub(crate) use truncate::truncate_to_checkpoint;
mod archive;
pub(crate) use archive::open_segment_archive;
pub mod catalog;
pub mod catalog_access;
mod catalog_commit;
pub mod catalog_head;
pub mod catalog_owner;
mod catalog_prefix;
pub mod catalog_seal;
mod view;
pub use view::{ActivePrefix, ReadAuthority};
mod suffix;
mod writer;
pub use suffix::CatalogBoundary;
mod reader;
pub use reader::LogReader;
mod segments;
pub(crate) use segments::segment_seq;
pub use segments::{LogWriter, MANIFEST, Manifest, SegmentInfo, SegmentedLog};

use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, bail};
use engine_core::command::Sequenced;
use engine_core::event::Event;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::sequencer::SignedWithdrawal;

// Unboxed on purpose: boxing every command would add an allocation to the hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum LogEntry {
    Command(CommandEntry),
    Checkpoint(Checkpoint),
    Withdrawal(SignedWithdrawal),
}

/// A sequenced command and the events applying it produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEntry {
    #[serde(flatten)]
    pub sequenced: Sequenced,
    /// `None` only in entries written before events were logged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<Event>>,
    /// Missing only in historical entries; an uncovered missing seal refuses recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seal: Option<CommandSeal>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Signed compare-and-swap predecessor; absent for historical v1 signatures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<CheckpointParent>,
    pub seq: u64,
    /// 0x-prefixed keccak-256 state root.
    pub root: String,
    /// Enclave signature over the root; see `tee::EnclaveKey::sign_checkpoint`.
    pub signature: String,
    pub signer: String,
    /// Missing only in historical logs; recover the domain from their recorded operator or auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<CheckpointDomain>,
    /// Withdrawal signatures follow only after custody confirms this commitment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred_withdrawals: bool,
}

/// The exact prior vault commitment covered by a v2 checkpoint signature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointParent {
    pub has_checkpoint: bool,
    pub seq: u64,
    pub root: engine_core::eip712::B256,
}

/// The custody deployment a checkpoint commits to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointDomain {
    pub chain_id: u64,
    pub vault: engine_core::types::AccountId,
}

/// Where a message sits in the log: its seq, and its index among that seq's entries (the command
/// is 0, then its withdrawals, then any checkpoint). Written `<seq>:<index>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Offset {
    pub seq: u64,
    pub index: u32,
}

impl fmt::Display for Offset {
    /// `<seq>:<index>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.seq, self.index)
    }
}

impl FromStr for Offset {
    type Err = anyhow::Error;

    /// Parses `<seq>:<index>`.
    /// Error cases:
    /// - no `:`, or either side is not an unsigned integer in range
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (seq, index) = s.split_once(':').with_context(|| format!("offset {s:?} is not <seq>:<index>"))?;
        Ok(Self {
            seq: seq.parse().with_context(|| format!("offset {s:?}: seq"))?,
            index: index.parse().with_context(|| format!("offset {s:?}: index"))?,
        })
    }
}

impl Serialize for Offset {
    /// As its `<seq>:<index>` string.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Offset {
    /// From a `<seq>:<index>` string.
    /// Error cases:
    /// - the value is not a string in that form
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(|e| D::Error::custom(format!("{e:#}")))
    }
}

impl Offset {
    /// The first message of `seq`.
    pub fn start_of(seq: u64) -> Self {
        Self { seq, index: 0 }
    }

    /// The position right after this one within the same seq.
    pub fn next(self) -> Self {
        Self { seq: self.seq, index: self.index.saturating_add(1) }
    }
}

/// The offset of `entry`, given the offset of the entry before it. Every segment starts with a
/// command, so `prev` is `None` only there.
/// Error cases:
/// - a withdrawal or checkpoint with no entry before it
pub fn offset_of(prev: Option<Offset>, entry: &LogEntry) -> anyhow::Result<Offset> {
    match (entry, prev) {
        (LogEntry::Command(c), _) => Ok(Offset::start_of(c.sequenced.seq)),
        (_, Some(prev)) => Ok(prev.next()),
        (_, None) => bail!("log entry before the first command of its segment"),
    }
}

/// Signals an early end to a log read: `read_file` and `SegmentedLog::open` stop at the entry
/// whose visit returns it, as if the log ended there, and propagate it. A reader that needs only
/// a log's prefix (`engine exit-proof` at a requested checkpoint) stops at it, so a torn suffix
/// past the prefix does not block it; such a reader catches the error by this marker and treats
/// the read as complete.
pub(crate) struct Stop;

impl std::fmt::Debug for Stop {
    /// The marker's name.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Stop")
    }
}

impl std::fmt::Display for Stop {
    /// The marker's name.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("log read stopped at the caller's request")
    }
}

impl std::error::Error for Stop {}

/// Reads a single log file a line at a time, handing each entry to `visit` in order.
/// Error cases:
/// - the file does not exist or cannot be read
/// - a non-blank line is not a valid entry
/// - `visit` fails, propagating [`Stop`] for an early end
pub fn read_file(path: &Path, mut visit: impl FnMut(LogEntry) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let file = File::open(path).with_context(|| format!("opening log {}", path.display()))?;
    read_lines(BufReader::new(file), |n, line| match parse_line(n, line)? {
        Some(entry) => visit(entry),
        None => Ok(()),
    })
    .with_context(|| format!("log {}", path.display()))
}

/// Hands each line of `input` to `visit` with its line number, line break included where it has
/// one, holding one line in memory at a time.
/// Error cases:
/// - `input` cannot be read, or `visit` fails
fn read_lines(
    mut input: impl BufRead,
    mut visit: impl FnMut(usize, &[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let (mut line, mut n) = (Vec::new(), 0);
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).context("reading")? == 0 {
            return Ok(());
        }
        n += 1;
        visit(n, &line)?;
    }
}

/// The entry on line `n`, or `None` for a blank line.
/// Error cases:
/// - a non-blank line is not a valid entry
fn parse_line(n: usize, line: &[u8]) -> anyhow::Result<Option<LogEntry>> {
    if line.trim_ascii().is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(line).map(Some).with_context(|| format!("log line {n}"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod authenticated_tests;
