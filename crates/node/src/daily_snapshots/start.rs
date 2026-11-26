//! The daily snapshot a start takes: the newest, among those in the node's own directory of them whose file its host
//! holds and, for a member whose copy is new, those its origins list, from their disk or the archive's bucket, whose checkpoint the log on this host holds and the vault
//! has finalized, of the format that carries the replay state, signed over its state and what it carries by
//! the vault-recorded signer that signed that checkpoint (`open_signed`). Offsets are tried newest first, and at
//! each the sources that list it in turn: the node's own directory, then each origin's in order. A fetched one
//! is used only once its file is whole, and one that is refused is passed over, a fetched one's file removed,
//! since only its signature and root make it usable. A snapshot of a format before that is passed over without
//! a warning: it carries no replay state, and a replay in ranges still reads it. One the vault has not yet
//! finalized is not a candidate, so the checkpoint the vault finalizes next, which a start anchors at, is one the
//! start replays; nor, for a member, is one from the turns of the member in office, whose commands the member's
//! own log check at its first take must have replayed (`log_check::own_check`); nor one after a seq the start
//! must keep (`keep`). Where the vault accepted a checkpoint at a candidate's seq, the candidate's root must be
//! the one it accepted, so an orphan fork's snapshot is refused before it is fetched or decoded.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::types::{AccountId, Millis};
use tracing::{debug, info, warn};

use crate::daily_snapshots::fetch::{EarlierFormat, Takes, download, offered};
use crate::daily_snapshots::writer::write_manifest;
use crate::daily_snapshots::{
    FORMAT_BYTES, Listed, Signed, SnapshotBucket, earlier_format, open_signed, read_manifest,
};
use crate::log::{self, CheckpointDomain, LogEntry, Offset, SegmentedLog, Stop};

/// How many of the newest offsets are tried, each from every source that lists it, before the start replays the
/// whole log.
const ATTEMPTS: usize = 3;

/// The roots of the checkpoints the vault accepted, as a start reads them, and the key it names at a seq.
pub(crate) trait AcceptedRoots {
    /// The root of the checkpoint the vault accepted at `seq`; none where it accepted none there, or the read
    /// fails, which leaves the candidate to the checks after it.
    async fn root_at(&self, seq: u64) -> Option<B256>;

    /// The key the checkpoint at `seq`, and so its snapshot, must be signed by: the signer of the checkpoint the vault
    /// accepted there, or, where it accepted none, of the one it finalized, which the snapshot rests on; none where the
    /// start reads no vault, which leaves the snapshot to the signers it is given.
    async fn signer_at(&self, _seq: u64) -> Option<AccountId> {
        None
    }
}

/// No vault read: each candidate rests on the checks after it.
pub(crate) struct NoRoots;

impl AcceptedRoots for NoRoots {
    /// None.
    async fn root_at(&self, _seq: u64) -> Option<B256> {
        None
    }
}

/// Where a candidate snapshot is: in the member's own directory, or listed by an origin.
enum Source {
    Held,
    Offered(String),
}

/// Where a start fetches the snapshots its origins list: with `client`, from `origins`, following an origin's
/// redirect to the archive's bucket only to `bucket`, the one the node's own `[archive]` names, where it names one.
#[derive(Clone, Copy)]
pub(crate) struct Fetching<'a> {
    pub client: &'a reqwest::Client,
    pub origins: &'a [String],
    pub bucket: Option<&'a SnapshotBucket>,
}

/// Where a start looks for its snapshot, and what bounds it.
#[derive(Clone, Copy)]
pub(crate) struct Bounds {
    /// The vault's finalized checkpoint's seq: no snapshot after it is a candidate.
    pub finalized: u64,
    /// When the turns of the member in office began, for a member's start: no snapshot stamped then or after.
    pub before: Option<Millis>,
    /// The first seq the start must replay, as a promotion's fixed head: no snapshot at it or after.
    pub keep_from: Option<u64>,
}

/// The daily snapshot the replica of the copy in `copy` starts from, from the member's directory of them at `dir`
/// or fetched with `client` from `origins` into it and listed there once it passes: of a checkpoint at or before
/// seq `bound`, the vault's finalized checkpoint, and stamped before `before`, when the turns of the member in
/// office began, that the copy holds at the snapshot's offset, and signed over its state by its checkpoint's
/// signer, one of `signers`, in `domain`. Each candidate refused, and no origin answering, is paged as Warning
/// `snapshot_start_refused`; none passing gives none.
#[cfg(test)]
pub(crate) async fn snapshot_start(
    client: &reqwest::Client,
    origins: &[String],
    (dir, copy): (&Path, &Path),
    domain: CheckpointDomain,
    signers: &BTreeSet<AccountId>,
    (finalized, before): (u64, Option<Millis>),
) -> Option<Signed> {
    let bounds = Bounds { finalized, before, keep_from: None };
    let fetching = Fetching { client, origins, bucket: None };
    snapshot_start_against(Some(fetching), (dir, copy), (domain, signers), bounds, &NoRoots).await
}

/// `snapshot_start`, from the node's own directory alone when `fetching` names no origins, refusing a candidate
/// whose root is not the one the vault accepted at its seq, where `roots` names one, before fetching or decoding
/// it, and passing over one after `bounds.keep_from`. A snapshot the directory lists whose file its host no longer
/// holds, which the bucket keeps, is a candidate only where an origin lists it.
pub(crate) async fn snapshot_start_against(
    fetching: Option<Fetching<'_>>,
    (dir, copy): (&Path, &Path),
    (domain, signers): (CheckpointDomain, &BTreeSet<AccountId>),
    bounds: Bounds,
    roots: &impl AcceptedRoots,
) -> Option<Signed> {
    let held = match read_manifest(dir) {
        Ok(held) => held,
        Err(e) => {
            warn!(alert = "snapshot_start_refused", "the daily snapshots this node holds cannot be listed: {e:#}");
            return None;
        }
    };
    let (offers, answered) = match fetching {
        Some(Fetching { client, origins, .. }) if !origins.is_empty() => {
            offered(client, origins, bounds.finalized).await
        }
        _ => (Vec::new(), true),
    };
    if !answered {
        warn!(alert = "snapshot_start_refused", "no origin's daily snapshot manifest could be read");
    }
    // By offset, the node's own first, then each origin that lists it in order, each giving way to the next.
    let fits = |listing: &Listed| {
        listing.seq <= bounds.finalized
            && bounds.before.is_none_or(|before| listing.ts < before)
            && bounds.keep_from.is_none_or(|keep| listing.seq < keep)
    };
    let mut candidates: BTreeMap<(u64, u32), Vec<(Source, Listed)>> = BTreeMap::new();
    for listing in held.iter().filter(|listing| fits(listing) && carries_state(&dir.join(&listing.file)) == Some(true))
    {
        candidates.entry((listing.seq, listing.index)).or_default().push((Source::Held, listing.clone()));
    }
    for (origin, listing) in offers.into_iter().filter(|(_, listing)| fits(listing)) {
        candidates.entry((listing.seq, listing.index)).or_default().push((Source::Offered(origin), listing));
    }
    let ids = (domain, signers, bounds.before);
    for (source, listing) in candidates.into_values().rev().take(ATTEMPTS).flatten() {
        match candidate(fetching, (dir, copy), &source, &listing, ids, roots).await {
            Ok(signed) => {
                if matches!(source, Source::Offered(_)) {
                    let offset = (listing.seq, listing.index);
                    let mut listed: Vec<Listed> =
                        held.iter().filter(|held| (held.seq, held.index) != offset).cloned().collect();
                    listed.push(listing.clone());
                    listed.sort_by_key(|listing| (listing.seq, listing.index));
                    if let Err(e) = write_manifest(dir, &listed) {
                        warn!(
                            alert = "snapshot_start_refused",
                            seq = listing.seq,
                            "listing a fetched snapshot failed: {e:#}"
                        );
                    }
                    info!(seq = listing.seq, bytes = listing.bytes, "fetched a daily snapshot to start from");
                }
                return Some(signed);
            }
            // As one this node holds, a snapshot of an earlier format carries no replay state, and is passed over.
            Err(e) if e.is::<EarlierFormat>() => {
                debug!(seq = listing.seq, "daily snapshot {} offered: {e:#}", listing.file);
            }
            Err(e) => warn!(
                alert = "snapshot_start_refused",
                seq = listing.seq,
                "the start does not take daily snapshot {}: {e:#}",
                listing.file
            ),
        }
    }
    None
}

/// The seqs of the newest daily snapshots in `dir` at or before `finalized` that a start from them without origins
/// tries (`snapshot_start_against`), newest first.
pub(super) fn candidate_seqs(dir: &Path, finalized: u64) -> Vec<u64> {
    let held = read_manifest(dir).unwrap_or_default();
    let fits = |listing: &&Listed| listing.seq <= finalized && carries_state(&dir.join(&listing.file)) == Some(true);
    let seqs: BTreeSet<u64> = held.iter().filter(fits).map(|listing| listing.seq).collect();
    seqs.into_iter().rev().take(ATTEMPTS).collect()
}

/// Whether the held snapshot at `path` may carry the replay state: its first bytes, where every writer puts its
/// format, name no earlier format (`earlier_format`); one that cannot be read, or names none, is left for its
/// check to refuse. None when the host no longer holds the file, as once the bucket keeps it.
fn carries_state(path: &Path) -> Option<bool> {
    let mut head = [0u8; FORMAT_BYTES];
    let read = match File::open(path) {
        Ok(mut file) => file.read(&mut head),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => Err(e),
    };
    let Ok(read) = read else { return Some(true) };
    let carries = !earlier_format(&head[..read]);
    if !carries {
        debug!(snapshot = %path.display(), "a daily snapshot of an earlier format carries no replay state for a start");
    }
    Some(carries)
}

/// The snapshot `listing` names, from `source`, checked as `snapshot_start` checks it: an offered one fetched into
/// `dir` first, and its file removed when it is refused.
/// Error cases:
/// - the copy at `copy` holds no checkpoint at its offset with the listed root
/// - `roots` names another root at its seq, or the checkpoint's signature does not verify, or it is signed by another
///   key than the one `roots` names at its seq
/// - the fetch fails (`download`), or the snapshot is refused (`open_signed`)
/// - its checkpoint is not the one the copy holds, or its state is stamped at or after `before`
async fn candidate(
    fetching: Option<Fetching<'_>>,
    (dir, copy): (&Path, &Path),
    source: &Source,
    listing: &Listed,
    (domain, signers, before): (CheckpointDomain, &BTreeSet<AccountId>, Option<Millis>),
    roots: &impl AcceptedRoots,
) -> anyhow::Result<Signed> {
    let offset = Offset { seq: listing.seq, index: listing.index };
    let Some(LogEntry::Checkpoint(held)) = entry_at(copy, offset)?.filter(|held| match held {
        LogEntry::Checkpoint(held) => held.root == listing.root,
        _ => false,
    }) else {
        anyhow::bail!("the copy holds no checkpoint at {offset} with the listed root");
    };
    if let Some(accepted) = roots.root_at(listing.seq).await {
        let listed: B256 = listing.root.parse().context("the listed root")?;
        ensure!(
            listed == accepted,
            "the vault accepted root {accepted} at seq {}, not the listed {listed}",
            listing.seq
        );
    }
    // Only the key the vault names at the checkpoint's seq vouches for the state a snapshot of it carries, and the
    // snapshot is signed by the key its checkpoint names (`open_signed`): that checkpoint must be one it signed.
    crate::checkpoints::signature::verify_domain(&held, domain).context("the checkpoint the copy holds there")?;
    if let Some(required) = roots.signer_at(listing.seq).await {
        let named: AccountId = held.signer.parse().context("the checkpoint's signer")?;
        ensure!(
            named == required,
            "its checkpoint is signed by {named}, not {required}, the vault's signer at its seq"
        );
    }
    let path = match source {
        Source::Held => dir.join(&listing.file),
        Source::Offered(origin) => {
            let fetching = fetching.context("an offered snapshot needs a client")?;
            let takes = Takes { bucket: fetching.bucket, this_format: true };
            download(fetching.client, origin, dir, listing, takes).await?
        }
    };
    let checked = open_signed(&path, domain, signers).and_then(|signed| {
        ensure!(signed.snapshot.checkpoint == held, "its checkpoint is not the one the copy holds at {offset}");
        let stamped = signed.snapshot.engine.now();
        ensure!(
            before.is_none_or(|before| stamped < before),
            "its state, stamped {stamped}, is from the turns of the member in office, which began at {before:?}"
        );
        Ok(signed)
    });
    checked.inspect_err(|_| {
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
