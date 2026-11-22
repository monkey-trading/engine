//! How long the host's tasks waited on storage and for a CPU, from Linux's pressure stall information
//! (`/proc/pressure`), and the I/O its disks completed (`/sys/block/*/stat`). The storage wait counts
//! the node's own log syncs too, so a disk stall shows as a window's wait standing well above its
//! neighbours'; the I/O counts everything the host read, wrote, discarded and flushed, the node's log
//! and its journal included, and how long each disk had I/O in flight.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The host's wait totals at one moment, in microseconds, and its disks' counts, each None where the
/// host reports none.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Waits {
    pub(super) io_us: Option<u64>,
    pub(super) cpu_us: Option<u64>,
    pub(super) disks: Option<Disks>,
}

/// What the host's disks had completed at one moment: their I/O and the bytes it moved, summed, and each
/// disk's time with I/O in flight in milliseconds, by name.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Disks {
    pub(super) ops: u64,
    pub(super) bytes: u64,
    pub(super) busy_ms: BTreeMap<String, u64>,
}

/// How long the host's tasks waited between two `Waits`, in milliseconds: on storage, the node's own
/// log syncs included, and for a CPU, and what its disks did. Beside the batch time, a window that
/// breaks its bar with far more of either wait than its neighbours names the disk or the cores.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Waited {
    pub(super) io_ms: Option<f64>,
    pub(super) cpu_ms: Option<f64>,
    pub(super) disk_io: Option<DiskIo>,
}

/// What the host's disks did between two `Waits`: the I/O they completed and the bytes it moved, and
/// the busiest disk's time with I/O in flight, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct DiskIo {
    pub(super) ops: u64,
    pub(super) bytes: u64,
    pub(super) busiest_ms: u64,
}

/// `host_io_wait_ms=… host_cpu_wait_ms=… host_disk_io=…/…MiB/…ms`, `n/a` where the host reports none.
impl fmt::Display for Waited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |wait: Option<f64>| wait.map_or_else(|| "n/a".to_owned(), |ms| format!("{ms:.0}"));
        let io = self
            .disk_io
            .map_or_else(|| "n/a".to_owned(), |io| format!("{}/{}MiB/{}ms", io.ops, io.bytes >> 20, io.busiest_ms));
        write!(f, "host_io_wait_ms={} host_cpu_wait_ms={} host_disk_io={io}", ms(self.io_ms), ms(self.cpu_ms))
    }
}

impl Waits {
    /// The host's totals now.
    pub(super) fn now() -> Self {
        Waits { io_us: wait_us("io"), cpu_us: wait_us("cpu"), disks: disks() }
    }

    /// How long the host's tasks waited from `earlier` to this, and what its disks did.
    pub(super) fn since(&self, earlier: &Waits) -> Waited {
        let ms = |now: Option<u64>, then: Option<u64>| {
            now.zip(then).map(|(now, then)| now.saturating_sub(then) as f64 / 1_000.0)
        };
        let disk_io = self.disks.as_ref().zip(earlier.disks.as_ref()).map(|(now, then)| {
            let busy = |(name, busy): (&String, &u64)| then.busy_ms.get(name).map(|was| busy.saturating_sub(*was));
            DiskIo {
                ops: now.ops.saturating_sub(then.ops),
                bytes: now.bytes.saturating_sub(then.bytes),
                busiest_ms: now.busy_ms.iter().filter_map(busy).max().unwrap_or(0),
            }
        });
        Waited { io_ms: ms(self.io_us, earlier.io_us), cpu_ms: ms(self.cpu_us, earlier.cpu_us), disk_io }
    }
}

/// What every disk of the host has completed, from each whole disk's `stat`: a partition is counted in
/// its disk, and a device built on others (`dm-`, `md`), memory (`zram`, `ram`) or a file (`loop`) is
/// not a disk of its own.
fn disks() -> Option<Disks> {
    let mut disks = Disks::default();
    for disk in std::fs::read_dir("/sys/block").ok()?.filter_map(Result::ok) {
        let name = disk.file_name().to_string_lossy().into_owned();
        if ["dm-", "md", "zram", "ram", "loop"].iter().any(|virtual_disk| name.starts_with(virtual_disk)) {
            continue;
        }
        let (ops, bytes, busy_ms) =
            std::fs::read_to_string(disk.path().join("stat")).ok().as_deref().and_then(completed)?;
        (disks.ops, disks.bytes) = (disks.ops + ops, disks.bytes + bytes);
        disks.busy_ms.insert(name, busy_ms);
    }
    Some(disks)
}

/// The I/O completed, the bytes it moved and the time with I/O in flight in a block device's `stat`:
/// its reads, writes, discards and flushes (fields 1, 5, 12 and 16), the sectors read, written and
/// discarded (3, 7 and 14) at 512 bytes each, and its milliseconds doing I/O (10). A kernel that counts
/// no discards or flushes lists none.
fn completed(stat: &str) -> Option<(u64, u64, u64)> {
    let fields: Vec<u64> = stat.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let field = |number: usize| fields.get(number - 1).copied().unwrap_or(0);
    let ops = field(1) + field(5) + field(12) + field(16);
    (fields.len() >= 11).then(|| (ops, (field(3) + field(7) + field(14)) * 512, field(10)))
}

/// How long any task on the host has waited for `resource` (`io` or `cpu`), in microseconds.
fn wait_us(resource: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/proc/pressure/{resource}")).ok().as_deref().and_then(some_total_us)
}

/// The `total` of the `some` line of a pressure stall file: the time at least one task stalled.
fn some_total_us(text: &str) -> Option<u64> {
    let some = text.lines().find(|line| line.starts_with("some "))?;
    some.split_whitespace().find_map(|field| field.strip_prefix("total="))?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_is_the_pressure_files_some_total() {
        let pressure = "some avg10=1.50 avg60=0.40 avg300=0.10 total=412345\n\
                        full avg10=1.20 avg60=0.30 avg300=0.08 total=365000\n";
        assert_eq!(some_total_us(pressure), Some(412_345), "the some line's total, not the full line's");
        assert_eq!(some_total_us("full avg10=0.00 avg60=0.00 avg300=0.00 total=9\n"), None);
        assert_eq!(some_total_us(""), None);
    }

    #[test]
    fn a_disks_io_is_every_read_write_discard_and_flush_in_its_stat_and_its_time_doing_io() {
        let stat = "10521 2087 812040 4127 228716 113921 16312906 188905 0 44753 193032 7 0 4096 3 512 9\n";
        let (ops, bytes) = (10_521 + 228_716 + 7 + 512, (812_040 + 16_312_906 + 4_096) * 512);
        assert_eq!(completed(stat), Some((ops, bytes, 44_753)));
        let older = "10521 2087 812040 4127 228716 113921 16312906 188905 0 44753 193032";
        let (ops, bytes) = (10_521 + 228_716, (812_040 + 16_312_906) * 512);
        assert_eq!(completed(older), Some((ops, bytes, 44_753)), "no discards or flushes");
        assert_eq!(completed("1 2 3 4 5 6 7"), None, "a stat too short to hold the time doing I/O");
    }

    #[test]
    fn a_window_waited_what_each_total_grew_by_in_milliseconds_or_none_reported() {
        let disks = |ops, bytes, sda, nvme| Disks {
            ops,
            bytes,
            busy_ms: BTreeMap::from([("sda".to_owned(), sda), ("nvme0n1".to_owned(), nvme)]),
        };
        let (earlier, later) = (
            Waits { io_us: Some(1_000), cpu_us: Some(20_000), disks: Some(disks(100, 1 << 20, 5_000, 900)) },
            Waits { io_us: Some(251_000), cpu_us: Some(20_500), disks: Some(disks(350, 6 << 20, 5_240, 990)) },
        );
        let waited = later.since(&earlier);
        let disk_io = DiskIo { ops: 250, bytes: 5 << 20, busiest_ms: 240 };
        assert_eq!(
            waited,
            Waited { io_ms: Some(250.0), cpu_ms: Some(0.5), disk_io: Some(disk_io) },
            "the busiest disk"
        );
        let line = "host_io_wait_ms=250 host_cpu_wait_ms=0 host_disk_io=250/5MiB/240ms";
        assert_eq!(waited.to_string(), line, "rounded");
        for (now, then) in [(later.clone(), Waits::default()), (Waits::default(), later)] {
            let waited = now.since(&then);
            assert_eq!(waited, Waited::default(), "a total the host did not report waits nothing");
            assert_eq!(waited.to_string(), "host_io_wait_ms=n/a host_cpu_wait_ms=n/a host_disk_io=n/a");
        }
    }
}
