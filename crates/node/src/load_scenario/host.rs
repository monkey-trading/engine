//! How long the host's tasks waited on storage and for a CPU, from Linux's pressure stall information
//! (`/proc/pressure`), and the I/O its disks completed (`/sys/block/*/stat`). The storage wait counts
//! the node's own log syncs too, so a disk stall shows as a window's wait standing well above its
//! neighbours'; the I/O counts everything the host read, wrote, discarded and flushed, the node's log
//! and its journal included, and the flushes apart.

use std::collections::BTreeSet;
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

/// What the host's disks had completed at one moment, summed: their I/O, the bytes it moved and their
/// flushes (`completed`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Disks {
    pub(super) ops: u64,
    pub(super) bytes: u64,
    pub(super) flushes: u64,
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

/// What the host's disks did between two `Waits`: the I/O they completed, the bytes it moved and the
/// flushes among it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct DiskIo {
    pub(super) ops: u64,
    pub(super) bytes: u64,
    pub(super) flushes: u64,
}

/// `host_io_wait_ms=… host_cpu_wait_ms=… host_disk_io=…/…MiB/…flushes`, `n/a` where the host reports
/// none.
impl fmt::Display for Waited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = |wait: Option<f64>| wait.map_or_else(|| "n/a".to_owned(), |ms| format!("{ms:.0}"));
        let io = self
            .disk_io
            .map_or_else(|| "n/a".to_owned(), |io| format!("{}/{}MiB/{}flushes", io.ops, io.bytes >> 20, io.flushes));
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
        let disk_io = self.disks.zip(earlier.disks).map(|(now, then)| DiskIo {
            ops: now.ops.saturating_sub(then.ops),
            bytes: now.bytes.saturating_sub(then.bytes),
            flushes: now.flushes.saturating_sub(then.flushes),
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
        let (ops, bytes, flushes) =
            std::fs::read_to_string(disk.path().join("stat")).ok().as_deref().and_then(completed)?;
        (disks.ops, disks.bytes, disks.flushes) = (disks.ops + ops, disks.bytes + bytes, disks.flushes + flushes);
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
    let (major, minor) = major_minor(std::fs::metadata(path).ok()?.dev());
    std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).ok()
}

/// The major and minor numbers of Linux's device number `device`: the major in its bits 8 to 19 and 32
/// to 63, the minor in bits 0 to 7 and 20 to 31.
fn major_minor(device: u64) -> (u64, u64) {
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    (major, minor)
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
    } else if let Some(name) = device.file_name().map(|name| name.to_string_lossy().into_owned())
        && !not_a_disk(&name)
    {
        disks.insert(name);
    }
}

/// The I/O completed, the bytes it moved and the flushes in a block device's `stat`: its reads, writes,
/// discards and flushes (fields 1, 5, 12 and 16), the sectors read, written and discarded (3, 7 and 14)
/// at 512 bytes each, and its flushes (16). None from a kernel that counts no flushes, since a reading
/// that may hide one cannot show the disks flushed nothing.
fn completed(stat: &str) -> Option<(u64, u64, u64)> {
    let fields: Vec<u64> = stat.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
    let field = |number: usize| fields[number - 1];
    (fields.len() >= 17).then(|| {
        let ops = field(1) + field(5) + field(12) + field(16);
        (ops, (field(3) + field(7) + field(14)) * 512, field(16))
    })
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
    fn a_disks_io_is_every_read_write_discard_and_flush_in_its_stat_with_its_flushes() {
        let stat = "10521 2087 812040 4127 228716 113921 16312906 188905 0 44753 193032 7 0 4096 3 512 9\n";
        let (ops, bytes) = (10_521 + 228_716 + 7 + 512, (812_040 + 16_312_906 + 4_096) * 512);
        assert_eq!(completed(stat), Some((ops, bytes, 512)), "its flushes apart");
        let older = "10521 2087 812040 4127 228716 113921 16312906 188905 0 44753 193032 7 0 4096 3";
        assert_eq!(completed(older), None, "a kernel that counts no flushes");
        assert_eq!(completed("1 2 3 x 5 6 7 8 9 10 11 12 13 14 15 16 17"), None, "a field that is no count");
    }

    /// Linux's device numbers, as `makedev` lays them out: the large major and minor numbers split
    /// around the small ones.
    #[test]
    fn a_device_number_splits_into_its_major_and_minor_numbers() {
        let devices = [(0x801, (8, 1)), (0x1_0300, (259, 0)), (0x10_082c, (8, 300)), (0x1000_0000_0400, (0x1004, 0))];
        for (device, numbers) in devices {
            assert_eq!(major_minor(device), numbers, "{device:#x}");
        }
        #[cfg(target_os = "linux")]
        for (major, minor) in [(8, 1), (259, 0), (8, 300), (0x1004, 0), (0xf_ffff, 0xffff_ffff)] {
            let device = nix::libc::makedev(major, minor);
            assert_eq!(major_minor(device), (u64::from(major), u64::from(minor)), "libc's {major}:{minor}");
        }
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
        filesystem_on_a_block_device_names_its_disks(&sys);
    }

    /// A directory on a block device, as CI's Linux runners' are, names the disks under it, or no stall of
    /// them could be excused, unless the device is one in memory, on a file or built on those, whose stall
    /// excuses nothing; one on no block device (major number 0), as in memory, is not checked.
    #[cfg(target_os = "linux")]
    fn filesystem_on_a_block_device_names_its_disks(dir: &Path) {
        use std::os::unix::fs::MetadataExt;
        let device = std::fs::metadata(dir).expect("the directory").dev();
        let (major, minor) = (nix::libc::major(device), nix::libc::minor(device));
        if major == 0 {
            return;
        }
        let listed = std::fs::canonicalize(format!("/sys/dev/block/{major}:{minor}")).expect("sysfs lists it");
        assert_eq!(sysfs_device(dir), Some(listed.clone()), "the device under {dir:?}");
        let disk = if listed.join("partition").exists() { listed.parent().expect("a disk") } else { &listed };
        let name = disk.file_name().expect("a name").to_string_lossy();
        match backing_disks(dir) {
            Some(disks) => assert!(!disks.is_empty(), "the disks under {dir:?}"),
            None => assert!(not_a_disk(&name), "no disk under {dir:?}, on {name}"),
        }
    }

    /// Only Linux lists block devices.
    #[cfg(not(target_os = "linux"))]
    fn filesystem_on_a_block_device_names_its_disks(dir: &Path) {
        assert_eq!(sysfs_device(dir), None, "no sysfs");
    }

    /// Only the disks named are read; every disk the host lists otherwise.
    #[test]
    fn a_reading_on_named_disks_reads_only_those() {
        let none = disks(Some(&BTreeSet::new()));
        let Ok(listed) = std::fs::read_dir("/sys/block") else {
            assert_eq!(none, None, "a host that lists no disks reads none");
            return;
        };
        assert_eq!(none, Some(Disks::default()), "no disk named, none read");
        let names: BTreeSet<String> = listed
            .filter_map(|disk| Some(disk.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|name| !not_a_disk(name))
            .collect();
        // A disk's counts only grow, so a reading taken between two of every disk lies between them, and
        // one disk's falls below the later.
        let within =
            |low: &Disks, high: &Disks| low.ops <= high.ops && low.bytes <= high.bytes && low.flushes <= high.flushes;
        let before = disks(None).expect("the host's disks");
        let named = disks(Some(&names)).expect("every disk, named");
        let one = names.first().map(|one| disks(Some(&BTreeSet::from([one.clone()]))).expect("the named disk"));
        let after = disks(None).expect("the host's disks again");
        assert!(within(&before, &named) && within(&named, &after), "{before:?} {named:?} {after:?}");
        assert!(one.is_none_or(|one| within(&one, &after)), "{one:?} {after:?}");
    }

    #[test]
    fn a_window_waited_what_each_total_grew_by_in_milliseconds_or_none_reported() {
        let disks = |ops, bytes, flushes| Some(Disks { ops, bytes, flushes });
        let (earlier, later) = (
            Waits { io_us: Some(1_000), cpu_us: Some(20_000), disks: disks(100, 1 << 20, 4) },
            Waits { io_us: Some(251_000), cpu_us: Some(20_500), disks: disks(350, 6 << 20, 6) },
        );
        let waited = later.since(&earlier);
        let disk_io = DiskIo { ops: 250, bytes: 5 << 20, flushes: 2 };
        assert_eq!(waited, Waited { io_ms: Some(250.0), cpu_ms: Some(0.5), disk_io: Some(disk_io) });
        let line = "host_io_wait_ms=250 host_cpu_wait_ms=0 host_disk_io=250/5MiB/2flushes";
        assert_eq!(waited.to_string(), line, "rounded");
        for (now, then) in [(later.clone(), Waits::default()), (Waits::default(), later)] {
            let waited = now.since(&then);
            assert_eq!(waited, Waited::default(), "a total the host did not report waits nothing");
            assert_eq!(waited.to_string(), "host_io_wait_ms=n/a host_cpu_wait_ms=n/a host_disk_io=n/a");
        }
    }
}
