//! Real-file fixtures for catalog, suffix, stream, and archive regressions.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use engine_core::command::{Command, Sequenced};
use tokio::sync::watch;

use crate::authenticated::{Root, Store};
use crate::log::authenticated_tests::head_support::{HeadFiles, access, authority};
use crate::log::catalog::LazyCatalogView;
use crate::log::catalog_access::CatalogRead;
use crate::log::catalog_owner::CatalogOwner;
use crate::log::{
    CatalogBoundary, Checkpoint, CommandEntry, LogEntry, LogReader, LogWriter, MANIFEST, Manifest, Offset, SegmentedLog,
};
use crate::test_support::scratch_dir;

mod active;
mod catalog;
mod continuation;
mod head_support;
mod publication;
mod rewind;
mod stream;

pub(crate) struct Fixture {
    pub dir: PathBuf,
    pub store: Arc<Store>,
    pub read: Arc<dyn CatalogRead>,
    pub heads: Arc<HeadFiles>,
    pub pins: Vec<Arc<LazyCatalogView>>,
    pub writer: LogWriter,
    pub view: watch::Receiver<Manifest>,
}

/// Creates a real catalog database and an authenticated writer over real segment files.
pub(crate) fn fixture(label: &str, segment_commands: u64) -> Fixture {
    let dir = scratch_dir(label);
    fs::create_dir_all(&dir).expect("fixture directory");
    let store = Arc::new(Store::create(dir.join("catalog.sqlite")).expect("catalog store"));
    let read = access(store.clone(), dir.join("catalog.sqlite"));
    let heads = Arc::new(HeadFiles::new(&dir));
    let owner = CatalogOwner::create(store.clone(), read.clone(), Root::empty([7; 32]), authority(heads.clone()))
        .expect("initial signed head");
    let log = SegmentedLog::open(&dir, |_| Ok(()))
        .expect("empty strict replay")
        .attach_catalog(owner)
        .expect("initial catalog");
    let (tx, view) = watch::channel(Manifest::default());
    let writer = log.into_writer(segment_commands, tx).expect("writer");
    Fixture { dir, store, read, heads, pins: Vec::new(), writer, view }
}

/// A deterministic sequenced command with event logging enabled.
pub(crate) fn command(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        seal: None,
        sequenced: Sequenced { seq, ts: seq as i64, cmd: Command::Tick },
        events: Some(Vec::new()),
    })
}

/// A checkpoint whose signatures are opaque to structural log tests.
pub(crate) fn checkpoint(seq: u64) -> LogEntry {
    LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq,
        root: "00".repeat(32),
        signature: "11".repeat(65),
        signer: "22".repeat(20),
        domain: None,
        deferred_withdrawals: false,
    })
}

/// Serializes one public log line, including its final newline.
pub(crate) fn line(entry: &LogEntry) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(entry).expect("entry encoding");
    bytes.push(b'\n');
    bytes
}

/// Commits a boundary at the current checkpoint after the authoritative log sync.
fn boundary(f: &mut Fixture, seq: u64) -> CatalogBoundary {
    f.writer.append(&checkpoint(seq)).expect("checkpoint append");
    f.writer.sync().expect("log sync");
    let (boundary, pin) = f.writer.seal_catalog_boundary().expect("catalog seal");
    f.pins.push(pin);
    boundary
}

/// Appends host-controlled bytes without advancing the writer's trusted commitment.
fn host_append(path: &std::path::Path, bytes: &[u8]) {
    fs::OpenOptions::new().append(true).open(path).expect("host open").write_all(bytes).expect("host append");
}

/// Collects every available authenticated offset, propagating integrity failures.
fn offsets(reader: &mut LogReader) -> anyhow::Result<Vec<Offset>> {
    let mut out = Vec::new();
    while let Some((offset, _)) = reader.next_entry()? {
        out.push(offset);
    }
    Ok(out)
}

mod recovery_guards;
