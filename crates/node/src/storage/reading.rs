//! One read of every store's bytes and every volume's space.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use nix::sys::statvfs::statvfs;

use super::layout::{Layout, Store};
use crate::metrics::{Metrics, StoreLabel, VolumeLabel};

/// A data volume, named by the stores on it joined with `+`.
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
    /// The bytes the disk allocates to each store's files, in `Store` order.
    pub stores: Vec<(Store, u64)>,
    pub volumes: Vec<Volume>,
    /// Each store that could not be read, with why: its directory's walk or its volume's size.
    pub failures: Vec<(Store, String)>,
}

impl Layout {
    /// Reads every store and volume. A store whose directory does not exist yet holds nothing.
    pub fn read(&self) -> Reading {
        let mut reading = Reading::default();
        let mut devices: BTreeMap<u64, (Vec<Store>, u64, u64)> = BTreeMap::new();
        for (store, dirs) in self.stores() {
            let mut bytes = 0u64;
            for (dir, pick) in dirs {
                match walk(dir, pick, true) {
                    Ok(found) => bytes = bytes.saturating_add(found),
                    Err(error) => reading.failures.push((store, format!("reading {}: {error}", dir.display()))),
                }
                match volume_of(dir) {
                    Ok((device, total, free)) => {
                        let (stores, ..) = devices.entry(device).or_insert((Vec::new(), total, free));
                        if !stores.contains(&store) {
                            stores.push(store);
                        }
                    }
                    Err(error) => {
                        reading.failures.push((store, format!("reading the volume of {}: {error}", dir.display())))
                    }
                }
            }
            reading.stores.push((store, bytes));
        }
        reading.volumes = devices
            .into_values()
            .map(|(mut stores, total_bytes, free_bytes)| {
                stores.sort();
                let name = stores.iter().map(|store| store.name()).collect::<Vec<_>>().join("+");
                Volume { name, stores, total_bytes, free_bytes }
            })
            .collect();
        reading.volumes.sort_by(|a, b| a.stores.cmp(&b.stores));
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

fn gauge(bytes: u64) -> i64 {
    i64::try_from(bytes).unwrap_or(i64::MAX)
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
        let metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        bytes = bytes.saturating_add(if metadata.is_dir() {
            walk(&entry.path(), pick, false)?
        } else {
            // 512-byte blocks, as `du` counts them: a sparse or preallocated file is what the disk holds.
            metadata.blocks().saturating_mul(512)
        });
    }
    Ok(bytes)
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
