//! The standby's archive reads: the cold-start bootstrap, the takeover's refresh, and the
//! bounded, validated fetches behind both. Each segment comes from the archive's bucket first, when the
//! reader names one, then from the node origins (`archive::Segments`).

use std::collections::BTreeSet;

use anyhow::{Context, bail, ensure};
use reqwest::header::LOCATION;
use tokio::time::timeout;
use tracing::{info, warn};

use super::{BODY_TIMEOUT, FETCH_TIMEOUT, sha256};
use crate::archive::{ArchiveOrigins, Segments};
use crate::log::MANIFEST;
use crate::log::{Manifest, SegmentInfo};

/// The most bytes of the archive's manifest a fetch reads: about seven million listings, or seventy years of
/// launch flow in segments of 10,000 commands. The listings it holds take about as much memory as the node's
/// own manifest does.
const MAX_MANIFEST_BYTES: u64 = 1 << 30;

/// What one archive origin served for a file.
pub(crate) enum Fetched {
    /// The file's bytes.
    Bytes(Vec<u8>),
    /// The origin no longer holds the segment, and redirected (`308`) to the bucket's object of the range that
    /// holds its first seq.
    Moved(reqwest::Url),
}

/// What the archive served failed its integrity checks: a file differs from its listing, or the
/// listings are not canonical or not in order. Any other failure — an origin that is unreachable,
/// slow, or answers an error, or a body over its bound — is a transport failure.
#[derive(Debug)]
pub(crate) struct ArchiveIntegrity(String);

impl std::fmt::Display for ArchiveIntegrity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ArchiveIntegrity {}

/// A cold start: fetches the published manifest and every completed segment it lists into the
/// empty copy, from the bucket first when `origins` names one (`Segments`), checking each file
/// against its listing, and writes the manifest. A missing manifest (a fresh deployment) leaves the
/// copy empty for the stream to fill from seq 0.
/// Error cases:
/// - a fetch fails, exceeds the bound, or a file differs from its listing
pub(crate) async fn bootstrap_archive(
    archive: &reqwest::Client,
    origins: &(impl ArchiveOrigins + ?Sized),
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let mut segments = Segments::new(archive, origins);
    let listings = match segments.manifest().await {
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
        let bytes = segments.segment(listed).await?;
        // Stage under a name `SegmentedLog::open` ignores, then link into place: a crash leaves
        // either the old or the new whole file, never a torn one. Syncs make the adopted copy
        // durable: a confirmed handover must survive a VM stop, and the guest page cache is lost
        // whenever the VM stops.
        let staged = dir.join(format!("{}.staging", listed.file));
        write_synced(&staged, &bytes)?;
        std::fs::rename(&staged, dir.join(&listed.file)).with_context(|| format!("linking {}", listed.file))?;
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
    }
    let staged = dir.join(format!("{MANIFEST}.staging"));
    write_synced(&staged, text.as_bytes())?;
    std::fs::rename(&staged, dir.join(MANIFEST)).with_context(|| format!("linking {MANIFEST}"))?;
    sync_dir(dir)?;
    info!(segments = listings.len(), "backfilled the copy from the operator's archive");
    Ok(())
}

/// Best-effort refresh of completed segments the copy lacks, run once a takeover is promoted, from the
/// bucket first when `origins` names one: the checkpoint the vault accepted last may post after the
/// stream last delivered. Existing files are never overwritten: a local file with the segment's name
/// is this follower's own frame-verified bytes, which wins over anything an archive serves. New
/// segments are staged and linked before the manifest gains their lines, and the manifest keeps its
/// local lines, so a crash leaves at most unlisted trailing files, which open as the open segment.
/// Each staged segment is linked only once `accept` passes it.
/// Error cases:
/// - a manifest line is invalid, a fetched file differs from its listing, or `accept` refuses one
pub(crate) async fn refresh_archive(
    archive: &reqwest::Client,
    origins: &(impl ArchiveOrigins + ?Sized),
    dir: &std::path::Path,
    mut accept: impl FnMut(&std::path::Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut segments = Segments::new(archive, origins);
    let listings = segments.manifest().await.context("fetching the published manifest")?;
    let manifest = std::fs::read_to_string(dir.join(MANIFEST)).unwrap_or_default();
    // The copy's own stream-fed tail wins: with an open (unlisted) segment holding entries, the
    // follower's segmentation may differ from the operator's, so no later listing can be
    // adopted safely. Only a copy whose tail is entirely listed backfills. Its last listed segment
    // and any after it say so: the segments before it need not be on this host.
    let last_listed = crate::log::Manifest::load(dir)?.segments.last().map_or(0, |listed| listed.first_seq);
    let from = crate::log::Offset::start_of(last_listed);
    let open_entries = crate::log::SegmentedLog::open_from(dir, from, |_| Ok(()))?.has_open_tail();
    if open_entries {
        warn!("the copy has an unlisted tail; the archive refresh is skipped");
        return Ok(());
    }
    let held: BTreeSet<String> = Manifest::load(dir)?.segments.iter().map(|listed| listed.file.clone()).collect();
    let mut text = manifest.clone();
    for listed in &listings {
        validate_listing(listed)?;
        let path = dir.join(&listed.file);
        if path.exists() || held.contains(&listed.file) {
            continue;
        }
        let bytes = segments.segment(listed).await?;
        let staged = dir.join(format!("{}.staging", listed.file));
        write_synced(&staged, &bytes)?;
        if let Err(e) = accept(&staged) {
            let _ = std::fs::remove_file(&staged);
            return Err(e).with_context(|| format!("archive segment {}", listed.file));
        }
        std::fs::rename(&staged, &path).with_context(|| format!("linking {}", listed.file))?;
        // Commit each segment's listing as it lands: a crash then leaves the manifest naming
        // exactly the linked prefix, which opens.
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
        let staged_manifest = dir.join(format!("{MANIFEST}.staging"));
        write_synced(&staged_manifest, text.as_bytes())?;
        std::fs::rename(&staged_manifest, dir.join(MANIFEST)).with_context(|| format!("linking {MANIFEST}"))?;
    }
    sync_dir(dir)?;
    Ok(())
}

/// One listing's file name must be the canonical segment name for its first seq, so a manifest
/// can never point a write outside the copy, and listings must be ordered and bounded.
/// Error cases:
/// - the name is not canonical (`ArchiveIntegrity`)
pub(crate) fn validate_listing(listed: &SegmentInfo) -> anyhow::Result<()> {
    if crate::log::segment_seq(&listed.file) != Some(listed.first_seq) {
        return Err(ArchiveIntegrity(format!(
            "archive listing {:?} is not the canonical segment name for seq {}",
            listed.file, listed.first_seq
        ))
        .into());
    }
    Ok(())
}

/// The archive's manifest, from the first node origin of `origins` that serves it whole: one segment listing a
/// line, oldest first, validated before use, however many it lists within `MAX_MANIFEST_BYTES`.
/// Error cases:
/// - the fetch fails, or a line is not a listing
/// - a listing is not canonical, or the listings are unordered (`ArchiveIntegrity`)
pub(crate) async fn fetch_manifest(
    archive: &reqwest::Client,
    origins: &(impl ArchiveOrigins + ?Sized),
) -> anyhow::Result<Vec<SegmentInfo>> {
    let mut last = None;
    let mut bytes = None;
    for origin in origins.nodes() {
        match fetch_from(archive, origin, MANIFEST, MAX_MANIFEST_BYTES).await {
            Ok(Fetched::Bytes(served)) => {
                bytes = Some(served);
                break;
            }
            Ok(Fetched::Moved(location)) => {
                last = Some(anyhow::anyhow!("{origin} redirected its manifest to {location}"))
            }
            Err(e) => last = Some(e),
        }
    }
    let Some(bytes) = bytes else {
        return Err(last.unwrap_or_else(|| anyhow::anyhow!("no archive origin to read")))
            .context("every archive origin failed");
    };
    let listings: Vec<SegmentInfo> = bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
        .map(|line| serde_json::from_slice(line).context("parsing a manifest line"))
        .collect::<anyhow::Result<_>>()?;
    let mut previous = None;
    for listed in &listings {
        validate_listing(listed)?;
        if previous.is_some_and(|first| listed.first_seq <= first) {
            return Err(ArchiveIntegrity("the archive's listings are not strictly ordered".into()).into());
        }
        previous = Some(listed.first_seq);
    }
    Ok(listings)
}

/// A fetched segment must match its listing's size and hash.
/// Error cases:
/// - the bytes differ from the listing (`ArchiveIntegrity`)
pub(crate) fn check_listing(listed: &SegmentInfo, bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.len() as u64 != listed.bytes || sha256(bytes) != listed.sha256 {
        return Err(ArchiveIntegrity(format!("archive segment {} differs from its listing", listed.file)).into());
    }
    Ok(())
}

/// Writes `bytes` to `path` and syncs them to storage.
fn write_synced(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes).with_context(|| format!("writing {}", path.display()))?;
    file.sync_all().with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

/// Syncs the directory entry naming everything written under it.
fn sync_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::File::open(dir)?.sync_all().with_context(|| format!("syncing {}", dir.display()))?;
    Ok(())
}

/// One archive GET of `file` from `origin`, bounded to `bound` bytes and in time end to end: its bytes, or the
/// bucket's object a `308` names.
/// Error cases:
/// - the request fails or times out, or the origin answers an error, or a `308` that names no URL
/// - the body stalls past the deadline, or exceeds the bound
pub(crate) async fn fetch_from(
    archive: &reqwest::Client,
    origin: &str,
    file: &str,
    bound: u64,
) -> anyhow::Result<Fetched> {
    let url = format!("{}/v1/archive/{file}", origin.trim_end_matches('/'));
    let response = timeout(FETCH_TIMEOUT, archive.get(&url).send()).await.context("archive fetch timed out")??;
    if response.status() == reqwest::StatusCode::PERMANENT_REDIRECT {
        let location = response.headers().get(LOCATION).and_then(|location| location.to_str().ok());
        let location = location.with_context(|| format!("{url} answered 308 with no Location"))?;
        let target = response.url().join(location).with_context(|| format!("{url} redirected to {location:?}"))?;
        return Ok(Fetched::Moved(target));
    }
    let mut response = response.error_for_status().with_context(|| format!("fetching {url}"))?;
    let body = async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("reading the archive")? {
            ensure!(bytes.len() as u64 + chunk.len() as u64 <= bound, "archive file {file} exceeds the bound");
            bytes.extend_from_slice(&chunk);
        }
        anyhow::Ok(bytes)
    };
    match timeout(BODY_TIMEOUT, body).await {
        Ok(bytes) => bytes.map(Fetched::Bytes),
        Err(_) => bail!("reading {url} timed out"),
    }
}

/// The TLS client config for archive fetches: public roots, or no verification when the config
/// says so. The stream's frame signatures are the authenticity boundary either way.
pub(crate) fn archive_client(danger_accept_invalid_certs: bool) -> anyhow::Result<reqwest::Client> {
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| reqwest::Certificate::from_der(cert.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    let mut builder = reqwest::Client::builder()
        .timeout(BODY_TIMEOUT)
        .tls_certs_only(roots)
        .redirect(reqwest::redirect::Policy::none());
    if danger_accept_invalid_certs {
        warn!("the archive fetch does not verify TLS certificates");
        builder = builder.danger_accept_invalid_certs(true);
    }
    builder.build().context("building the archive client")
}
