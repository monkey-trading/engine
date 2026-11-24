//! The node watching its disks. Every `alerts.storage.interval` it reads the bytes the host's disk allocates to each
//! store, and the size and free space of each volume the stores sit on, sets them as metrics, and raises
//! `disk_space_low` (Warning) and `disk_space_critical` (Critical) for a volume whose free share, or whose projected
//! time to fill at its recent growth, passes the thresholds. A directory or volume it cannot read raises
//! `storage_unreadable`.
//!
//! The host controls the disk and the clock, so these figures are for the operator's eyes only: a host can report
//! any size, and a host that fills the disk or stalls it is also caught by `log_sync_held`. Nothing here is sequenced
//! or read by the engine.

mod layout;
mod reading;
mod watch;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval, timeout};

pub use layout::Layout;
pub use reading::Reading;
pub use watch::Watch;

use crate::config::StorageSection;
use crate::metrics::Metrics;

/// How long a read of the stores may take before the next sample goes without it: a host that stalls the disk
/// holds the read, and one read in flight is all the node keeps.
const READ_BOUND: Duration = Duration::from_secs(30);

/// Reads and judges the stores every `cfg.interval` until a read's thread panics.
/// Error cases:
/// - the thread reading the disks panicked
pub async fn run(layout: Layout, cfg: StorageSection, metrics: Arc<Metrics>) -> anyhow::Result<()> {
    let mut every = interval(cfg.interval);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watch = Watch::new(cfg);
    let layout = Arc::new(layout);
    let mut reading: Option<JoinHandle<Reading>> = None;
    loop {
        every.tick().await;
        let mut handle = reading.take().unwrap_or_else(|| {
            let layout = layout.clone();
            tokio::task::spawn_blocking(move || layout.read())
        });
        match timeout(READ_BOUND, &mut handle).await {
            Ok(done) => {
                let done = done.context("the storage read panicked")?;
                done.record(&metrics);
                watch.observe(std::time::Instant::now(), &done, &metrics);
            }
            Err(_) => {
                reading = Some(handle);
                watch.unread(&format!("the read has not finished in {READ_BOUND:?}"));
            }
        }
    }
}

#[cfg(test)]
mod tests;
