//! A range a segment of which is no longer on the host: the log's local window removes a segment only once the bucket
//! holds every range it holds seqs of, and the host may lose one before. The segment is put back from the bucket's
//! public origin, taken only as its listing names it, so the range builds and reads back against the copy as any
//! other. One the bucket does not give back as its listing names it, as when the bucket lacks a range of it or holds
//! other bytes, holds the uploads at its range under `archive_upload_lagging`, until another copy's uploader puts the
//! range there.

use anyhow::Context;
use tokio::task::spawn_blocking;
use tracing::warn;

use super::Work;
use crate::archive::window::{RestoreSources, write_segment};
use crate::archive::{Finality, absent_segments};
use crate::log::Manifest;

impl<F: Finality> Work<F> {
    /// Puts back on the host each segment holding a seq of range `index` that the host lacks, newest first, from the
    /// bucket, and returns how many. The window removes none of them again while the range is at or past the count
    /// of ranges the bucket holds, which it is until it is confirmed.
    /// Error cases:
    /// - a listing cannot be read, the bucket does not give back a segment as its listing names it, or it cannot be
    ///   written
    pub(super) async fn put_back_absent(&self, view: &Manifest, index: u64) -> anyhow::Result<usize> {
        let (dir, read) = (self.dir.clone(), view.clone());
        let absent = spawn_blocking(move || absent_segments(&dir, &read, index)).await??;
        if absent.is_empty() {
            return Ok(0);
        }
        let (http, bucket) = self.store.reader(&self.prefix)?;
        let sources = RestoreSources { http, bucket, nodes: Vec::new() };
        for listed in absent.iter().rev() {
            let bytes =
                sources.read_back(listed).await.with_context(|| format!("{} is not on this host", listed.file))?;
            let (dir, put) = (self.dir.clone(), listed.clone());
            spawn_blocking(move || write_segment(&dir, &put, &bytes)).await??;
            warn!(segment = %listed.file, "this host lacked a segment of a range the bucket holds, put back from it");
        }
        Ok(absent.len())
    }
}
