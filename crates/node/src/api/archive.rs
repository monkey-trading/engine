//! Public canonical manifest and authenticated completed-segment bytes, and the daily snapshots beside them.

use std::io::{self, Read};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use futures_util::stream::try_unfold;
use sha2::{Digest, Sha256};
use tokio::task::spawn_blocking;
use tracing::field::display;
use tracing::{Span, error};

use crate::api::{ApiError, ApiState};
use crate::daily_snapshots;
use crate::log::{MANIFEST, open_segment_archive, segment_seq};

const JSON_LINES: &str = "application/x-ndjson";
const JSON: &str = "application/json";
const ARCHIVE_CHUNK_BYTES: usize = 64 * 1024;

/// Serves a pinned canonical catalog listing or an authenticated completed segment.
/// Error cases:
/// - the file is not a completed segment, a required proof fails, or disk bytes changed
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
        let reader = open_segment_archive(&s.log_dir, &listed)?;
        Ok(Some((listed, reader)))
    })
    .await
    .map_err(|e| unreadable(format!("archive task: {e}")))?
    .map_err(|e| unreadable(format!("archive: {e:#}")))?
    .ok_or(ApiError::NotFound)?;
    let (listed, reader) = opened;
    let headers = [(CONTENT_TYPE, JSON_LINES.to_owned()), (CONTENT_LENGTH, listed.bytes.to_string())];
    Ok((headers, chunked(reader, "checked segment")).into_response())
}

/// A body streaming `reader`'s bytes in chunks, each read on the blocking pool, paging `archive_unreadable`
/// for a read of `what` that fails.
fn chunked(reader: impl Read + Send + 'static, what: &'static str) -> Body {
    Body::from_stream(try_unfold(reader, move |mut reader| async move {
        spawn_blocking(move || {
            let mut bytes = vec![0; ARCHIVE_CHUNK_BYTES];
            let n = reader.read(&mut bytes).map_err(|e| {
                error!(alert = "archive_unreadable", "reading {what}: {e}");
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
/// listing's. Nothing but its root authenticates a snapshot to a replay, which checks it against the
/// checkpoint the log holds (`daily_snapshots`), so the check here only keeps a damaged file from being served.
/// Error cases:
/// - the node publishes no daily snapshots, or the file is neither the manifest nor a listed snapshot
/// - the manifest or the file cannot be read, or the file differs from its listing
pub async fn snapshot(State(s): State<ApiState>, Path(file): Path<String>) -> Result<Response, ApiError> {
    Span::current().record("id", display(&file));
    let dir = s.daily_snapshots.clone().ok_or(ApiError::NotFound)?;
    let permit = s.backfills.clone().acquire_owned().await.map_err(|_| ApiError::Internal)?;
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
        let mut hash = Sha256::new();
        let bytes = io::copy(&mut std::fs::File::open(&path)?, &mut hash)?;
        anyhow::ensure!(
            bytes == listed.bytes && hex::encode(hash.finalize()) == listed.sha256,
            "daily snapshot {file} differs from its listing"
        );
        let body = chunked(std::fs::File::open(&path)?, "daily snapshot");
        Ok(Some((JSON, listed.bytes, body)))
    })
    .await
    .map_err(|e| unreadable(format!("daily snapshot task: {e}")))?
    .map_err(|e| unreadable(format!("daily snapshot: {e:#}")))?;
    let (kind, bytes, body) = opened.ok_or(ApiError::NotFound)?;
    Ok(([(CONTENT_TYPE, kind.to_owned()), (CONTENT_LENGTH, bytes.to_string())], body).into_response())
}

/// Logs an archive integrity or IO failure before returning the existing API error.
fn unreadable(message: String) -> ApiError {
    error!(alert = "archive_unreadable", "{message}");
    ApiError::Internal
}
