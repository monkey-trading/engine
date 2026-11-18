//! Each command's derived rows on disk, for the history's retention, so the financing read proves a row
//! the history lists once the rows have left memory (`DerivedRows`). The history writes a command's rows
//! as it takes them, one file a command, `<seq>.rows`: its stamp, its row count and its rows. The host
//! holds the files, so a reader takes a file's rows only once they give the aggregates the log holds
//! for the command (`Derived::checked`). The files sit in `<history.dir>/financing/`, about 18 MB a
//! rollover at 440k positions. A rebuild of an authenticated history generation takes its rows from its
//! own replay and writes each command's file as the history does.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::financing::FinancingRow;
use engine_core::types::{AccountId, Millis};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::history::derived::{Derived, aggregates};
use crate::log::{CommandEntry, LogEntry, LogReader, Manifest, Offset};

/// The directory under the history's own.
const DIR: &str = "financing";
const MAGIC: &[u8; 8] = b"engrows1";
/// The magic, the command's stamp and the row count.
const HEADER: usize = 24;
/// An account's 20 bytes, its market's 4 and the amount's 16, big-endian.
const ROW: usize = 40;

/// The derived rows of the commands the history took, by seq, and the last ones a read checked
/// against the log.
pub(crate) struct RowsStore {
    dir: PathBuf,
    retention_ms: Millis,
    checked: Mutex<Option<(u64, Derived)>>,
    /// One permit, held by the read that owns `derived_at`'s turn (`turn`).
    gate: Arc<Semaphore>,
}

impl RowsStore {
    /// The store under `history_dir`, keeping a command's rows `retention_ms` from its stamp, as the
    /// history keeps its rows.
    pub(crate) fn new(history_dir: &Path, retention_ms: Millis) -> Self {
        Self { dir: history_dir.join(DIR), retention_ms, checked: Mutex::default(), gate: Arc::new(Semaphore::new(1)) }
    }

    /// Waits for the turn to read from this store, which `derived_at` takes one at a time. A reader holds
    /// it from before it takes any permit the log's other readers share, so those waiting hold none.
    pub(crate) async fn turn(&self) -> OwnedSemaphorePermit {
        self.gate.clone().acquire_owned().await.expect("the store's gate is never closed")
    }

    /// Writes `rows`, the derived rows of the command at `seq` stamped `ts`, over any file for `seq`,
    /// and removes the files of commands stamped more than the retention before it.
    /// Error cases:
    /// - the directory or the file cannot be written, or an old file cannot be removed
    pub(crate) fn record(&self, seq: u64, ts: Millis, rows: &[FinancingRow]) -> anyhow::Result<()> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let mut bytes = Vec::with_capacity(HEADER + ROW * rows.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&ts.to_be_bytes());
        bytes.extend_from_slice(&(rows.len() as u64).to_be_bytes());
        for row in rows {
            bytes.extend_from_slice(&row.account.0);
            bytes.extend_from_slice(&row.market.to_be_bytes());
            bytes.extend_from_slice(&row.amount.to_be_bytes());
        }
        let path = self.path(seq);
        let partial = path.with_extension("partial");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&partial)
            .with_context(|| format!("creating {}", partial.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", partial.display()))?;
        fs::rename(&partial, &path).with_context(|| format!("replacing {}", path.display()))?;
        // The rename, and the directory's own entry, survive a crash once the history moves past them.
        for dir in [Some(self.dir.as_path()), self.dir.parent()].into_iter().flatten() {
            File::open(dir).and_then(|dir| dir.sync_all()).with_context(|| format!("syncing {}", dir.display()))?;
        }
        self.prune(ts.saturating_sub(self.retention_ms))
    }

    /// `command`'s rows, which the log holds, from this store when its file gives the aggregates
    /// `command` logs; none when it logs none or there is no file.
    /// Error cases:
    /// - the file cannot be read, or its rows are not the ones `command`'s aggregates cover
    pub(crate) fn derived(&self, command: &CommandEntry) -> anyhow::Result<Option<Derived>> {
        let (seq, events) = (command.sequenced.seq, command.events.as_deref().unwrap_or_default());
        let expected = aggregates(events)?.iter().map(|aggregate| aggregate.positions).sum();
        if expected == 0 {
            return Ok(None);
        }
        let Some(rows) = self.rows(seq, command.sequenced.ts, expected)? else { return Ok(None) };
        Derived::checked(seq, events, rows)
    }

    /// The rows of the command at `seq`, which the log in `dir` holds durably, as `derived` finds them.
    /// One read at a time reads the log and a file, and the last rows found are kept for the next.
    /// Error cases:
    /// - the log cannot be read at `seq`, or as for `derived`
    pub(crate) fn derived_at(
        &self,
        dir: &Path,
        manifest: watch::Receiver<Manifest>,
        seq: u64,
    ) -> anyhow::Result<Option<Derived>> {
        let mut checked = self.checked.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, derived)) = checked.as_ref().filter(|(at, _)| *at == seq) {
            return Ok(Some(derived.clone()));
        }
        // A command with no file, as most are, costs no read of the log.
        let path = self.path(seq);
        if !path.try_exists().with_context(|| format!("checking for {}", path.display()))? {
            return Ok(None);
        }
        let mut reader = LogReader::open(dir, Offset::start_of(seq), manifest)
            .with_context(|| format!("opening the log at seq {seq}"))?;
        while let Some((_, entry)) = reader.next_entry().with_context(|| format!("reading the log at seq {seq}"))? {
            let LogEntry::Command(command) = entry else { continue };
            ensure!(command.sequenced.seq == seq, "the log reads seq {} at {seq}", command.sequenced.seq);
            let derived = self.derived(&command)?;
            if let Some(derived) = &derived {
                *checked = Some((seq, derived.clone()));
            }
            return Ok(derived);
        }
        Ok(None)
    }

    /// The `expected` rows stored for the command at `seq` stamped `ts`, as the host left them; none
    /// when there is no file.
    /// Error cases:
    /// - the file cannot be read, or is not a rows file of `expected` rows of that command
    fn rows(&self, seq: u64, ts: Millis, expected: usize) -> anyhow::Result<Option<Vec<FinancingRow>>> {
        let path = self.path(seq);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        let len = HEADER + ROW * expected;
        // Bounded by the logged count before a byte is read, so a host's file cannot size the read.
        let size = file.metadata().with_context(|| format!("reading the size of {}", path.display()))?.len();
        ensure!(size == len as u64, "{} is not {expected} rows long", path.display());
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes).with_context(|| format!("reading {}", path.display()))?;
        ensure!(&bytes[..8] == MAGIC, "{} is not a rows file", path.display());
        ensure!(bytes[8..16] == ts.to_be_bytes(), "{} holds another command's rows", path.display());
        ensure!(bytes[16..HEADER] == (expected as u64).to_be_bytes(), "{} counts other rows", path.display());
        let rows = bytes[HEADER..]
            .as_chunks::<ROW>()
            .0
            .iter()
            .map(|row| FinancingRow {
                account: AccountId(row[..20].try_into().expect("20 bytes")),
                market: u32::from_be_bytes(row[20..24].try_into().expect("4 bytes")),
                amount: Fixed::from_raw(i128::from_be_bytes(row[24..].try_into().expect("16 bytes"))),
            })
            .collect();
        Ok(Some(rows))
    }

    fn path(&self, seq: u64) -> PathBuf {
        self.dir.join(format!("{seq}.rows"))
    }

    /// Removes the files of commands stamped before `oldest`, files that hold no stamp, and the partial
    /// files a crash left.
    /// Error cases:
    /// - the directory cannot be listed, or a file cannot be read or removed
    fn prune(&self, oldest: Millis) -> anyhow::Result<()> {
        for entry in fs::read_dir(&self.dir).with_context(|| format!("listing {}", self.dir.display()))? {
            let path = entry?.path();
            let stale = match path.extension().and_then(|extension| extension.to_str()) {
                Some("partial") => true,
                Some("rows") => {
                    let mut header = [0; 16];
                    let read = File::open(&path).and_then(|mut file| file.read_exact(&mut header));
                    let stamp = read.ok().filter(|()| &header[..8] == MAGIC);
                    stamp.is_none_or(|()| Millis::from_be_bytes(header[8..].try_into().expect("8 bytes")) < oldest)
                }
                _ => false,
            };
            if stale {
                fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            }
        }
        Ok(())
    }
}
