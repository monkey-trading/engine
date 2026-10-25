//! Test helper that publishes a log directory's manifest.

use crate::log::{Manifest, SegmentedLog};
use std::path::Path;
use tokio::sync::watch;

/// The manifest on disk, as a writer that has flushed everything on disk publishes it.
pub fn published_manifest(dir: &Path) -> watch::Receiver<Manifest> {
    let log = SegmentedLog::open(dir, |_| Ok(())).expect("strict test log read");
    watch::channel(log.manifest_view()).1
}
