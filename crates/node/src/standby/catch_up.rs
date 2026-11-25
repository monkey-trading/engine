//! A follower's catch-up from the archive. An operator that started from a daily snapshot serves its signed stream
//! only from after the snapshot's checkpoint (`log::Manifest::verified_from`), so a member or standby whose copy
//! trails it is refused a stream from its head (`BeforeServed`). It then fetches the completed segments the archive
//! origins list past its head, through the readers a cold start uses (`archive::fetch_manifest`,
//! `archive::fetch_archive_bytes`, each segment checked against its listing), takes each entry after the head as a
//! stream's entry is taken, its seal and link checked against the copy (`Chain::check`) before the copy appends it,
//! and resumes the stream from its new head.

use anyhow::Context;

use crate::log::{LogEntry, LogWriter, Offset, offset_of};
use crate::standby::archive::{fetch_archive_bytes, fetch_manifest, validate_listing};
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

/// Hands `take`, segment by segment, the entries the completed segments `origins` list past `head`, the copy's last
/// entry, hold after it, as JSON lines with the offset they follow, each segment fetched and checked against its
/// listing as a cold start's backfill fetches it; `take` checks and appends them and gives the copy's new head. The
/// copy's head once every listed segment is taken.
/// Error cases:
/// - the manifest or a segment cannot be fetched, a listing is not canonical, a segment differs from its listing,
///   or a line does not parse
/// - `take` refuses a segment's entries (a `Rollback` for one that does not extend the copy)
/// - the archive lists nothing past the copy's head, so the stream would be refused again
pub(crate) async fn catch_up(
    archive: &reqwest::Client,
    origins: &[String],
    mut head: Option<Offset>,
    mut take: impl FnMut(Option<Offset>, &[u8]) -> anyhow::Result<Option<Offset>>,
) -> anyhow::Result<Option<Offset>> {
    let (listings, from) = (fetch_manifest(archive, origins).await.context("fetching the published manifest")?, head);
    for listed in listings.iter().filter(|listed| head.is_none_or(|head| listed.last_seq >= head.seq)) {
        validate_listing(listed)?;
        let bytes = fetch_archive_bytes(archive, origins, &listed.file, Some(listed)).await?;
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
/// entry at `after`, each checked as a stream's entry is (`Chain::check`) before it is appended; syncs the copy and
/// gives its new head.
/// Error cases:
/// - a line does not parse, an entry does not extend the copy (a `Rollback`), or the copy cannot be written or synced
pub(super) fn take_into(
    writer: &mut LogWriter,
    copied: &mut Chain,
    after: Option<Offset>,
    bytes: &[u8],
) -> anyhow::Result<Option<Offset>> {
    if let Some(after) = after {
        copied.begin_after(after);
    }
    let mut previous = after;
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let entry: LogEntry = serde_json::from_slice(line).context("an archive line is not a log entry")?;
        let offset = offset_of(previous, &entry)?;
        previous = Some(offset);
        if let Step::Extends(link) = copied.check(offset, &entry)? {
            writer.append(&entry).context("appending an archive entry to the copy")?;
            copied.extend(offset, entry, link);
        }
    }
    writer.sync().context("syncing the copy")?;
    Ok(copied.head())
}

#[cfg(test)]
mod tests;
