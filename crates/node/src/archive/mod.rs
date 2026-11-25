//! The log's archive in object storage (`[archive]`): a bucket that keeps the whole log, so it stays public and
//! replayable from genesis whatever the host keeps.
//!
//! The bucket holds the log in ranges of `RANGE_SEQS` sequence numbers, each object the lines of every entry whose
//! seq falls in its range, in log order, compressed with zstd, at `log/<chain id>/<vault>/<first seq>.jsonl.zst`:
//! a new log, which takes a new vault, never meets an earlier log's objects. The ranges do not
//! follow any node's segment files, which each writer cuts for itself (a handover seals the predecessor's open
//! segment early, and nobody else's), so every node builds the same bytes for a range from its own copy. Range `k`
//! starts at `k * RANGE_SEQS`, so the keys need no manifest to list them. Every object is created once and never
//! replaced: uploads are create-only, and the bucket's lock refuses overwrites and deletes.
//!
//! A range goes up only once its last seq is below a finalized checkpoint of the vault's that the copy holds at the
//! vault's root: no takeover, promotion or rollback rewrites the vault's history below it, and a copy that holds
//! another history past an earlier checkpoint, as a returning operator's does, uploads none of it. The uploader (`uploader`) runs for the node's life beside whichever log writer
//! is in use, the serving sequencer's or a following member's copy's, whose published manifest is the authority on
//! what each segment holds. It builds a range only from segments checked against that listing, and counts the range
//! as archived only once a read of its object from the bucket's public origin, over TLS that ends in the enclave,
//! decompresses to those entries. A store or network outage leaves ranges unconfirmed and raises
//! `archive_upload_lagging`; the writer and the sequencer never wait on it.

mod backlog;
mod finality;
pub(crate) use finality::{Finality, same_root};
mod range;
pub(crate) use range::{RANGE_SEQS, checkpoint_root, log_prefix, range_bytes, range_key, whole_ranges};
mod store;
pub(crate) use store::{Created, Object, Store};
mod uploader;
pub(crate) use uploader::{Sources, Uploader};

#[cfg(test)]
pub(crate) mod stand_in;

use anyhow::Context;

use crate::log::LogEntry;
use crate::standby::chain::spelt;

/// The zstd level: on launch flow it compressed 1,000 and 10,000 commands of the log about 6.1 times at over a
/// gigabyte a second, ahead of every level up to 15, and 19 gained a tenth more at a hundredth of the speed.
const LEVEL: i32 = 1;
/// A compressed range's media type.
pub(crate) const ZSTD: &str = "application/zstd";

/// `raw` compressed into one zstd frame that records its content's checksum.
/// Error cases:
/// - libzstd refuses the input
pub(crate) fn compress(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::new(LEVEL).context("starting libzstd")?;
    compressor
        .set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
        .context("asking libzstd for a checksum")?;
    compressor.compress(raw).context("compressing a range")
}

/// The most bytes `raw`'s compressed frame can take.
pub(crate) fn stored_bound(raw: &[u8]) -> u64 {
    zstd::zstd_safe::compress_bound(raw.len()) as u64
}

/// Whether `stored`, an object read from the bucket, holds `raw`, the node's copy of a range: the same bytes, or
/// the same entries as every copy of one history reads them, their hex signatures and signer spelt as their writer
/// spells them and a checkpoint's deployment left out (`standby::chain::spelt`), as a copy backfilled from another
/// archive may spell them. Decompression stops at twice `raw`'s length.
pub(crate) fn holds(stored: &[u8], raw: &[u8]) -> bool {
    let Ok(decompressed) = zstd::bulk::decompress(stored, raw.len().saturating_mul(2)) else { return false };
    decompressed == raw || same_entries(&decompressed, raw)
}

/// Whether `a` and `b` hold the same entries in the same order, each as `spelt` reads it.
fn same_entries(a: &[u8], b: &[u8]) -> bool {
    let entries = |bytes: &[u8]| {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.trim_ascii().is_empty())
            .map(|line| serde_json::from_slice::<LogEntry>(line).map(|entry| spelt(&entry)))
            .collect::<Result<Vec<_>, _>>()
    };
    matches!((entries(a), entries(b)), (Ok(a), Ok(b)) if a == b)
}

#[cfg(test)]
mod scenario;
#[cfg(test)]
mod tests;
