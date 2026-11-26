//! A follower's catch-up from the archive. An operator that started from a daily snapshot serves its signed stream
//! only from after the snapshot's checkpoint (`log::Manifest::verified_from`), so a member or standby whose copy
//! trails it is refused a stream from its head (`BeforeServed`). It then fetches the completed segments the first
//! archive origin that lists any past its head lists, through the readers a cold start uses (`archive::fetch_manifest`
//! and `crate::archive::Segments`, the bucket first, each segment checked against its listing), takes each entry after
//! the head as a stream's entry is taken, its seal and link checked against the copy (`Chain::check`) before the copy
//! appends it, and resumes the stream from its new head.

use std::collections::BTreeSet;

use anyhow::Context;
use engine_core::types::AccountId;

use crate::archive::{ArchiveOrigins, Origins, Segments};
use crate::log::{CheckpointDomain, LogEntry, LogWriter, Offset, offset_of};
use crate::standby::archive::{fetch_manifest, validate_listing};
use crate::standby::chain::{Chain, Step};

/// Marks a stream the operator refused because the copy resumes before the first seq its start verified, with the
/// refusal it gave: the archive holds the entries between.
#[derive(Debug)]
pub(crate) struct BeforeServed(pub(crate) String);

impl std::fmt::Display for BeforeServed {
    /// Says the operator serves its stream only from a later seq, with its refusal.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the operator serves its stream only from after the copy's head: {}", self.0)
    }
}

impl std::error::Error for BeforeServed {}

/// Whether `error` is the operator's refusal of a stream from before what its start verified (`BeforeServed`).
pub(crate) fn before_served(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<BeforeServed>())
}

/// Hands `take`, segment by segment, the entries the completed segments listed past `head`, the copy's last entry,
/// hold after it, as JSON lines with the offset they follow, from the first of `origins` whose manifest lists one, so
/// a mirror that stopped at or before the copy's head gives way to the next; each segment is fetched from the bucket
/// `origins` names, when it names one, or that origin and then the others in turn, and checked against its listing as a cold start's backfill
/// fetches it; `take` checks and appends them and gives
/// the copy's new head. The copy's head once every listed segment is taken.
/// Error cases:
/// - no origin's manifest can be fetched, a listing is not canonical, a segment differs from its listing, or a line
///   does not parse
/// - `take` refuses a segment's entries (a `Rollback` for one that does not extend the copy)
/// - no origin lists anything past the copy's head, so the stream would be refused again
pub(crate) async fn catch_up(
    archive: &reqwest::Client,
    origins: &(impl ArchiveOrigins + ?Sized),
    mut head: Option<Offset>,
    mut take: impl FnMut(Option<Offset>, &[u8]) -> anyhow::Result<Option<Offset>>,
) -> anyhow::Result<Option<Offset>> {
    let from = head;
    let past = |listed: &crate::log::SegmentInfo| from.is_none_or(|from| listed.last_seq > from.seq);
    let mut failed = None;
    let mut found = None;
    for origin in origins.nodes() {
        let origin = std::slice::from_ref(origin);
        match fetch_manifest(archive, origin).await {
            Ok(listings) if listings.iter().any(past) => {
                found = Some((origin, listings));
                break;
            }
            Ok(_) => {}
            Err(e) => failed = Some(e),
        }
    }
    let Some((origin, listings)) = found else {
        return Err(match failed {
            Some(e) => e.context("fetching the published manifest"),
            None => anyhow::anyhow!("the archive lists nothing past the copy's head, {from:?}"),
        });
    };
    // The chosen origin first, then the others in turn, so a segment it lists and cannot serve comes from another.
    let others = origins.nodes().iter().filter(|other| !origin.contains(*other)).cloned();
    let nodes = origin.iter().cloned().chain(others).collect();
    let reading = Origins { bucket: origins.bucket().cloned(), nodes };
    let mut segments = Segments::new(archive, &reading);
    for listed in listings.iter().filter(|listed| from.is_none_or(|from| listed.last_seq >= from.seq)) {
        validate_listing(listed)?;
        let bytes = segments.segment(listed).await?;
        let (mut at, mut after) = (None, Vec::new());
        for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let entry: LogEntry = serde_json::from_slice(line).context("an archive line is not a log entry")?;
            at = Some(offset_of(at, &entry)?);
            if head.is_none_or(|head| at > Some(head)) {
                after.extend_from_slice(line);
                after.push(b'\n');
            }
        }
        if !after.is_empty() {
            head = take(head, &after).with_context(|| format!("archive segment {}", listed.file))?;
        }
    }
    anyhow::ensure!(head != from, "the archive lists nothing past the copy's head, {from:?}");
    Ok(head)
}

/// Appends to a standby's copy, open in `writer` with its chain `copied`, the entries `bytes` holds after the copy's
/// entry at `after`, each checked as a stream's entry is (`Chain::check`), every one of `signers`, the vault's recorded
/// signers, admitted to seal them, and, as no frame signature vouches for an archive's bytes, each checkpoint and
/// withdrawal entry signed by one of `signers` in `domain`, before any is appended; syncs the copy and gives its new
/// head.
/// Error cases:
/// - a line does not parse, an entry does not extend the copy (a `Rollback`), a checkpoint or withdrawal entry is not
///   signed by one of `signers`, or the copy cannot be written or synced
pub(super) fn take_into(
    (writer, copied): (&mut LogWriter, &mut Chain),
    (domain, signers): (CheckpointDomain, &BTreeSet<AccountId>),
    after: Option<Offset>,
    bytes: &[u8],
) -> anyhow::Result<Option<Offset>> {
    let mut previous = after;
    let mut entries = Vec::new();
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let entry: LogEntry = serde_json::from_slice(line).context("an archive line is not a log entry")?;
        let offset = offset_of(previous, &entry)?;
        previous = Some(offset);
        signed_by(&entry, domain, signers).with_context(|| format!("the archive's entry at {offset}"))?;
        entries.push((offset, entry));
    }
    if let Some(after) = after {
        copied.begin_after(after);
    }
    // A signer the copy holds no checkpoint from may seal what the archive holds: a key a promotion moved to seals
    // commands before its first checkpoint.
    signers.iter().for_each(|signer| copied.admit(*signer));
    for (offset, entry) in entries {
        if let Step::Extends(link) = copied.check(offset, &entry)? {
            writer.append(&entry).context("appending an archive entry to the copy")?;
            copied.extend(offset, entry, link);
        }
    }
    writer.sync().context("syncing the copy")?;
    Ok(copied.head())
}

/// Whether `entry`, if a checkpoint or a withdrawal, is signed by one of `signers` in `domain`; a command's seal is
/// the chain's to check.
/// Error cases:
/// - a checkpoint's signature does not verify or is by a key `signers` does not hold, or a withdrawal's by none of them
fn signed_by(entry: &LogEntry, domain: CheckpointDomain, signers: &BTreeSet<AccountId>) -> anyhow::Result<()> {
    match entry {
        LogEntry::Command(_) => Ok(()),
        LogEntry::Checkpoint(checkpoint) => {
            crate::checkpoints::signature::verify_domain(checkpoint, domain)?;
            let signer: AccountId = checkpoint.signer.parse().context("the checkpoint's signer")?;
            anyhow::ensure!(
                signers.contains(&signer),
                "the checkpoint is signed by {signer}, which the vault never named"
            );
            Ok(())
        }
        LogEntry::Withdrawal(withdrawal) => {
            let verifies = |signer: &AccountId| {
                crate::checkpoints::signature::verify_withdrawal(withdrawal, Some(domain), Some(*signer)).is_ok()
            };
            anyhow::ensure!(signers.iter().any(verifies), "the withdrawal is signed by no key the vault named");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
