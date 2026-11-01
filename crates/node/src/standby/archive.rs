//! The standby's archive reads: the cold-start bootstrap, the takeover's refresh, and the
//! bounded, validated fetches behind both.

use anyhow::{Context, ensure};
use tokio::time::timeout;
use tracing::{info, warn};

use super::{BODY_TIMEOUT, FETCH_TIMEOUT, MAX_ARCHIVE_BYTES, MAX_ARCHIVE_LISTINGS, sha256};
use crate::log::MANIFEST;
use crate::log::SegmentInfo;

/// A cold start: fetches the published manifest and every completed segment it lists into the
/// empty copy, checking each file against its listing, and writes the manifest. A missing
/// manifest (a fresh deployment) leaves the copy empty for the stream to fill from seq 0.
/// Error cases:
/// - a fetch fails, exceeds the bound, or a file differs from its listing
pub(crate) async fn bootstrap_archive(
    archive: &reqwest::Client,
    origins: &[String],
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listings = match fetch_manifest(archive, origins).await {
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
        let bytes = fetch_archive_bytes(archive, origins, &listed.file, Some(listed)).await?;
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
pub(crate) async fn refresh_archive(
    archive: &reqwest::Client,
    origins: &[String],
    dir: &std::path::Path,
) -> anyhow::Result<()> {
    let listings = fetch_manifest(archive, origins).await.context("fetching the published manifest")?;
    let manifest = std::fs::read_to_string(dir.join(MANIFEST)).unwrap_or_default();
    // The copy's own stream-fed tail wins: with an open (unlisted) segment holding entries, the
    // follower's segmentation may differ from the operator's, so no later listing can be
    // adopted safely. Only a copy whose tail is entirely listed backfills.
    let open_entries = crate::log::SegmentedLog::open(dir, |_| Ok(()))?.has_open_tail();
    if open_entries {
        warn!("the copy has an unlisted tail; the archive refresh is skipped");
        return Ok(());
    }
    let mut text = manifest.clone();
    for listed in &listings {
        validate_listing(listed)?;
        let path = dir.join(&listed.file);
        if path.exists() || text.contains(&listed.file) {
            continue;
        }
        let bytes = fetch_archive_bytes(archive, origins, &listed.file, Some(listed)).await?;
        check_listing(listed, &bytes)?;
        let staged = dir.join(format!("{}.staging", listed.file));
        std::fs::write(&staged, bytes).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, &path).with_context(|| format!("linking {}", listed.file))?;
        // Commit each segment's listing as it lands: a crash then leaves the manifest naming
        // exactly the linked prefix, which opens.
        text.push_str(&serde_json::to_string(listed).context("encoding a manifest line")?);
        text.push('\n');
        let staged_manifest = dir.join(format!("{MANIFEST}.staging"));
        std::fs::write(&staged_manifest, &text).with_context(|| format!("writing {}", staged_manifest.display()))?;
        std::fs::rename(&staged_manifest, dir.join(MANIFEST)).with_context(|| format!("linking {MANIFEST}"))?;
    }
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
pub(crate) async fn fetch_manifest(archive: &reqwest::Client, origins: &[String]) -> anyhow::Result<Vec<SegmentInfo>> {
    let bytes = fetch_archive_bytes(archive, origins, MANIFEST, None).await?;
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
    origins: &[String],
    file: &str,
    listed: Option<&SegmentInfo>,
) -> anyhow::Result<Vec<u8>> {
    let mut last = None;
    for origin in origins {
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
            Ok(Ok(bytes)) => {
                if let Some(listed) = listed {
                    // A bad mirror must not block the rest: its bytes are only adopted when
                    // they match the listing.
                    if let Err(e) = check_listing(listed, &bytes) {
                        last = Some(e);
                        continue;
                    }
                }
                return Ok(bytes);
            }
            Ok(Err(e)) => last = Some(e),
            Err(_) => last = Some(anyhow::anyhow!("reading {url} timed out")),
        }
    }
    Err(last.expect("at least one archive origin")).context("every archive origin failed")
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
