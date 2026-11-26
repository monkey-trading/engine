//! Public canonical manifest and authenticated completed-segment bytes, and the daily snapshots beside them. A
//! listed segment the host no longer holds is redirected to the archive's bucket, whose objects readers read
//! themselves: the enclave relays none of them.

use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION};
use axum::response::{IntoResponse, Response};
use futures_util::stream::try_unfold;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::spawn_blocking;
use tracing::field::display;
use tracing::{Span, error, warn};

use crate::api::{ApiError, ApiState};
use crate::archive::RANGE_SEQS;
use crate::daily_snapshots;
use crate::log::{ArchiveReader, MANIFEST, SegmentInfo, open_segment_archive, segment_seq};

const JSON_LINES: &str = "application/x-ndjson";
const JSON: &str = "application/json";
const ARCHIVE_CHUNK_BYTES: usize = 64 * 1024;
/// How many daily snapshot requests may check their files against their listings at once; the streams that
/// follow are bounded as the segments' are, by the API's connection limits.
const SNAPSHOT_CHECKS: usize = 2;

/// The daily snapshots the archive serves: their directory, and the checks of them under way, held apart
/// from the stream's backfills, since a snapshot's check reads all of a file of hundreds of megabytes.
#[derive(Debug)]
pub struct ServedSnapshots {
    pub dir: PathBuf,
    checks: Arc<Semaphore>,
}

/// What the archive serves for a listed segment.
enum Served {
    /// The segment, checked against its listing, and its reader.
    Segment(SegmentInfo, ArchiveReader),
    /// The bucket's object of the range holding the segment's first seq, which the host no longer holds.
    Moved(reqwest::Url),
}

impl ServedSnapshots {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, checks: Arc::new(Semaphore::new(SNAPSHOT_CHECKS)) }
    }
}

/// Serves a pinned canonical catalog listing or an authenticated completed segment, or redirects (`308`) a listed
/// segment the host no longer holds to the bucket's object of the range holding its first seq.
/// Error cases:
/// - the file is not a completed segment, a required proof fails, or disk bytes changed
/// - the segment is not on disk and the node names no bucket
pub async fn archive(State(s): State<ApiState>, Path(file): Path<String>) -> Result<Response, ApiError> {
    Span::current().record("id", display(&file));
    if file == MANIFEST {
        // One listing at a time, awaited off the blocking pool. The view is read under the lock, so
        // the cached listing only moves forward and each request proves only segments completed
        // since; it takes no backfill permit. The walk holds the lock and caches its own listing, so
        // a request dropped mid-walk, as when its client disconnects, still leaves it for the next.
        let mut cached = s.listing.clone().lock_owned().await;
        let view = s.manifest.borrow().clone();
        let body = spawn_blocking(move || {
            view.ensure_verified()?;
            let Some(catalog) = &view.catalog else { return Ok(Bytes::from(view.to_jsonl()?)) };
            let listing = catalog.listing_after(cached.as_ref())?;
            let body = Bytes::from_owner(listing.bytes.clone());
            *cached = Some(listing);
            anyhow::Ok(body)
        })
        .await
        .map_err(|e| unreadable(format!("manifest task: {e}")))?
        .map_err(|e| unreadable(format!("manifest: {e:#}")))?;
        return Ok(
            ([(CONTENT_TYPE, JSON_LINES.to_owned()), (CONTENT_LENGTH, body.len().to_string())], body).into_response()
        );
    }
    let view = s.manifest.borrow().clone();
    let seq = segment_seq(&file).ok_or(ApiError::NotFound)?;
    let permit = s.backfills.clone().acquire_owned().await.map_err(|_| ApiError::Internal)?;
    // The permit bounds the whole-segment hash at open; the body's chunk reads take none, so a
    // slow download never holds a permit stream backfill needs.
    let opened = spawn_blocking(move || {
        let _held = permit;
        view.ensure_verified()?;
        let listed = view.lookup(seq)?;
        let Some(listed) = listed else {
            return anyhow::Ok(None);
        };
        match open_segment_archive(&s.log_dir, &listed) {
            Ok(reader) => Ok(Some(Served::Segment(listed, reader))),
            // Only a file that is gone is in the bucket's keeping: one that is there and fails its check pages.
            Err(e) => match (&s.archive_objects, s.log_dir.join(&listed.file).try_exists()) {
                (Some(bucket), Ok(false)) => Ok(Some(Served::Moved(bucket.object(listed.first_seq / RANGE_SEQS)?))),
                _ => Err(e),
            },
        }
    })
    .await
    .map_err(|e| unreadable(format!("archive task: {e}")))?
    .map_err(|e| unreadable(format!("archive: {e:#}")))?
    .ok_or(ApiError::NotFound)?;
    match opened {
        Served::Segment(listed, reader) => {
            let headers = [(CONTENT_TYPE, JSON_LINES.to_owned()), (CONTENT_LENGTH, listed.bytes.to_string())];
            Ok((headers, chunked(reader, "checked segment", false)).into_response())
        }
        Served::Moved(object) => Ok((StatusCode::PERMANENT_REDIRECT, [(LOCATION, object.to_string())]).into_response()),
    }
}

/// A body streaming `reader`'s bytes in chunks, each read on the blocking pool, paging for a read of `what`
/// that fails: Critical `archive_unreadable` for a segment, or Warning `daily_snapshot_unreadable` for a
/// daily snapshot (`snapshot`).
fn chunked(reader: impl Read + Send + 'static, what: &'static str, snapshot: bool) -> Body {
    Body::from_stream(try_unfold(reader, move |mut reader| async move {
        spawn_blocking(move || {
            let mut bytes = vec![0; ARCHIVE_CHUNK_BYTES];
            let n = reader.read(&mut bytes).map_err(|e| {
                if snapshot {
                    warn!(alert = "daily_snapshot_unreadable", "reading {what}: {e}");
                } else {
                    error!(alert = "archive_unreadable", "reading {what}: {e}");
                }
                e
            })?;
            if n == 0 {
                return Ok(None);
            }
            bytes.truncate(n);
            Ok::<_, io::Error>(Some((bytes, reader)))
        })
        .await
        .map_err(io::Error::other)?
    }))
}

/// Serves the daily snapshots' manifest, or one daily snapshot it lists, once its length and SHA-256 are the
/// listing's, `SNAPSHOT_CHECKS` checks at a time. A replay checks a snapshot against the checkpoint the log holds and
/// the replay of the log before it (`daily_snapshots`), so the check here only keeps a damaged file from being
/// served.
/// Error cases:
/// - the node publishes no daily snapshots, or the file is neither the manifest nor a listed snapshot
/// - the manifest or the file cannot be read, or the file differs from its listing
pub async fn snapshot(State(s): State<ApiState>, Path(file): Path<String>) -> Result<Response, ApiError> {
    Span::current().record("id", display(&file));
    let served = s.daily_snapshots.clone().ok_or(ApiError::NotFound)?;
    let permit = served.checks.clone().acquire_owned().await.map_err(|_| ApiError::Internal)?;
    let dir = served.dir.clone();
    let opened = spawn_blocking(move || {
        let _held = permit;
        if file == daily_snapshots::MANIFEST {
            let manifest = std::fs::read(dir.join(&file)).or_else(|e| match e.kind() {
                io::ErrorKind::NotFound => Ok(Vec::new()),
                _ => Err(e),
            })?;
            return anyhow::Ok(Some((JSON_LINES, manifest.len() as u64, Body::from(manifest))));
        }
        let listed = daily_snapshots::read_manifest(&dir)?.into_iter().find(|listed| listed.file == file);
        let Some(listed) = listed else { return Ok(None) };
        let path = dir.join(&listed.file);
        let (mut reading, mut chunk, mut hash, mut bytes) =
            (std::fs::File::open(&path)?, vec![0; ARCHIVE_CHUNK_BYTES], Sha256::new(), 0u64);
        loop {
            let read = reading.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            hash.update(&chunk[..read]);
            bytes += read as u64;
        }
        anyhow::ensure!(
            bytes == listed.bytes && hex::encode(hash.finalize()) == listed.sha256,
            "daily snapshot {file} differs from its listing"
        );
        let body = chunked(std::fs::File::open(&path)?, "daily snapshot", true);
        Ok(Some((JSON, listed.bytes, body)))
    })
    .await
    .map_err(|e| snapshot_unreadable(format!("daily snapshot task: {e}")))?
    .map_err(|e| snapshot_unreadable(format!("daily snapshot: {e:#}")))?;
    let (kind, bytes, body) = opened.ok_or(ApiError::NotFound)?;
    Ok(([(CONTENT_TYPE, kind.to_owned()), (CONTENT_LENGTH, bytes.to_string())], body).into_response())
}

/// Logs an archive integrity or IO failure before returning the existing API error.
fn unreadable(message: String) -> ApiError {
    error!(alert = "archive_unreadable", "{message}");
    ApiError::Internal
}

/// A daily snapshot request that failed, paging Warning `daily_snapshot_unreadable`: the log and its archive
/// are untouched, and a replay in ranges takes the day from the snapshot before.
fn snapshot_unreadable(message: String) -> ApiError {
    warn!(alert = "daily_snapshot_unreadable", "{message}");
    ApiError::Internal
}
