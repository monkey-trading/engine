//! A range a segment of which is no longer on the host: the log's local window removes a segment only once the bucket
//! holds every range it holds seqs of, and the host may lose one before. The segment is read from the bucket's public
//! origin, taken only as its listing names it, and the range is built from it and the host's other segments and read
//! back as any other, with nothing written to the log's directory. One the bucket does not give back as its listing
//! names it, as when the bucket lacks a range of it or holds other bytes, holds the uploads at its range under
//! `archive_upload_lagging`, until another copy's uploader puts the range there.

use std::collections::BTreeMap;

use anyhow::Context;
use tokio::task::spawn_blocking;
use tracing::info;

use crate::archive::uploader::Work;
use crate::archive::window::RestoreSources;
use crate::archive::{Finality, absent_segments};
use crate::log::Manifest;

impl<F: Finality> Work<F> {
    /// The bytes of each segment holding a seq of range `index` that the host lacks, read from the bucket as its
    /// listing names it, by file.
    /// Error cases:
    /// - a listing cannot be read, or the bucket does not give back a segment as its listing names it
    pub(super) async fn absent_bytes(&self, view: &Manifest, index: u64) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
        let (dir, read) = (self.dir.clone(), view.clone());
        let absent = spawn_blocking(move || absent_segments(&dir, &read, index)).await??;
        let mut supplied = BTreeMap::new();
        if absent.is_empty() {
            return Ok(supplied);
        }
        let (http, bucket) = self.store.reader()?;
        let sources = RestoreSources { http, bucket, nodes: Vec::new() };
        for listed in absent {
            let bytes =
                sources.read_back(&listed).await.with_context(|| format!("{} is not on this host", listed.file))?;
            info!(segment = %listed.file, "this host lacks a segment of the range, which is read from the bucket");
            supplied.insert(listed.file, bytes);
        }
        Ok(supplied)
    }
}
