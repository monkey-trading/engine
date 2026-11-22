//! The log's own volume. Buffered writes of the history index, the marks cache, the recovery caches and the daily
//! snapshots land in the page cache of the disk they share with the log, and a flush of them holds the log's batch
//! syncs, and every order and stream frame waiting behind one.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Context;
use tracing::warn;

use super::NodeConfig;
use super::validation::resolved;

impl NodeConfig {
    /// A production build refuses a log directory on the filesystem of another store the node writes; a development
    /// build warns, naming each.
    /// `load` holds a node to it in a production build other than the unit tests', whose scratch directories share
    /// the build's disk.
    /// Error cases:
    /// - a production build and `history.dir`, `snapshots.dir` or `daily_snapshots.dir` shares a device with
    ///   `node.log_dir`
    /// - a directory's device cannot be read
    pub(super) fn check_log_volume(&self, production: bool) -> anyhow::Result<()> {
        let shared = self.stores_on_the_logs_volume(device)?;
        if shared.is_empty() {
            return Ok(());
        }
        let named = shared.iter().map(|(name, dir)| format!("{name} ({})", dir.display())).collect::<Vec<_>>();
        let named = named.join(", ");
        let log = self.node.log_dir.display();
        anyhow::ensure!(
            !production,
            "node.log_dir ({log}) must be on a volume of its own, but {named} share it: their writes stall the log's syncs"
        );
        warn!(log = %log, shared = %named, "the log shares its volume with stores whose writes stall its syncs");
        Ok(())
    }

    /// The stores whose directory is on the device of the log directory, by name.
    fn stores_on_the_logs_volume(
        &self,
        device: impl Fn(&Path) -> anyhow::Result<u64>,
    ) -> anyhow::Result<Vec<(&'static str, &Path)>> {
        let stores = [
            Some(("history.dir", self.history.dir.as_path())),
            self.snapshots.as_ref().map(|s| ("snapshots.dir", s.dir.as_path())),
            self.daily_snapshots.as_ref().map(|s| ("daily_snapshots.dir", s.dir.as_path())),
        ];
        let log = device(&self.node.log_dir)?;
        let mut shared = Vec::new();
        for (name, dir) in stores.into_iter().flatten() {
            if device(dir)? == log {
                shared.push((name, dir));
            }
        }
        Ok(shared)
    }
}

/// The device a write to `path` lands on, through links, whether or not the directory exists yet: its nearest
/// existing ancestor's.
fn device(path: &Path) -> anyhow::Result<u64> {
    let real = resolved(path)?;
    let existing = real.ancestors().find(|ancestor| ancestor.exists()).unwrap_or(&real);
    Ok(std::fs::metadata(existing).with_context(|| format!("reading the device of {}", existing.display()))?.dev())
}

#[cfg(test)]
#[path = "volumes_tests.rs"]
mod tests;
