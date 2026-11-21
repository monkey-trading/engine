//! The daily snapshot a member's replica starts from: the newest, among those in its own directory of them and
//! those its origins list, whose checkpoint the copy holds and the vault has finalized, signed over its state by the
//! vault-recorded signer that signed that checkpoint (`open_signed`). Candidates are tried newest first, a fetched
//! one only once its file is whole, and one that is refused is passed over for the next, a fetched one's file
//! removed, since only its signature and root make it usable. One the vault has not yet finalized is not a
//! candidate, so the checkpoint the vault finalizes next, which the member's start anchors at, is one the replica
//! retains.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::ensure;
use engine_core::types::AccountId;
use tracing::{info, warn};

use crate::daily_snapshots::fetch::{download, offered};
use crate::daily_snapshots::writer::write_manifest;
use crate::daily_snapshots::{Listed, Signed, open_signed, read_manifest};
use crate::log::{self, CheckpointDomain, LogEntry, Offset, SegmentedLog, Stop};

/// How many of the newest candidates are tried before the replica replays the whole copy.
const ATTEMPTS: usize = 3;

/// Where a candidate snapshot is: in the member's own directory, or listed by an origin.
enum Source {
    Held,
    Offered(String),
}

/// The daily snapshot the replica of the copy in `copy` starts from, from the member's directory of them at `dir`
/// or fetched with `client` from `origins` into it and listed there once it passes: of a checkpoint at or before
/// seq `bound`, the vault's finalized checkpoint, that the copy holds at the snapshot's offset with the listed
/// root, and signed over its state by its checkpoint's signer, one of `signers`, in `domain`. Each candidate
/// refused, and no origin answering, is paged as Warning `snapshot_start_refused`; none passing gives none.
pub(crate) async fn snapshot_start(
    client: &reqwest::Client,
    origins: &[String],
    (dir, copy): (&Path, &Path),
    domain: CheckpointDomain,
    signers: &BTreeSet<AccountId>,
    bound: u64,
) -> Option<Signed> {
    let held = match read_manifest(dir) {
        Ok(held) => held,
        Err(e) => {
            warn!(alert = "snapshot_start_refused", "the daily snapshots this member holds cannot be listed: {e:#}");
            return None;
        }
    };
    let (offers, answered) = offered(client, origins, bound).await;
    if !answered && !origins.is_empty() {
        warn!(alert = "snapshot_start_refused", "no origin's daily snapshot manifest could be read");
    }
    // By offset, the member's own first, then the first origin to list each.
    let mut candidates: BTreeMap<(u64, u32), (Source, Listed)> = BTreeMap::new();
    for listing in held.iter().filter(|listing| listing.seq <= bound) {
        candidates.insert((listing.seq, listing.index), (Source::Held, listing.clone()));
    }
    for (origin, listing) in offers {
        candidates.entry((listing.seq, listing.index)).or_insert((Source::Offered(origin), listing));
    }
    for (source, listing) in candidates.into_values().rev().take(ATTEMPTS) {
        match candidate(client, (dir, copy), &source, &listing, (domain, signers)).await {
            Ok(signed) => {
                if matches!(source, Source::Offered(_)) {
                    let mut listed = held.clone();
                    listed.push(listing.clone());
                    listed.sort_by_key(|listing| (listing.seq, listing.index));
                    if let Err(e) = write_manifest(dir, &listed) {
                        warn!(
                            alert = "snapshot_start_refused",
                            seq = listing.seq,
                            "listing a fetched snapshot failed: {e:#}"
                        );
                    }
                    info!(
                        seq = listing.seq,
                        bytes = listing.bytes,
                        "fetched a daily snapshot for the replica to start from"
                    );
                }
                return Some(signed);
            }
            Err(e) => warn!(
                alert = "snapshot_start_refused",
                seq = listing.seq,
                "the replica does not start from daily snapshot {}: {e:#}",
                listing.file
            ),
        }
    }
    None
}

/// The snapshot `listing` names, from `source`, checked as `snapshot_start` checks it: an offered one fetched into
/// `dir` first, and its file removed when it is refused.
/// Error cases:
/// - the copy at `copy` holds no checkpoint at its offset with the listed root
/// - the fetch fails (`download`), or the snapshot is refused (`open_signed`)
async fn candidate(
    client: &reqwest::Client,
    (dir, copy): (&Path, &Path),
    source: &Source,
    listing: &Listed,
    (domain, signers): (CheckpointDomain, &BTreeSet<AccountId>),
) -> anyhow::Result<Signed> {
    let offset = Offset { seq: listing.seq, index: listing.index };
    let held = matches!(entry_at(copy, offset)?, Some(LogEntry::Checkpoint(held)) if held.root == listing.root);
    ensure!(held, "the copy holds no checkpoint at {offset} with the listed root");
    let path = match source {
        Source::Held => dir.join(&listing.file),
        Source::Offered(origin) => download(client, origin, dir, listing).await?,
    };
    open_signed(&path, domain, signers).inspect_err(|_| {
        if matches!(source, Source::Offered(_)) {
            let _ = std::fs::remove_file(&path);
        }
    })
}

/// The entry the log directory at `path` holds at `offset`, if it holds one.
/// Error cases:
/// - the log cannot be read up to `offset` (`SegmentedLog::read_span_ahead`)
pub(crate) fn entry_at(path: &Path, offset: Offset) -> anyhow::Result<Option<LogEntry>> {
    let (mut at, mut found) = (None, None);
    SegmentedLog::read_span_ahead(path, offset.seq, |entry| {
        let here = log::offset_of(at, &entry)?;
        at = Some(here);
        if here < offset {
            return Ok(());
        }
        found = (here == offset).then_some(entry);
        Err(Stop.into())
    })?;
    Ok(found)
}
