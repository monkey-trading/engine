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

use anyhow::{Context, bail};
use tokio::sync::oneshot;
use tokio::time::{MissedTickBehavior, interval, timeout};

pub use layout::Layout;
pub use reading::Reading;
pub use watch::Watch;

use crate::config::StorageSection;
use crate::metrics::Metrics;

/// How long a read of the stores may take before the next sample goes without it: a host that stalls the disk
/// holds the read, and one read in flight is all the node keeps.
const READ_BOUND: Duration = Duration::from_secs(30);

/// Reads and judges the stores every `cfg.interval` until a read's thread panics. A read runs on a thread of its own
/// that nothing waits for, so a host that stalls the disk cannot hold the runtime's shutdown.
/// Error cases:
/// - the thread reading the disks cannot start or panicked
pub async fn run(layout: Layout, cfg: StorageSection, metrics: Arc<Metrics>) -> anyhow::Result<()> {
    run_reads(move || layout.read(), cfg, metrics, READ_BOUND).await
}

/// `run` over `read`, which a read longer than `bound` leaves in flight: the next ticks wait for it instead of
/// starting another, and raise `storage_unreadable` for `all` until it finishes.
/// Error cases:
/// - the thread reading cannot start or panicked
async fn run_reads(
    read: impl Fn() -> Reading + Send + Sync + 'static,
    cfg: StorageSection,
    metrics: Arc<Metrics>,
    bound: Duration,
) -> anyhow::Result<()> {
    let mut every = interval(cfg.interval);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut watch = Watch::new(cfg);
    let read = Arc::new(read);
    let mut reading: Option<oneshot::Receiver<Reading>> = None;
    loop {
        every.tick().await;
        let mut pending = match reading.take() {
            Some(pending) => pending,
            None => start_read(&read)?,
        };
        match timeout(bound, &mut pending).await {
            Ok(Ok(done)) => {
                done.record(&metrics);
                watch.observe(std::time::Instant::now(), &done, &metrics);
            }
            Ok(Err(_)) => bail!("the storage read stopped without a result"),
            Err(_) => {
                reading = Some(pending);
                watch.unread(&format!("the read has not finished in {bound:?}"));
            }
        }
    }
}

/// Starts `read` on a thread of its own.
/// Error cases:
/// - the thread cannot be started
fn start_read(read: &Arc<impl Fn() -> Reading + Send + Sync + 'static>) -> anyhow::Result<oneshot::Receiver<Reading>> {
    let (done, pending) = oneshot::channel();
    let read = read.clone();
    std::thread::Builder::new()
        .name("storage-read".to_owned())
        .spawn(move || {
            let _ = done.send(read());
        })
        .context("starting the storage read thread")?;
    Ok(pending)
}

#[cfg(test)]
mod tests;
