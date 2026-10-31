//! Marks-cache startup: seed the derived cache during recovery on a hidden file, move it into
//! place once startup commits (so a refused startup changes no persisted bytes), and hand the
//! store, the API's reader, and the task's settings to who uses them.

use anyhow::Context;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use engine_core::types::MarketId;
use tokio::sync::mpsc;

use crate::config::{BackfillSection, NodeConfig};
use crate::history::marks::store::{MARKS_FILE, Store};
use crate::history::marks::{MarkSample, Reader, Retentions, Seeder, Source};
use crate::sequencer::{Request, Sequencer};

/// What startup builds for the marks cache: the shared store recovery seeds through its replay's
/// sink, and the marks task's backfill settings. The store lives on a hidden seed file until
/// `activate` moves it into place; the guard drops the seed file if startup ends before that, so
/// a refused startup changes no persisted bytes.
pub(super) struct MarksStartup {
    /// The store, shared with the recovery seeder until `activate` moves it to the task.
    pub shared: Option<Rc<RefCell<Store>>>,
    dir: PathBuf,
    /// Set when opening the seed file created `dir`, which a refused startup's guard removes.
    created_dir: bool,
    /// The provider backfill settings, when configured and enabled.
    pub backfill: Option<BackfillSection>,
    /// The markets the feeds map to a candles adapter, one source per market.
    pub sources: BTreeMap<MarketId, Source>,
    active: bool,
}

/// The hidden file recovery seeds into, before activation moves it to `MARKS_FILE`.
const SEED_FILE: &str = ".marks-seed";

/// What activation returns: the store for the marks task, the reader the API serves from, and
/// the task's backfill settings.
pub(super) type Activated = (Store, Reader, Option<BackfillSection>, BTreeMap<MarketId, Source>);

/// Opens the cache on its hidden seed file in `cfg`'s history directory with its configured
/// retentions, authenticating rows with `mac_key` (the enclave key's marks derivation), and
/// resolves the backfill settings and the feed symbols a candles adapter serves.
/// Error cases:
/// - the retentions do not parse, or the cache cannot be opened
pub(super) fn startup(cfg: &NodeConfig, mac_key: [u8; 32]) -> anyhow::Result<MarksStartup> {
    let retentions = Retentions::of(cfg.history.marks.as_ref())?;
    let created_dir = !cfg.history.dir.exists();
    let store = Store::open(&cfg.history.dir.join(SEED_FILE), retentions, mac_key)?;
    let backfill =
        cfg.history.marks.as_ref().and_then(|marks| marks.backfill.clone()).filter(|backfill| backfill.enabled);
    let sources = crate::history::marks::sources(&cfg.feeds);
    Ok(MarksStartup {
        shared: Some(Rc::new(RefCell::new(store))),
        dir: cfg.history.dir.clone(),
        created_dir,
        backfill,
        sources,
        active: false,
    })
}

impl MarksStartup {
    /// Moves the seeded cache into place once startup commits — after custody and recovery
    /// authorization — returning the store for the marks task, the reader the API serves from,
    /// and the task's backfill settings. The open store keeps its file after the rename, with its
    /// authenticator.
    /// Error cases:
    /// - the checkpoint or rename fails
    pub(super) fn activate(mut self) -> anyhow::Result<Activated> {
        let shared = self.shared.take().expect("activate runs once");
        let store = match Rc::try_unwrap(shared) {
            Ok(cell) => cell.into_inner(),
            Err(_) => unreachable!("the recovery seeder is dropped with its replay"),
        };
        let backfill = self.backfill.take();
        let sources = std::mem::take(&mut self.sources);
        store.checkpoint().context("checkpointing the marks cache")?;
        let from = store.path().to_owned();
        let to = self.dir.join(MARKS_FILE);
        std::fs::rename(&from, &to).with_context(|| format!("moving {} into place", to.display()))?;
        for suffix in ["-wal", "-shm"] {
            let mut side = from.as_os_str().to_owned();
            side.push(suffix);
            if Path::new(&side).exists() {
                let mut to_side = to.as_os_str().to_owned();
                to_side.push(suffix);
                std::fs::rename(&side, &to_side).context("moving the marks cache's side files")?;
            }
        }
        self.active = true;
        let reader = Store::reader(&store);
        Ok((store, reader, backfill, sources))
    }
}

impl Drop for MarksStartup {
    fn drop(&mut self) {
        if self.active {
            return;
        }
        let path = self.dir.join(SEED_FILE).into_os_string();
        for suffix in ["", "-wal", "-shm"] {
            let mut side = path.clone();
            side.push(suffix);
            let _ = std::fs::remove_file(&side);
        }
        if self.created_dir {
            let _ = std::fs::remove_dir(&self.dir);
        }
    }
}

/// The sink recovery's replay hands each changed mark to, with the flags recovery checks
/// afterwards: `failed` is set if any seed write failed (recovery refuses to serve, rather than
/// sign a partially seeded cache) and `reset_done` says the log-derived content was reset — a
/// recovery with zero mark samples still resets through the caller.
pub(super) fn recovery_sink(
    shared: &Rc<RefCell<Store>>,
) -> (crate::history::marks::MarkSink, Rc<Cell<bool>>, Rc<Cell<bool>>) {
    let failed = Rc::new(Cell::new(false));
    let reset_done = Rc::new(Cell::new(false));
    let mut seeder = Seeder::new(shared.clone(), failed.clone(), reset_done.clone());
    (Box::new(move |market, ts, mark| seeder.sample((market, ts, mark))), failed, reset_done)
}

/// Attaches the marks-sample channel to `sequencer`, returning the receiver the marks task drains.
pub(super) fn track(sequencer: &mut Sequencer) -> mpsc::Receiver<MarkSample> {
    let (tx, rx) = mpsc::channel(crate::history::marks::run::QUEUE);
    sequencer.track_marks(tx);
    rx
}

/// The live-sample plumbing the marks task drains.
pub(super) fn live(rx: mpsc::Receiver<MarkSample>) -> crate::history::marks::run::Live {
    crate::history::marks::run::Live { samples: rx, buffered: Vec::new(), flushed_at: tokio::time::Instant::now() }
}

/// The supervised marks task: writes live mark reports and backfills provider history.
pub(super) async fn task(
    store: Store,
    sequencer: mpsc::WeakSender<Request>,
    backfill: Option<BackfillSection>,
    sources: BTreeMap<MarketId, Source>,
    live: crate::history::marks::run::Live,
) -> (&'static str, anyhow::Result<()>) {
    ("marks", crate::history::marks::run::run(store, sequencer, backfill, sources, live).await)
}
