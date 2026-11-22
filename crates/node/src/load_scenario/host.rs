//! How long the host's tasks waited on storage and for a CPU, from Linux's pressure stall information
//! (`/proc/pressure`), and the I/O its disks completed (`/sys/block/*/stat`). The storage wait counts
//! the node's own log syncs too, so a disk stall shows as a window's wait standing well above its
//! neighbours'; the I/O counts everything the host read, wrote, discarded and flushed, the node's log
//! and its journal included, and how long each disk's requests took.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

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
/// disk's requests' time in milliseconds, by name (`completed`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Disks {
    pub(super) ops: u64,
    pub(super) bytes: u64,
    pub(super) request_ms: BTreeMap<String, u64>,
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
/// the busiest disk's requests' time, in milliseconds.
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
    /// The host's totals now, over every disk.
    pub(super) fn now() -> Self {
        Waits { io_us: wait_us("io"), cpu_us: wait_us("cpu"), disks: disks(None) }
    }

    /// The host's totals now, over the disks named `only`.
    pub(super) fn now_on(only: &BTreeSet<String>) -> Self {
        Waits { io_us: wait_us("io"), cpu_us: wait_us("cpu"), disks: disks(Some(only)) }
    }

    /// How long the host's tasks waited from `earlier` to this, and what its disks did.
    pub(super) fn since(&self, earlier: &Waits) -> Waited {
        let ms = |now: Option<u64>, then: Option<u64>| {
            now.zip(then).map(|(now, then)| now.saturating_sub(then) as f64 / 1_000.0)
        };
        let disk_io = self.disks.as_ref().zip(earlier.disks.as_ref()).map(|(now, then)| {
            let busy = |(name, busy): (&String, &u64)| then.request_ms.get(name).map(|was| busy.saturating_sub(*was));
            DiskIo {
                ops: now.ops.saturating_sub(then.ops),
                bytes: now.bytes.saturating_sub(then.bytes),
                busiest_ms: now.request_ms.iter().filter_map(busy).max().unwrap_or(0),
            }
        });
        Waited { io_ms: ms(self.io_us, earlier.io_us), cpu_ms: ms(self.cpu_us, earlier.cpu_us), disk_io }
    }
}

/// What the host's disks, or those named `only`, have completed, from each whole disk's `stat`: a
/// partition is counted in its disk, and a device built on others (`dm-`, `md`), memory (`zram`,
/// `ram`) or a file (`loop`) is not a disk of its own.
fn disks(only: Option<&BTreeSet<String>>) -> Option<Disks> {
    let mut disks = Disks::default();
    for disk in std::fs::read_dir("/sys/block").ok()?.filter_map(Result::ok) {
        let name = disk.file_name().to_string_lossy().into_owned();
        if !only.is_none_or(|only| only.contains(&name)) || not_a_disk(&name) {
            continue;
        }
        let (ops, bytes, request_ms) =
            std::fs::read_to_string(disk.path().join("stat")).ok().as_deref().and_then(completed)?;
        (disks.ops, disks.bytes) = (disks.ops + ops, disks.bytes + bytes);
        disks.request_ms.insert(name, request_ms);
    }
    Some(disks)
}

/// Whether a block device named `name` is no disk of its own: one built on others (`dm-`, `md`), memory
/// (`zram`, `ram`) or a file (`loop`).
fn not_a_disk(name: &str) -> bool {
    ["dm-", "md", "zram", "ram", "loop"].iter().any(|built| name.starts_with(built))
}

/// The whole disks that hold the filesystem `path` is on (`whole_disks`), None where the host does not
/// list its device, as for a filesystem in memory, or names no disk under it.
pub(super) fn backing_disks(path: &Path) -> Option<BTreeSet<String>> {
    let mut disks = BTreeSet::new();
    whole_disks(&sysfs_device(path)?, &mut disks);
    (!disks.is_empty()).then_some(disks)
}

/// The sysfs directory of the block device that holds the filesystem `path` is on, from its device
/// number (`/sys/dev/block`); None where the host lists no such device.
fn sysfs_device(path: &Path) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let device = std::fs::metadata(path).ok()?.dev();
    // Linux's device number: the major number in bits 8 to 19 and 32 to 43, the minor in the rest.
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()
}

/// Adds to `disks` the whole disks under `device`, a block device's directory in sysfs: a partition's
/// disk, then the disks under a device built on others (`slaves`), or the device itself, unless it is
/// no disk of its own (`not_a_disk`).
fn whole_disks(device: &Path, disks: &mut BTreeSet<String>) {
    let device = if device.join("partition").exists() { device.parent().unwrap_or(device) } else { device };
    let slaves: Vec<_> = std::fs::read_dir(device.join("slaves"))
        .map(|slaves| slaves.filter_map(|slave| std::fs::canonicalize(slave.ok()?.path()).ok()).collect())
        .unwrap_or_default();
    if !slaves.is_empty() {
        slaves.iter().for_each(|slave| whole_disks(slave, disks));
    } else if let Some(name) = device.file_name().map(|name| name.to_string_lossy().into_owned()) {
        if !not_a_disk(&name) {
            disks.insert(name);
        }
    }
}

/// The I/O completed, the bytes it moved and its requests' time in a block device's `stat`: its reads,
/// writes, discards and flushes (fields 1, 5, 12 and 16), the sectors read, written and discarded (3, 7
/// and 14) at 512 bytes each, and the milliseconds its completed requests took, summed (11), less its
/// flushes' (17). The kernel sums each request's time from its own clock, where the time with I/O in
/// flight (10) may count a whole tick for each short request; a flush may destage writes completed
/// earlier, so it counts for none. A kernel that counts no discards or flushes lists none.
fn completed(stat: &str) -> Option<(u64, u64, u64)> {
    let fields: Vec<u64> = stat.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let field = |number: usize| fields.get(number - 1).copied().unwrap_or(0);
    let ops = field(1) + field(5) + field(12) + field(16);
    let bytes = (field(3) + field(7) + field(14)) * 512;
    (fields.len() >= 11).then(|| (ops, bytes, field(11).saturating_sub(field(17))))
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
    fn a_disks_io_is_every_read_write_discard_and_flush_in_its_stat_and_its_requests_time_less_flushes() {
        let stat = "10521 2087 812040 4127 228716 113921 16312906 188905 0 44753 193032 7 0 4096 3 512 9\n";
        let (ops, bytes) = (10_521 + 228_716 + 7 + 512, (812_040 + 16_312_906 + 4_096) * 512);
        assert_eq!(completed(stat), Some((ops, bytes, 193_032 - 9)), "its requests' time less its flushes'");
        let older = "10521 2087 812040 4127 228716 113921 16312906 188905 0 44753 193032";
        let (ops, bytes) = (10_521 + 228_716, (812_040 + 16_312_906) * 512);
        assert_eq!(completed(older), Some((ops, bytes, 193_032)), "no discards or flushes");
        assert_eq!(completed("1 2 3 4 5 6 7"), None, "a stat too short to hold its requests' time");
    }

    /// A whole disk names itself, a partition its disk, and a device built on others the disks under it,
    /// as sysfs lays them out: `slaves` links to each device it is built on, and a partition's disk is its
    /// parent directory. A device in memory or on a file names no disk.
    #[test]
    fn a_filesystems_backing_disks_are_its_whole_disks_through_partitions_and_built_devices() {
        let sys = crate::test_support::scratch_dir("backing-disks");
        let (nvme, sda1, sdb) = (sys.join("pci/nvme1n1"), sys.join("pci/sda/sda1"), sys.join("pci/sdb"));
        let (dm, md0p1, loop0) = (sys.join("virtual/dm-0"), sys.join("virtual/md0/md0p1"), sys.join("virtual/loop0"));
        for dir in [&nvme, &sda1, &sdb, &dm.join("slaves"), &md0p1, &sys.join("virtual/md0/slaves"), &loop0] {
            std::fs::create_dir_all(dir).expect("a directory");
        }
        for partition in [&sda1, &md0p1] {
            std::fs::write(partition.join("partition"), "1\n").expect("a partition's number");
        }
        let slave = |device: &Path, under: &Path, name: &str| {
            std::os::unix::fs::symlink(device, under.join("slaves").join(name)).expect("a slave");
        };
        slave(&nvme, &dm, "nvme1n1");
        slave(&sda1, &dm, "sda1");
        slave(&sda1, &sys.join("virtual/md0"), "sda1");
        slave(&sdb, &sys.join("virtual/md0"), "sdb");
        let under = |device: &Path| {
            let mut disks = BTreeSet::new();
            whole_disks(device, &mut disks);
            disks.into_iter().collect::<Vec<_>>()
        };
        assert_eq!(under(&nvme), ["nvme1n1"]);
        assert_eq!(under(&sda1), ["sda"], "a partition's disk");
        assert_eq!(under(&dm), ["nvme1n1", "sda"], "the disks under a device built on them");
        assert_eq!(under(&md0p1), ["sda", "sdb"], "a partition of a built device, through its disks");
        assert!(under(&loop0).is_empty(), "a device on a file names no disk");
        // A temporary directory on a block device, as on CI's Linux runners, names the disks under it, or
        // no stall of them could be excused; one in memory names none, and the probe stays off there.
        if sysfs_device(&sys).is_some() {
            assert!(backing_disks(&sys).is_some_and(|disks| !disks.is_empty()), "the disks under {sys:?}");
        }
    }

    /// Only the disks named are read; every disk the host lists otherwise.
    #[test]
    fn a_reading_on_named_disks_reads_only_those() {
        let none = disks(Some(&BTreeSet::new()));
        if std::path::Path::new("/sys/block").exists() {
            assert_eq!(none, Some(Disks::default()), "no disk named, none read");
            let every = disks(None).expect("the host's disks");
            let one: BTreeSet<String> = every.request_ms.keys().take(1).cloned().collect();
            let named = disks(Some(&one)).expect("the named disk");
            assert!(named.request_ms.keys().eq(one.iter()), "{named:?}");
        } else {
            assert_eq!(none, None, "a host that lists no disks reads none");
        }
    }

    #[test]
    fn a_window_waited_what_each_total_grew_by_in_milliseconds_or_none_reported() {
        let disks = |ops, bytes, busy: &[(&str, u64)]| Disks {
            ops,
            bytes,
            request_ms: busy.iter().map(|(name, ms)| ((*name).to_owned(), *ms)).collect(),
        };
        // The busiest disk comes first by name, and one appears only in the later reading.
        let (earlier, later) = (
            Waits {
                io_us: Some(1_000),
                cpu_us: Some(20_000),
                disks: Some(disks(100, 1 << 20, &[("nvme0n1", 900), ("sda", 5_000)])),
            },
            Waits {
                io_us: Some(251_000),
                cpu_us: Some(20_500),
                disks: Some(disks(350, 6 << 20, &[("nvme0n1", 1_140), ("sda", 5_090), ("sdb", 9_000)])),
            },
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
