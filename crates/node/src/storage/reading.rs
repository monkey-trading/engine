//! One read of every store's bytes and every volume's space.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::PoisonError;

use nix::sys::statvfs::statvfs;

use crate::log::{segment_name, segment_seq};
use crate::metrics::{Metrics, StoreLabel, VolumeLabel};
use crate::storage::layout::{Layout, Store};

/// A data volume, named by the first store on it, in `Store` order: `log` for the log's, and `history_index` for the
/// one the history directory is on, whichever optional stores share them.
#[derive(Debug, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    /// The stores whose first directory is on it, which name it.
    pub stores: Vec<Store>,
    /// The bytes each store's files on it take, a store's other directories included.
    pub holds: Vec<(Store, u64)>,
    pub total_bytes: u64,
    /// What the node may still write: the host's available blocks, which leave out the filesystem's own reserve.
    pub free_bytes: u64,
}

/// What a read found.
#[derive(Debug, Default)]
pub struct Reading {
    /// The bytes the disk allocates to each store's files, in `Store` order, for the stores whose every directory read.
    pub stores: Vec<(Store, u64)>,
    pub volumes: Vec<Volume>,
    /// Each store that could not be read, with why: its directory's walk or its volume's size.
    pub failures: Vec<(Store, String)>,
}

/// What a read has found of one device: the stores whose first directory is on it, the bytes of each store's files on
/// it, and its size and available bytes, none when they could not be read.
#[derive(Default)]
pub(super) struct Device {
    pub(super) stores: Vec<Store>,
    pub(super) holds: Vec<(Store, u64)>,
    pub(super) space: Option<(u64, u64)>,
}

impl Layout {
    /// Reads every store and volume. A store whose directory does not exist yet holds nothing. A store with a
    /// directory that cannot be walked has no size in the reading, so its metric keeps the last size read, and no entry
    /// in its volume's `holds`, but still names and sizes the volume it is on. A volume
    /// whose size cannot be read is left out, and a directory whose device cannot be found leaves out every volume,
    /// since its store might name one.
    pub fn read(&self) -> Reading {
        let mut reading = Reading::default();
        let mut devices: BTreeMap<u64, Device> = BTreeMap::new();
        let mut device_unread = false;
        for (store, dirs) in self.stores() {
            let mut bytes = 0u64;
            let mut readable = true;
            // A store sits on the volume of its first directory; its others only add their bytes to the volume
            // they are on.
            for (index, (dir, pick)) in dirs.into_iter().enumerate() {
                let walked = if store == Store::Log { self.walk_log(dir) } else { walk(dir, pick, true) };
                // A failed walk leaves out the store's bytes only: its volume is still named and judged.
                let found = match walked {
                    Ok(found) => {
                        bytes = bytes.saturating_add(found);
                        Some(found)
                    }
                    Err(error) => {
                        readable = false;
                        reading.failures.push((store, format!("reading {}: {error}", dir.display())));
                        None
                    }
                };
                let device = match device_of(dir) {
                    Ok(device) => devices.entry(device).or_default(),
                    Err(error) => {
                        device_unread = true;
                        reading.failures.push((store, format!("reading the volume of {}: {error}", dir.display())));
                        continue;
                    }
                };
                if let Some(found) = found {
                    match device.holds.iter_mut().find(|(held, _)| *held == store) {
                        Some((_, held)) => *held = held.saturating_add(found),
                        None => device.holds.push((store, found)),
                    }
                }
                if index == 0 {
                    device.stores.push(store);
                }
                if device.space.is_none() {
                    match space_of(dir) {
                        Ok(space) => device.space = Some(space),
                        Err(error) => reading
                            .failures
                            .push((store, format!("reading the space of the volume of {}: {error}", dir.display()))),
                    }
                }
            }
            if readable {
                reading.stores.push((store, bytes));
            }
        }
        if !device_unread {
            reading.volumes = name_volumes(devices);
        }
        reading
    }
}

impl Reading {
    /// Sets the figures as metrics.
    pub fn record(&self, metrics: &Metrics) {
        for (store, bytes) in &self.stores {
            metrics.storage.store_bytes.get_or_create(&StoreLabel { store: store.name() }).set(gauge(*bytes));
        }
        for volume in &self.volumes {
            let label = VolumeLabel { volume: volume.name.clone() };
            metrics.storage.volume_total_bytes.get_or_create(&label).set(gauge(volume.total_bytes));
            metrics.storage.volume_free_bytes.get_or_create(&label).set(gauge(volume.free_bytes));
        }
    }
}

impl Layout {
    /// The bytes allocated to the log directory's files. Segments are append-only and a new one starts every
    /// `node.segment_commands` commands, so a log has as many files as it has had thousands of commands, and statting
    /// each every read would grow without a bound: each segment but the two newest is statted once and its size kept.
    /// A segment's size is not read again while its file is listed, so a log cut back and regrown under the same
    /// names between two reads is miscounted by up to a segment, until a later read finds the name gone.
    /// Error cases:
    /// - the directory cannot be listed, or a file's metadata cannot be read, for a reason other than being absent
    fn walk_log(&self, dir: &Path) -> std::io::Result<u64> {
        let mut sealed = self.sealed.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                sealed.clear();
                return Ok(0);
            }
            Err(error) => return Err(error),
        };
        let (mut bytes, mut segments) = (0u64, Vec::new());
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            match entry.file_name().to_str().and_then(segment_seq) {
                Some(seq) => segments.push(seq),
                None => bytes = bytes.saturating_add(entry_bytes(&entry.path())?),
            }
        }
        segments.sort_unstable();
        let sealed_count = segments.len().saturating_sub(2);
        sealed.retain(|seq, _| segments[..sealed_count].binary_search(seq).is_ok());
        for (index, seq) in segments.iter().enumerate() {
            let size = match sealed.get(seq) {
                Some(size) if index < sealed_count => *size,
                _ => entry_bytes(&dir.join(segment_name(*seq)))?,
            };
            if index < sealed_count {
                sealed.insert(*seq, size);
            }
            bytes = bytes.saturating_add(size);
        }
        Ok(bytes)
    }
}

/// A byte count as a gauge, which holds a signed integer.
fn gauge(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
}

/// The volumes of the devices whose size was read and that a store's first directory is on, each named by its first
/// store, in `Store` order, so no two volumes share a name.
pub(super) fn name_volumes(devices: BTreeMap<u64, Device>) -> Vec<Volume> {
    let mut volumes: Vec<Volume> = devices
        .into_values()
        .filter(|device| !device.stores.is_empty())
        .filter_map(|Device { mut stores, holds, space }| {
            let (total_bytes, free_bytes) = space?;
            stores.sort();
            Some(Volume { name: stores[0].name().to_owned(), stores, holds, total_bytes, free_bytes })
        })
        .collect();
    volumes.sort_by(|a, b| a.stores.cmp(&b.stores));
    volumes
}

/// The bytes the disk allocates to the files under `dir`, among its own entries those `pick` names when `top`; a
/// symlink counts as itself and is not followed. A directory that does not exist, or a file removed during the
/// walk, holds nothing.
/// Error cases:
/// - a directory cannot be listed, or a file's metadata cannot be read, for a reason other than being absent
fn walk(dir: &Path, pick: fn(&str) -> bool, top: bool) -> std::io::Result<u64> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut bytes = 0u64;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if top && !entry.file_name().to_str().is_some_and(pick) {
            continue;
        }
        bytes = bytes.saturating_add(entry_bytes(&entry.path())?);
    }
    Ok(bytes)
}

/// The bytes the disk allocates to the file at `path`, or to everything under it for a directory; none for a file
/// removed meanwhile. A symlink counts as itself.
/// Error cases:
/// - the metadata or a directory under `path` cannot be read, for a reason other than being absent
fn entry_bytes(path: &Path) -> std::io::Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => walk(path, |_| true, false),
        // 512-byte blocks, as `du` counts them: a sparse or preallocated file is what the disk holds.
        Ok(metadata) => Ok(metadata.blocks().saturating_mul(512)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// The device a write to `dir` lands on: its nearest existing ancestor's.
/// Error cases:
/// - the metadata cannot be read
fn device_of(dir: &Path) -> std::io::Result<u64> {
    Ok(fs::metadata(existing(dir))?.dev())
}

/// The size and available bytes of the volume a write to `dir` lands on.
/// Error cases:
/// - the volume's statistics cannot be read, or it reports no size
fn space_of(dir: &Path) -> std::io::Result<(u64, u64)> {
    let stats = statvfs(existing(dir)).map_err(std::io::Error::from)?;
    // The counts' widths differ by platform; `as` widens them all to u64.
    let block = stats.fragment_size() as u64;
    let bytes = |count: u64| count.saturating_mul(block);
    let total = bytes(stats.blocks() as u64);
    if total == 0 {
        return Err(std::io::Error::other("the filesystem reports no size"));
    }
    Ok((total, bytes(stats.blocks_available() as u64)))
}

/// `dir`, or its nearest existing ancestor.
fn existing(dir: &Path) -> &Path {
    dir.ancestors().find(|ancestor| ancestor.exists()).unwrap_or(dir)
}
