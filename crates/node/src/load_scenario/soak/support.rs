//! What the soak needs of the machine around the node: the generator's process, the newest cache's
//! size and the API's address again.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::sleep;

use crate::load_scenario::durable::Durable;

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
