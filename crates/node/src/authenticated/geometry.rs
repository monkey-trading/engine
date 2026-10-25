//! Read-only SQLite header and sidecar checks before opening the host-controlled cache.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};

/// Refuses malformed geometry, WAL state and any hot rollback journal without recovering it.
/// Error cases:
/// - invalid/truncated header, inconsistent page geometry, nonregular files or recovery sidecars
pub(super) fn check(path: &Path) -> Result<()> {
    inspect(path, false).map(|_| ())
}

/// Checks a store a crash may have left mid-transaction: as `check`, but a hot rollback journal is
/// allowed and reported, so the caller reads the database file as it is and leaves the journal for
/// `Store::enable_writes` to roll back after recovery preflight.
/// Error cases:
/// - as `check`, except for a hot rollback journal
pub(super) fn check_crash_image(path: &Path) -> Result<bool> {
    inspect(path, true)
}

/// Whether a rollback journal SQLite would roll back is present. SQLite writes a journal's header
/// only when it syncs the journal, before changing any database page, so a journal whose first
/// byte is still zero never changed the database and SQLite ignores it.
/// Error cases:
/// - the journal path is not a regular file or cannot be read
pub(super) fn hot_journal(path: &Path) -> Result<bool> {
    let path = sidecar(path, "-journal");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(metadata.file_type().is_file(), "cache has a recovery sidecar that is not a file");
            let mut first = [0u8];
            Ok(File::open(&path)?.read(&mut first)? == 1 && first[0] != 0)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Validates the header and sidecars; returns whether a hot journal is present.
/// Error cases:
/// - as `check`, with a hot journal refused unless `allow_hot_journal`
fn inspect(path: &Path, allow_hot_journal: bool) -> Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.file_type().is_file(), "database is not a regular file");
    let mut file = File::open(path)?;
    let mut header = [0u8; 100];
    file.read_exact(&mut header)?;
    ensure!(&header[..16] == b"SQLite format 3\0", "invalid SQLite header");
    let raw = u16::from_be_bytes([header[16], header[17]]);
    let size = if raw == 1 { 65536u64 } else { u64::from(raw) };
    ensure!((512..=65536).contains(&size) && size.is_power_of_two(), "invalid page size");
    ensure!(header[18] == 1 && header[19] == 1, "WAL database is not a read-only rollback cache");
    ensure!(header[21..24] == [64, 32, 32] && u64::from(header[20]) <= size - 480, "invalid payload geometry");
    let pages = u64::from(word(&header, 28));
    ensure!(pages != 0 && pages.checked_mul(size) == Some(metadata.len()), "invalid page count/file length");
    ensure!(word(&header, 24) == word(&header, 92), "stale SQLite header page count");
    ensure!(u64::from(word(&header, 32)) <= pages && u64::from(word(&header, 36)) < pages, "invalid freelist geometry");
    ensure!((1..=4).contains(&word(&header, 44)), "invalid schema representation");
    ensure!(u64::from(word(&header, 52)) <= pages, "invalid auto-vacuum root page");
    let hot = hot_journal(path)?;
    ensure!(!hot || allow_hot_journal, "cache has a recovery sidecar; preserve it and rebuild after preflight");
    for suffix in ["-wal", "-shm"] {
        let path = sidecar(path, suffix);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_file() && metadata.len() == 0,
                "cache has a recovery sidecar; preserve it and rebuild after preflight"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(hot)
}

/// Appends a SQLite sidecar suffix without interpreting the original filename.
pub(super) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Reads a checked four-byte field from the fixed-size SQLite header.
fn word(header: &[u8; 100], offset: usize) -> u32 {
    u32::from_be_bytes([header[offset], header[offset + 1], header[offset + 2], header[offset + 3]])
}
