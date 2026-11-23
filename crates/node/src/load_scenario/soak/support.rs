//! What the soak needs of the machine around the node: the generator's process, the newest cache's
//! size and the API's address again.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::sleep;

use crate::load_scenario::durable::Durable;
use crate::load_scenario::report::process;

/// The generator's process, killed when the run ends however it ends.
pub(super) struct Reaper(pub(super) std::process::Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

/// What a sample of the process's memory measured, as the report names it.
pub(super) const FOOTPRINT: &str = "physical_footprint_mib";
pub(super) const RESIDENT: &str = "resident_mib";

/// The process's memory in MiB, and which measure it is: on macOS its physical footprint, which counts the
/// pages the system has compressed that resident memory leaves out (on a loaded machine the two differ by
/// several times); where the platform reports none, or `footprint` fails, its resident memory.
pub(super) fn memory_mib() -> (u64, &'static str) {
    let footprint = cfg!(target_os = "macos")
        .then(|| std::process::Command::new("footprint").args(["-p", &std::process::id().to_string()]).output().ok())
        .flatten()
        .and_then(|out| parse_footprint_mib(&String::from_utf8_lossy(&out.stdout)));
    footprint.map_or_else(|| (process().1, RESIDENT), |mib| (mib, FOOTPRINT))
}

/// The footprint in MiB that `footprint -p` prints on its first line, `Footprint: 1936 KB (...)`, in bytes,
/// KB, MB or GB.
fn parse_footprint_mib(text: &str) -> Option<u64> {
    let mut words = text.lines().find_map(|line| line.split_once("Footprint:"))?.1.split_whitespace();
    let amount: f64 = words.next()?.parse().ok()?;
    let bytes = match words.next()? {
        "B" | "bytes" => 1.0,
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        "GB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((amount * bytes / (1024.0 * 1024.0)) as u64)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_footprint_is_read_from_its_first_line_in_whatever_unit_it_prints() {
        let line =
            |amount: &str| format!("zsh [49356]: 64-bit    Footprint: {amount} (16384 bytes per page)\n\n  Dirty ...");
        assert_eq!(parse_footprint_mib(&line("1936 KB")), Some(1));
        assert_eq!(parse_footprint_mib(&line("2048 KB")), Some(2), "a kilobyte is 1024 bytes");
        assert_eq!(parse_footprint_mib(&line("480 MB")), Some(480));
        assert_eq!(parse_footprint_mib(&line("1.5 GB")), Some(1_536));
        assert_eq!(parse_footprint_mib(&line("2097152 B")), Some(2));
        assert_eq!(parse_footprint_mib(&line("12 furlongs")), None, "an unknown unit is no figure");
        assert_eq!(parse_footprint_mib("no such process"), None);
    }

    #[test]
    fn a_memory_sample_names_its_measure() {
        let (mib, measure) = memory_mib();
        assert!(mib > 0, "the test process holds memory");
        assert!(matches!(measure, FOOTPRINT | RESIDENT));
        if cfg!(not(target_os = "macos")) {
            assert_eq!(measure, RESIDENT, "only macOS reports a footprint");
        }
    }
}
