//! Reading the log from the archive's bucket: a reader goes to the bucket's public origin itself, never through a
//! node, and derives each key it needs from the seqs it needs, as range `k` holds seqs `k * RANGE_SEQS` on
//! (`range_key`). The bucket holds a prefix of the log, the ranges below a checkpoint the vault finalized, so a `404`
//! on a key a reader needs is the object missing (`Unread::Missing`), never the log's end: a cache in front of the
//! bucket's domain can answer a stale `404`. Nothing a reader takes from the bucket is trusted: a segment rebuilt
//! from its ranges must be the bytes its listing names (`standby::archive::check_listing`), and a copy built from
//! them replays, its seals, links and checkpoint roots checked, as any copy of the log does.

use std::io::{BufReader, Read};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail, ensure};
use engine_core::eip712::B256;
use reqwest::{Client, Url};
use tokio::task::spawn_blocking;

use crate::archive::{Object, RANGE_SEQS, log_prefix, next_entry, range_key, read_public, tail_prefix};
use crate::config::NodeConfig;

mod segments;
pub(crate) use segments::MAX_REBUILT_BYTES;
pub(crate) use segments::{ArchiveOrigins, Origins, Segments};
mod source;
#[cfg(test)]
pub(crate) use source::download;
pub use source::{LocalLog, PublishedLog};
mod tail;
pub(crate) use tail::read_through;

/// How long a reader waits for the bucket to answer a request, as a node fetch waits for an archive origin; half a
/// second in tests.
const ANSWERED_WITHIN: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(30) };
/// The most bytes one range holds outside tests: launch flow fills a range to about 5.5 MB, and a carried genesis,
/// every part of which can sit in one range, adds up to the largest carry file a start reads (`carry::MAX_FILE_BYTES`)
/// and well under a kibibyte of framing a command, which the 256 MiB beside it covers.
pub(crate) const RANGE_BYTES: u64 = (256 << 20) + crate::carry::MAX_FILE_BYTES;
/// The most bytes of a range's object a reader reads: as many as one range holds, which its lines never pass
/// compressed, though launch flow compresses a range to under a megabyte.
pub(crate) const MAX_OBJECT_BYTES: u64 = RANGE_BYTES;
/// The most bytes a reader decompresses from one range's object (`RANGE_BYTES`); a mebibyte in tests.
const MAX_RANGE_BYTES: u64 = if cfg!(test) { 1 << 20 } else { RANGE_BYTES };

/// Where a log's objects stand at the bucket's public origin: the directory of its range objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bucket {
    /// The URL of the log's key prefix, ending in `/`: each range's object is its file name joined to it.
    base: Url,
}

/// Why a reader could not take seqs from the bucket.
#[derive(Debug)]
pub(crate) enum Unread {
    /// The bucket answered `404` for range `index`'s object, which the seqs need: `archive_segment_missing` once no
    /// other source has them.
    Missing { index: u64, object: Url },
    /// The bucket could not be asked, or answered other than `200` or `404`.
    Unreachable(anyhow::Error),
    /// The bucket served an object that holds other than the seqs needed: too long, no zstd frame, other seqs, or
    /// other bytes than their listing names.
    Refused(anyhow::Error),
}

/// A reader of the log's ranges in a bucket, holding the last object it read, compressed, for the seqs after.
pub(crate) struct Ranges {
    bucket: Bucket,
    held: Option<(u64, Arc<Vec<u8>>)>,
}

impl Bucket {
    /// The bucket `cfg`'s `[archive]` reads the node's own log from: its public origin and the log's key prefix,
    /// its custody chain and vault. None without the section; a reader needs neither key.
    /// Error cases:
    /// - the custody vault does not parse, or the prefix does not join the origin
    pub(crate) fn for_config(cfg: &NodeConfig) -> anyhow::Result<Option<Self>> {
        let Some(section) = &cfg.archive else { return Ok(None) };
        let vault = cfg.custody.vault.parse().context("custody.vault")?;
        Self::new(&section.public_url, &log_prefix(cfg.custody.chain_id, vault)).map(Some)
    }

    /// The log under key `prefix` at the public origin `public`, as `Store` reads it back.
    /// Error cases:
    /// - the prefix does not join the origin
    pub(crate) fn new(public: &Url, prefix: &str) -> anyhow::Result<Self> {
        Ok(Self { base: public.join(prefix).with_context(|| format!("the public URL of {prefix}"))? })
    }

    /// The bucket a node's `308` for the segment starting at `first_seq` points at: `location` must be the object of
    /// the range holding that seq, over `http` or `https`, and the log's other objects stand beside it.
    /// Error cases:
    /// - `location` is not over `http` or `https`, or names another object than that range's
    pub(crate) fn redirected(location: &Url, first_seq: u64) -> anyhow::Result<Self> {
        ensure!(matches!(location.scheme(), "http" | "https"), "a redirect to {location} is not over http");
        let base = location.join("./").with_context(|| format!("the directory of {location}"))?;
        let bucket = Self { base };
        ensure!(
            bucket.object(first_seq / RANGE_SEQS)? == *location,
            "a redirect for seq {first_seq} names {location}, not the object of the range holding it"
        );
        Ok(bucket)
    }

    /// The tail of the checkpoint at `seq` rooted at `root`, in this log's bucket: its pieces stand under it as
    /// ranges do under the log (`tail_prefix`).
    /// Error cases:
    /// - the tail's prefix does not join the log's
    pub(crate) fn tail(&self, seq: u64, root: &B256) -> anyhow::Result<Self> {
        let prefix = tail_prefix("", seq, root);
        Ok(Self { base: self.base.join(&prefix).with_context(|| format!("{prefix} under {}", self.base))? })
    }

    /// The URL of range `index`'s object.
    /// Error cases:
    /// - the file name does not join the base
    pub(crate) fn object(&self, index: u64) -> anyhow::Result<Url> {
        let file = range_key("", index);
        self.base.join(&file).with_context(|| format!("the URL of {file} under {}", self.base))
    }

    /// Range `index`'s object, for a message: its URL, or its file name.
    fn object_name(&self, index: u64) -> String {
        self.object(index).map_or_else(|_| range_key("", index), |url| url.to_string())
    }
}

impl std::fmt::Display for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.base.fmt(f)
    }
}

impl Ranges {
    /// A reader of `bucket`'s ranges, holding none yet.
    pub(crate) fn new(bucket: Bucket) -> Self {
        Self { bucket, held: None }
    }

    /// The bucket read.
    pub(crate) fn bucket(&self) -> &Bucket {
        &self.bucket
    }

    /// The lines of seqs `first..=last`, in log order, read over `http` from the objects of the ranges holding
    /// them: each command's line and the lines after it up to the next command. At most `limit` bytes are taken.
    /// Error cases:
    /// - the bucket answers `404` for one of the objects (`Missing`), or cannot be asked (`Unreachable`)
    /// - an object is longer than `MAX_OBJECT_BYTES` or does not decompress within `MAX_RANGE_BYTES`, holds a seq
    ///   other than the next in its range or ends before the seqs needed, or the lines come to more than `limit`
    ///   bytes (`Refused`)
    pub(crate) async fn seqs(
        &mut self,
        http: &Client,
        (first, last): (u64, u64),
        limit: u64,
    ) -> Result<Vec<u8>, Unread> {
        if first > last {
            return Err(Unread::Refused(anyhow!("seqs {first} to {last} are no range")));
        }
        let mut lines = Vec::new();
        for index in first / RANGE_SEQS..=last / RANGE_SEQS {
            let object = self.object(http, index).await?;
            // The listing that names the seqs is untrusted: the range's end past `u64::MAX` is its last seq.
            let end =
                index.checked_add(1).and_then(|next| next.checked_mul(RANGE_SEQS)).map_or(u64::MAX, |next| next - 1);
            let wanted = (first.max(index * RANGE_SEQS), last.min(end));
            let room = limit.saturating_sub(lines.len() as u64);
            let taken = spawn_blocking(move || lines_of(&object, index, wanted, room))
                .await
                .map_err(|e| Unread::Unreachable(anyhow!("reading a range stopped: {e}")))?
                .with_context(|| {
                    let object = self.bucket.object_name(index);
                    format!("the archive's object {object}, read for seqs {first} to {last} in at most {limit} bytes")
                })
                .map_err(Unread::Refused)?;
            lines.extend_from_slice(&taken);
        }
        Ok(lines)
    }

    /// Range `index`'s object, compressed: the one held, or read from the bucket and held.
    /// Error cases:
    /// - as `seqs`
    async fn object(&mut self, http: &Client, index: u64) -> Result<Arc<Vec<u8>>, Unread> {
        if let Some((held, object)) = &self.held
            && *held == index
        {
            return Ok(object.clone());
        }
        let object = self.bucket.object(index).map_err(Unread::Refused)?;
        let read = read_public(http, object.clone(), MAX_OBJECT_BYTES, ANSWERED_WITHIN).await;
        let object = match read.map_err(Unread::Unreachable)? {
            Object::Absent => return Err(Unread::Missing { index, object }),
            Object::Oversized => {
                return Err(Unread::Refused(anyhow!("{object} is longer than {MAX_OBJECT_BYTES} bytes")));
            }
            Object::Read(object) => Arc::new(object),
        };
        self.held = Some((index, object.clone()));
        Ok(object)
    }
}

impl Unread {
    /// The failure as an error, naming the object a `Missing` lacks.
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Missing { object, .. } => anyhow!("the archive's bucket has no object at {object}"),
            Self::Unreachable(e) | Self::Refused(e) => e,
        }
    }
}

/// The lines of seqs `first..=last` of range `index`'s compressed `object`, decompressed a line at a time and
/// checked to hold each seq of the range in order from its first, at most `limit` bytes of them.
/// Error cases:
/// - the object is no zstd frame, decompresses past `MAX_RANGE_BYTES`, or holds a line that is not an entry
/// - its first line is not its first seq's command, a command is not the next seq, or it ends before `last`
/// - the lines come to more than `limit` bytes
fn lines_of(object: &[u8], index: u64, (first, last): (u64, u64), limit: u64) -> anyhow::Result<Vec<u8>> {
    let decoder = zstd::stream::read::Decoder::with_buffer(object).context("reading a zstd frame")?;
    let mut reader = BufReader::new(decoder.take(MAX_RANGE_BYTES + 1));
    let (mut taken, mut line, mut read) = (Vec::new(), Vec::new(), 0u64);
    let mut seq: Option<u64> = None;
    let mut inflated = |n: usize| {
        read += n as u64;
        ensure!(read <= MAX_RANGE_BYTES, "it decompresses past {MAX_RANGE_BYTES} bytes");
        Ok(())
    };
    while let Some(head) = next_entry(&mut reader, &mut line, &mut inflated).context("decompressing the zstd frame")? {
        if head.entry == "command" {
            let next = seq
                .map_or(Some(index * RANGE_SEQS), |seq| seq.checked_add(1))
                .context("it holds seqs past the last")?;
            let named = head.seq.context("a command names no seq")?;
            ensure!(named == next, "it holds seq {named} where seq {next} belongs");
            seq = Some(named);
        }
        let Some(at) = seq else { bail!("it starts with an entry that is not a command") };
        if at > last {
            return Ok(taken);
        }
        if at >= first {
            ensure!(
                (taken.len() + line.len()) as u64 <= limit,
                "its lines for seqs {first} to {last} pass {limit} bytes"
            );
            taken.extend_from_slice(&line);
        }
    }
    let ended = seq.map_or_else(|| "with no command".to_owned(), |seq| format!("at seq {seq}"));
    ensure!(seq.is_some_and(|seq| seq >= last), "it ends {ended}, before seq {last}");
    Ok(taken)
}

#[cfg(test)]
pub(crate) mod origin;
#[cfg(test)]
mod scenario;
#[cfg(test)]
mod tests;
