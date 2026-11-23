//! A member's reads of the daily snapshots its origins publish: each origin's manifest, and one snapshot's file
//! fetched into the member's own directory of daily snapshots, which its replica may start from (`start`) and which
//! it serves once it sequences. What makes a snapshot usable is its signature and its root, never its source, so
//! the fetch checks only what keeps a damaged or unbounded file out: a canonical name, and each byte against the
//! listing's length and SHA-256, within bounds of size and time.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use sha2::{Digest, Sha256};
use tokio::time::timeout;
use tracing::debug;

use crate::daily_snapshots::writer::{publish_staged, sync_dir};
use crate::daily_snapshots::{Listed, MANIFEST, MAX_SNAPSHOT_BYTES, offset_named};
use crate::log::Offset;
use crate::paced::Paced;

/// The most bytes a snapshot manifest may hold: one line a day for decades.
const MAX_MANIFEST_BYTES: u64 = 8 << 20;
/// How long a manifest's fetch may take, end to end.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a snapshot's fetch may take, end to end.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The snapshots `origins` list at or before the checkpoint at seq `bound` under their canonical names, each with
/// the origin that lists it, and whether any origin answered: with its manifest, or with none, as an origin that
/// publishes no snapshots answers. An origin whose manifest cannot be read is passed over.
pub(crate) async fn offered(client: &reqwest::Client, origins: &[String], bound: u64) -> (Vec<(String, Listed)>, bool) {
    let (mut offered, mut answered) = (Vec::new(), false);
    for origin in origins {
        match manifest(client, origin).await {
            Ok(listings) => {
                answered = true;
                let canonical = |listing: &Listed| {
                    listing.seq <= bound
                        && offset_named(&listing.file) == Some(Offset { seq: listing.seq, index: listing.index })
                };
                offered.extend(listings.into_iter().filter(canonical).map(|listing| (origin.clone(), listing)));
            }
            Err(e)
                if e.downcast_ref::<reqwest::Error>().and_then(reqwest::Error::status)
                    == Some(reqwest::StatusCode::NOT_FOUND) =>
            {
                answered = true;
                debug!(origin = %origin, "this origin publishes no daily snapshots");
            }
            Err(e) => debug!(origin = %origin, "no daily snapshot listing from this origin: {e:#}"),
        }
    }
    (offered, answered)
}

/// `origin`'s daily snapshot manifest.
/// Error cases:
/// - the fetch fails, takes too long or exceeds the bound, or a line is not a listing
async fn manifest(client: &reqwest::Client, origin: &str) -> anyhow::Result<Vec<Listed>> {
    let url = format!("{}/v1/archive/snapshots/{MANIFEST}", origin.trim_end_matches('/'));
    let mut bytes = Vec::new();
    let read = async {
        let mut response = client.get(&url).send().await?.error_for_status()?;
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() as u64 + chunk.len() as u64 <= MAX_MANIFEST_BYTES, "{url} exceeds the bound");
            bytes.extend_from_slice(&chunk);
        }
        anyhow::Ok(())
    };
    timeout(MANIFEST_TIMEOUT, read).await.with_context(|| format!("fetching {url} timed out"))??;
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
        .map(|line| serde_json::from_slice(line).with_context(|| format!("a line of {url} is not a listing")))
        .collect()
}

/// Fetches `listing`'s snapshot from `origin` into `dir` under its name, unlisted: staged, each byte checked
/// against the listing's length and SHA-256, synced, then renamed into place. Its path.
/// Error cases:
/// - the listing is over the bound, the fetch fails or takes too long, or the bytes differ from the listing
/// - `dir` cannot be created, or the file cannot be written, synced or renamed
pub(crate) async fn download(
    client: &reqwest::Client,
    origin: &str,
    dir: &Path,
    listing: &Listed,
) -> anyhow::Result<PathBuf> {
    ensure!(listing.bytes <= MAX_SNAPSHOT_BYTES, "daily snapshot {} is longer than the bound", listing.file);
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let url = format!("{}/v1/archive/snapshots/{}", origin.trim_end_matches('/'), listing.file);
    let staged = dir.join(format!("{}.tmp", listing.file));
    let read = async {
        let mut response = client.get(&url).timeout(SNAPSHOT_TIMEOUT).send().await?.error_for_status()?;
        let mut file = BufWriter::new(Paced::new(
            File::create(&staged).with_context(|| format!("creating {}", staged.display()))?,
        ));
        let (mut hash, mut bytes) = (Sha256::new(), 0u64);
        while let Some(chunk) = response.chunk().await? {
            bytes += chunk.len() as u64;
            ensure!(bytes <= listing.bytes, "{url} is longer than its listing");
            hash.update(&chunk);
            file.write_all(&chunk).with_context(|| format!("writing {}", staged.display()))?;
        }
        ensure!(
            bytes == listing.bytes && hex::encode(hash.finalize()) == listing.sha256,
            "{url} differs from its listing"
        );
        let file =
            file.into_inner().map_err(|e| e.into_error()).with_context(|| format!("writing {}", staged.display()))?;
        file.get_ref().sync_all().with_context(|| format!("syncing {}", staged.display()))?;
        anyhow::Ok(())
    };
    let fetched = timeout(SNAPSHOT_TIMEOUT, read).await.with_context(|| format!("fetching {url} timed out"));
    if let Err(e) = fetched.and_then(|read| read) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    let path = dir.join(&listing.file);
    publish_staged(&staged, &path).with_context(|| format!("publishing {}", listing.file))?;
    sync_dir(dir)?;
    Ok(path)
}

#[cfg(test)]
mod tests;
