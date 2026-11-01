//! The standby's archive reads: the cold-start bootstrap, the takeover's refresh, and the
//! bounded, validated fetches behind both.

use std::path::Path;

use anyhow::{Context, ensure};
use reqwest::StatusCode;
use tokio::time::timeout;
use tracing::{info, warn};

use super::{BODY_TIMEOUT, FETCH_TIMEOUT, MAX_ARCHIVE_BYTES, MAX_ARCHIVE_LISTINGS, sha256};
use crate::config::StandbySection;
use crate::log::{MANIFEST, SegmentInfo};

/// A cold start: fetches the published manifest and every completed segment it lists into the
/// empty copy, checking each file against its listing, and writes the manifest. A missing
/// manifest (a fresh deployment) leaves the copy empty for the stream to fill from seq 0.
/// Error cases:
/// - a fetch fails, exceeds the bound, or a file differs from its listing
pub(super) async fn bootstrap_archive(
    archive: &reqwest::Client,
    standby: &StandbySection,
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listings = match fetch_manifest(archive, standby).await {
        Ok(listings) => listings,
        Err(e) => match e.downcast::<reqwest::Error>() {
            Ok(reqwest) if reqwest.status() == Some(reqwest::StatusCode::NOT_FOUND) => return Ok(()),
            Ok(reqwest) => return Err(reqwest).context("fetching the published manifest"),
            Err(e) => return Err(e).context("fetching the published manifest"),
        },
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut text = String::new();
    for listed in &listings {
        let bytes = fetch_archive_bytes(archive, standby, &listed.file).await?;
        check_listing(listed, &bytes)?;
        // Stage under a name `SegmentedLog::open` ignores, then link into place: a crash leaves
        // either the old or the new whole file, never a torn one.
        let staged = dir.join(format!("{}.staging", listed.file));
        std::fs::write(&staged, bytes).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, dir.join(&listed.file)).with_context(|| format!("linking {}", listed.file))?;
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
    }
    let staged = dir.join(format!("{MANIFEST}.staging"));
    std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, dir.join(MANIFEST)).with_context(|| format!("linking {MANIFEST}"))?;
    info!(segments = listings.len(), "backfilled the copy from the operator's archive");
    Ok(())
}

/// Best-effort refresh of completed segments the copy lacks, run once a takeover is promoted:
/// the checkpoint the vault accepted last may post after the stream last delivered. Existing
/// files are never overwritten: a local file with the segment's name is this follower's own
/// frame-verified bytes, which wins over anything an archive serves. New segments are staged and
/// linked before the manifest gains their lines, and the manifest keeps its local lines, so a
/// crash leaves at most unlisted trailing files, which open as the open segment.
/// Error cases:
/// - a manifest line is invalid, or a fetched file differs from its listing
pub(super) async fn refresh_archive(
    archive: &reqwest::Client,
    standby: &StandbySection,
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listings = fetch_manifest(archive, standby).await.context("fetching the published manifest")?;
    let manifest = std::fs::read_to_string(dir.join(MANIFEST)).unwrap_or_default();
    let mut text = manifest.clone();
    for listed in &listings {
        validate_listing(listed)?;
        let path = dir.join(&listed.file);
        if path.exists() {
            // The follower's own bytes win; only a listing for a file we lack is adopted.
            continue;
        }
        let bytes = fetch_archive_bytes(archive, standby, &listed.file).await?;
        check_listing(listed, &bytes)?;
        let staged = dir.join(format!("{}.staging", listed.file));
        std::fs::write(&staged, bytes).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, &path).with_context(|| format!("linking {}", listed.file))?;
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
    }
    let staged = dir.join(format!("{MANIFEST}.staging"));
    std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, dir.join(MANIFEST)).with_context(|| format!("linking {MANIFEST}"))?;
    Ok(())
}

/// One listing's file name must be the canonical segment name for its first seq, so a manifest
/// can never point a write outside the copy, and listings must be ordered and bounded.
/// Error cases:
/// - a name is not canonical, a seq regresses or repeats, or the list exceeds the bound
pub(super) fn validate_listing(listed: &SegmentInfo) -> anyhow::Result<()> {
    ensure!(
        crate::log::segment_seq(&listed.file) == Some(listed.first_seq),
        "archive listing {:?} is not the canonical segment name for seq {}",
        listed.file,
        listed.first_seq
    );
    Ok(())
}

/// The archive's manifest: one segment listing a line, oldest first, validated before use.
/// Error cases:
/// - the fetch fails, a line is not a listing, or the listings are unordered or unbounded
pub(super) async fn fetch_manifest(
    archive: &reqwest::Client,
    standby: &StandbySection,
) -> anyhow::Result<Vec<SegmentInfo>> {
    let bytes = fetch_archive_bytes(archive, standby, MANIFEST).await?;
    let listings: Vec<SegmentInfo> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
        .map(|line| serde_json::from_slice(line).context("parsing a manifest line"))
        .collect::<anyhow::Result<_>>()?;
    ensure!(listings.len() <= MAX_ARCHIVE_LISTINGS, "the archive lists too many segments");
    let mut previous = None;
    for listed in &listings {
        validate_listing(listed)?;
        if let Some(first) = previous {
            ensure!(listed.first_seq > first, "the archive's listings are not strictly ordered");
        }
        previous = Some(listed.first_seq);
    }
    Ok(listings)
}

/// A fetched segment must match its listing's size and hash.
/// Error cases:
/// - the bytes differ from the listing
fn check_listing(listed: &SegmentInfo, bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(
        bytes.len() as u64 == listed.bytes && sha256(bytes) == listed.sha256,
        "archive segment {} differs from its listing",
        listed.file
    );
    Ok(())
}

/// One archive GET from the first mirror that answers, bounded in bytes and time end to end.
/// Error cases:
/// - every origin fails, the body stalls past the deadline, or the body exceeds the bound
async fn fetch_archive_bytes(
    archive: &reqwest::Client,
    standby: &StandbySection,
    file: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut last = None;
    for origin in standby.mirrors.iter().map(String::as_str).chain(std::iter::once(standby.operator.as_str())) {
        let url = format!("{}/v1/archive/{file}", origin.trim_end_matches('/'));
        let fetched = timeout(FETCH_TIMEOUT, archive.get(&url).send())
            .await
            .context("archive fetch timed out")
            .and_then(|sent| sent?.error_for_status().with_context(|| format!("fetching {url}")));
        let mut response = match fetched {
            Ok(response) => response,
            Err(e) => {
                last = Some(e);
                continue;
            }
        };
        let body = async {
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.context("reading the archive")? {
                ensure!(
                    bytes.len() as u64 + chunk.len() as u64 <= MAX_ARCHIVE_BYTES,
                    "archive file {file} exceeds the bound"
                );
                bytes.extend_from_slice(&chunk);
            }
            anyhow::Ok(bytes)
        };
        match timeout(BODY_TIMEOUT, body).await {
            Ok(Ok(bytes)) => return Ok(bytes),
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow::anyhow!("reading {url} timed out")),
        }
    }
    Err(last.expect("at least one archive origin")).context("every archive origin failed")
}

/// The TLS client config for archive fetches: public roots, or no verification when the config
/// says so. The stream's frame signatures are the authenticity boundary either way.
pub(super) fn archive_client(standby: &StandbySection) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(BODY_TIMEOUT);
    if standby.danger_accept_invalid_certs {
        warn!("standby.danger_accept_invalid_certs: the operator's TLS certificate is not verified");
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("building the archive client")
}
