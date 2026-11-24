//! One read of every store's bytes and every volume's space.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::PoisonError;

use nix::sys::statvfs::statvfs;

use crate::log::segment_seq;
use crate::metrics::{Metrics, StoreLabel, VolumeLabel};
use crate::storage::layout::{Layout, Store};

/// A data volume, named by the first store on it, in `Store` order: `log` for the log's, and `history_index` for the
/// one the history directory is on, whichever optional stores share them.
#[derive(Debug, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub stores: Vec<Store>,
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

impl Layout {
    /// Reads every store and volume. A store whose directory does not exist yet holds nothing. A store with a
    /// directory that cannot be read has no size in the reading, so its metric keeps the last size read, and a read
    /// in which any volume cannot be found reports no volumes, since a volume's name is the stores on it.
    pub fn read(&self) -> Reading {
        let mut reading = Reading::default();
        let mut devices: BTreeMap<u64, (Vec<Store>, u64, u64)> = BTreeMap::new();
        let mut volume_unread = false;
        for (store, dirs) in self.stores() {
            let mut bytes = 0u64;
            let mut readable = true;
            for (dir, pick) in dirs {
                let walked = if store == Store::Log { self.walk_log(dir) } else { walk(dir, pick, true) };
                match walked {
                    Ok(found) => bytes = bytes.saturating_add(found),
                    Err(error) => {
                        readable = false;
                        reading.failures.push((store, format!("reading {}: {error}", dir.display())));
                    }
                }
                match volume_of(dir) {
                    Ok((device, total, free)) => {
                        let (stores, ..) = devices.entry(device).or_insert((Vec::new(), total, free));
                        if !stores.contains(&store) {
                            stores.push(store);
                        }
                    }
                    Err(error) => {
                        volume_unread = true;
                        reading.failures.push((store, format!("reading the volume of {}: {error}", dir.display())));
                    }
                }
            }
            if readable {
                reading.stores.push((store, bytes));
            }
        }
        if !volume_unread {
            reading.volumes = devices
                .into_values()
                .map(|(mut stores, total_bytes, free_bytes)| {
                    stores.sort();
                    let name = stores[0].name().to_owned();
                    Volume { name, stores, total_bytes, free_bytes }
                })
                .collect();
            reading.volumes.sort_by(|a, b| a.stores.cmp(&b.stores));
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

/// A byte count as a gauge, which holds a signed integer.
fn gauge(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
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
                Some(seq) => segments.push((seq, entry.path())),
                None => bytes = bytes.saturating_add(entry_bytes(&entry.path())?),
            }
        }
        segments.sort_unstable_by_key(|(seq, _)| *seq);
        let sealed_count = segments.len().saturating_sub(2);
        let mut kept = BTreeMap::new();
        for (index, (seq, path)) in segments.iter().enumerate() {
            let size = match sealed.get(seq) {
                Some(size) if index < sealed_count => *size,
                _ => entry_bytes(path)?,
            };
            if index < sealed_count {
                kept.insert(*seq, size);
            }
            bytes = bytes.saturating_add(size);
        }
        *sealed = kept;
        Ok(bytes)
    }
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

/// The device, size and available bytes of the volume a write to `dir` lands on: its nearest existing ancestor's.
/// Error cases:
/// - the volume's metadata or statistics cannot be read
fn volume_of(dir: &Path) -> std::io::Result<(u64, u64, u64)> {
    let existing = dir.ancestors().find(|ancestor| ancestor.exists()).unwrap_or(dir);
    let device = fs::metadata(existing)?.dev();
    let stats = statvfs(existing).map_err(std::io::Error::from)?;
    // The counts' widths differ by platform; `as` widens them all to u64.
    let block = stats.fragment_size() as u64;
    let bytes = |count: u64| count.saturating_mul(block);
    let total = bytes(stats.blocks() as u64);
    if total == 0 {
        return Err(std::io::Error::other("the filesystem reports no size"));
    }
    Ok((device, total, bytes(stats.blocks_available() as u64)))
}
