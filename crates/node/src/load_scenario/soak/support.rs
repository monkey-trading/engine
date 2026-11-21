//! What the soak needs of the machine around the node: the generator's process, the newest cache's
//! size and the API's address again.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::{Instant, sleep};

use crate::load_scenario::durable::Durable;
use crate::load_scenario::soak::hours::dir_bytes;

/// How long the log must hold its size once a node stops, and how long it has to.
const QUIET: Duration = Duration::from_secs(2);
const LOG_QUIET_WITHIN: Duration = Duration::from_secs(60);

/// The generator's process, killed when the run ends however it ends.
pub(super) struct Reaper(pub(super) std::process::Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// The size of the newest cache, which the state's size sets, 0 with none.
pub(super) fn newest_cache(durable: &Durable) -> u64 {
    let section = durable.cfg.snapshots.as_ref().expect("[snapshots]");
    crate::snapshot::candidates_readonly(section)
        .ok()
        .and_then(|paths| paths.into_iter().next())
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or(0, |found| found.len())
}

/// The API's listener on `addr` again, once the stopped node has let go of it.
pub(super) async fn rebind(addr: SocketAddr) -> TcpListener {
    for _ in 0..100 {
        if let Ok(listener) = TcpListener::bind(addr).await {
            return listener;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("{addr} stayed taken after the stopped node");
}

/// Waits until the log in `dir` has held its size for `QUIET`, so nothing of a stopped node still writes it
/// when another starts on it. The node's log-sync thread is not joined by a stop, which a crash leaves
/// with its queued writes.
/// Error cases:
/// - the log is still growing after `LOG_QUIET_WITHIN`
pub(super) async fn log_quiet(dir: &std::path::Path) -> anyhow::Result<()> {
    let (mut last, mut quiet_since) = (dir_bytes(dir), Instant::now());
    let started = Instant::now();
    while quiet_since.elapsed() < QUIET {
        anyhow::ensure!(
            started.elapsed() < LOG_QUIET_WITHIN,
            "the log was still growing {LOG_QUIET_WITHIN:?} after the stop"
        );
        sleep(Duration::from_millis(200)).await;
        let now = dir_bytes(dir);
        if now != last {
            (last, quiet_since) = (now, Instant::now());
        }
    }
    Ok(())
}
