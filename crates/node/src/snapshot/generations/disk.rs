//! Fixed-size locator codec and durable filesystem operations; journal data is never root authority.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::snapshot::head::StoreId;

const JOURNAL: &str = "authenticated-generations.v1";
const NEXT: &str = "authenticated-generations.v1.next";
const LOCK: &str = "authenticated-generations.lock";
const MAGIC: &[u8; 8] = b"EGNLC01\0";
// Four optional 32-byte identifiers, each preceded by one tag, plus the eight-byte format magic.
const ENCODED: usize = 8 + 4 * 33;
const SUFFIXES: [&str; 6] = ["", ".lock", "-journal", "-wal", "-shm", ".journal"];

#[derive(Clone, Default, Eq, PartialEq)]
pub(super) struct State {
    pub staging: [Option<StoreId>; 2],
    pub retiring: [Option<StoreId>; 2],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Point {
    DirectoryOpen,
    DirectorySync,
    JournalCreate,
    JournalWrite,
    JournalSync,
    JournalRename,
    Remove,
    StoreCreate,
}

pub(super) struct Disk {
    directory: PathBuf,
    #[cfg(test)]
    fault: std::cell::Cell<Option<(Point, usize)>>,
}

impl State {
    /// Indicates that no allocation or last-reference-removal attempt remains outstanding.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.staging.iter().chain(&self.retiring).all(Option::is_none)
    }

    /// Encodes only fixed-width locators, with no key, root, signature or deletion-authorization field.
    fn encode(&self) -> [u8; ENCODED] {
        let mut bytes = [0; ENCODED];
        bytes[..8].copy_from_slice(MAGIC);
        for (slot, entry) in
            bytes[8..].as_chunks_mut::<33>().0.iter_mut().zip(self.staging.iter().chain(&self.retiring))
        {
            if let Some(id) = entry {
                slot[0] = 1;
                slot[1..].copy_from_slice(&id.0);
            }
        }
        bytes
    }

    /// Rejects invalid tags, padding, format, length and duplicate identifiers without host allocations.
    /// Error cases:
    /// - any noncanonical journal encoding
    fn decode(bytes: &[u8; ENCODED]) -> Result<Self> {
        ensure!(&bytes[..8] == MAGIC, "unsupported generation journal codec");
        let mut values = [None; 4];
        for (index, slot) in bytes[8..].as_chunks::<33>().0.iter().enumerate() {
            match slot[0] {
                0 => ensure!(slot[1..].iter().all(|byte| *byte == 0), "noncanonical empty locator"),
                1 => {
                    let mut id = [0; 32];
                    id.copy_from_slice(&slot[1..]);
                    ensure!(!values[..index].contains(&Some(StoreId(id))), "duplicate generation locator");
                    values[index] = Some(StoreId(id));
                }
                _ => anyhow::bail!("invalid generation locator tag"),
            }
        }
        Ok(Self { staging: [values[0], values[1]], retiring: [values[2], values[3]] })
    }
}

impl Disk {
    /// Requires an existing directory, without creating it or touching companion files.
    /// Error cases:
    /// - the path is absent, not a directory, or cannot be inspected
    pub fn new(directory: &Path) -> Result<Self> {
        let directory =
            if directory.is_absolute() { directory.to_owned() } else { std::env::current_dir()?.join(directory) };
        ensure!(fs::symlink_metadata(&directory)?.file_type().is_dir(), "generation directory must be a directory");
        Ok(Self {
            directory,
            #[cfg(test)]
            fault: std::cell::Cell::new(None),
        })
    }

    /// Returns the configured directory; file names are always generated from StoreId.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Holds one stable post-preflight lock for the actor's entire manager lifetime.
    /// Error cases:
    /// - nonregular lock file, competing manager, or filesystem failure
    pub fn lock(&self) -> Result<File> {
        let path = self.directory.join(LOCK);
        regular_or_missing(&path)?;
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        ensure!(file.metadata()?.is_file(), "generation manager lock is not regular");
        file.try_lock().map_err(|error| anyhow::anyhow!("generation manager already owned: {error}"))?;
        Ok(file)
    }

    /// Bootstraps only a generation-free directory; a missing existing journal is never reset.
    /// This one-time bootstrap enumeration is absent from the normal restart/maintenance path.
    /// Error cases:
    /// - an existing journal/generation, incomplete directory enumeration, or durability failure
    pub fn initialize(&self) -> Result<()> {
        ensure!(!regular_or_missing(&self.directory.join(JOURNAL))?, "generation journal already exists");
        for entry in fs::read_dir(&self.directory)? {
            let name = entry?.file_name();
            ensure!(
                !owned_generation_name(&name.to_string_lossy()),
                "existing generation requires its original lifecycle journal"
            );
        }
        self.save(&State::default())
    }

    /// Reads exactly one small canonical frame; large/sparse files cannot drive allocation or traversal.
    /// Error cases:
    /// - absent/nonregular, wrong-length, torn or malformed journal
    pub fn load(&self) -> Result<State> {
        let path = self.directory.join(JOURNAL);
        ensure!(regular_or_missing(&path)?, "generation journal missing; refusing maintenance reset");
        let mut file = File::open(path)?;
        ensure!(file.metadata()?.len() == ENCODED as u64, "invalid generation journal length");
        let mut bytes = [0; ENCODED];
        file.read_exact(&mut bytes)?;
        let mut trailing = [0];
        ensure!(file.read(&mut trailing)? == 0, "generation journal changed length");
        State::decode(&bytes)
    }

    /// Atomically replaces a synced locator frame, then syncs its containing directory.
    /// Error cases:
    /// - nonregular staging path or any write/sync/rename/directory failure
    pub fn save(&self, state: &State) -> Result<()> {
        let bytes = state.encode();
        State::decode(&bytes)?;
        let next = self.directory.join(NEXT);
        if regular_or_missing(&next)? {
            fs::remove_file(&next)?;
        }
        self.check(Point::JournalCreate)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&next)?;
        self.check(Point::JournalWrite)?;
        file.write_all(&bytes)?;
        self.check(Point::JournalSync)?;
        file.sync_all()?;
        drop(file);
        self.check(Point::JournalRename)?;
        fs::rename(next, self.directory.join(JOURNAL))?;
        self.sync_directory()
    }

    /// Checks all fixed sidecar names as well as the primary database before reserving a fresh ID.
    /// Error cases:
    /// - a path cannot be inspected or is not a regular owned file
    pub fn generation_exists(&self, id: StoreId) -> Result<bool> {
        let mut exists = false;
        for path in self.paths(id) {
            exists |= regular_or_missing(&path)?;
        }
        Ok(exists)
    }

    /// Unlinks only this ID's exact owned names, after checking every sidecar's type.
    /// Error cases:
    /// - nonregular owned name, inspection or deletion failure; no recursive removal occurs
    pub fn remove_generation(&self, id: StoreId) -> Result<()> {
        let paths = self.paths(id);
        for path in &paths {
            regular_or_missing(path)?;
        }
        for path in paths {
            self.check(Point::Remove)?;
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("removing unreferenced generation file"),
            }
        }
        Ok(())
    }

    /// Makes publication/pruning or database unlink directory entries durable before proceeding.
    /// Error cases:
    /// - opening or syncing the directory fails
    pub fn sync_directory(&self) -> Result<()> {
        self.check(Point::DirectoryOpen)?;
        let file = File::open(&self.directory).context("opening generation directory")?;
        self.check(Point::DirectorySync)?;
        file.sync_all().context("syncing generation directory")
    }

    /// Constructs the database, ownership lock, SQLite sidecars and app journal names without host paths.
    fn paths(&self, id: StoreId) -> [PathBuf; 6] {
        let database = id.path(&self.directory);
        SUFFIXES.map(|suffix| {
            let mut name = database.as_os_str().to_os_string();
            name.push(suffix);
            PathBuf::from(name)
        })
    }

    /// Supplies deterministic syscall-boundary failures to source tests; production has no fault state.
    /// Error cases:
    /// - the test's selected operation fails
    pub fn check(&self, point: Point) -> Result<()> {
        #[cfg(test)]
        if let Some((selected, remaining)) = self.fault.get()
            && selected == point
        {
            if remaining == 0 {
                self.fault.set(None);
                return Err(std::io::Error::other(format!("injected {point:?}")).into());
            }
            self.fault.set(Some((selected, remaining - 1)));
        }
        let _ = point;
        Ok(())
    }

    /// Fails once at the specified matching syscall boundary after skipping earlier matches.
    #[cfg(test)]
    pub fn fail_at(&self, point: Point, skip: usize) {
        self.fault.set(Some((point, skip)));
    }
}

/// Recognizes only the fixed StoreId spelling and its explicitly owned suffixes, for bootstrap only.
fn owned_generation_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("authenticated-") else { return false };
    let Some((id, suffix)) = rest.split_once(".sqlite") else { return false };
    id.len() == 64
        && id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && SUFFIXES.contains(&suffix)
}

/// Rejects symlinks/directories/devices rather than following or recursively deleting them.
/// Error cases:
/// - an existing nonregular path or an inspection failure
fn regular_or_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(metadata.file_type().is_file(), "nonregular generation path: {}", path.display());
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting generation path {}", path.display())),
    }
}
