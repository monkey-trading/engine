//! Public canonical manifest and authenticated completed-segment bytes.

use std::io::{self, Read};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use futures_util::stream::try_unfold;
use tokio::task::spawn_blocking;
use tracing::field::display;
use tracing::{Span, error};

use crate::api::{ApiError, ApiState};
use crate::log::{MANIFEST, open_segment_archive, segment_seq};

const JSON_LINES: &str = "application/x-ndjson";
const ARCHIVE_CHUNK_BYTES: usize = 64 * 1024;

/// Serves a pinned canonical catalog listing or an authenticated completed segment.
/// Error cases:
/// - the file is not a completed segment, a required proof fails, or disk bytes changed
pub async fn archive(State(s): State<ApiState>, Path(file): Path<String>) -> Result<Response, ApiError> {
    Span::current().record("id", display(&file));
    if file == MANIFEST {
        // One listing at a time, awaited off the blocking pool. The view is read under the lock, so
        // the cached listing only moves forward and each request proves only segments completed
        // since; it takes no backfill permit.
        let mut cached = s.listing.lock().await;
        let view = s.manifest.borrow().clone();
        let known = cached.clone();
        let (listing, body) = spawn_blocking(move || {
            view.ensure_verified()?;
            let Some(catalog) = &view.catalog else { return Ok((None, Bytes::from(view.to_jsonl()?))) };
            let listing = catalog.listing_after(known.as_ref())?;
            let body = Bytes::from_owner(listing.bytes.clone());
            anyhow::Ok((Some(listing), body))
        })
        .await
        .map_err(|e| unreadable(format!("manifest task: {e}")))?
        .map_err(|e| unreadable(format!("manifest: {e:#}")))?;
        if listing.is_some() {
            *cached = listing;
        }
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
    let body = try_unfold(reader, |mut reader| async move {
        spawn_blocking(move || {
            let mut bytes = vec![0; ARCHIVE_CHUNK_BYTES];
            let n = reader.read(&mut bytes).map_err(|e| {
                error!(alert = "archive_unreadable", "reading checked segment: {e}");
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
    });
    let headers = [(CONTENT_TYPE, JSON_LINES.to_owned()), (CONTENT_LENGTH, listed.bytes.to_string())];
    Ok((headers, Body::from_stream(body)).into_response())
}

/// Logs an archive integrity or IO failure before returning the existing API error.
fn unreadable(message: String) -> ApiError {
    error!(alert = "archive_unreadable", "{message}");
    ApiError::Internal
}
