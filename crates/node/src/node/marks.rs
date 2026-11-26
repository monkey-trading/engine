//! Marks-cache startup: seed the derived cache during recovery on a hidden file, move it into
//! place once startup commits (so a refused startup changes no persisted bytes), and hand the
//! store, the API's reader, and the task's settings to who uses them. Recovery chooses what the
//! seed starts from: the cache an earlier run left (`import_live`), or, for a start from a daily
//! snapshot, the frozen copy of the cache at its checkpoint that the snapshot's anchors vouch for
//! (`import_frozen`). A member's start from its replica does none of the work its cache's size sets
//! before its first command: it starts on an empty cache, sets its earlier holding's aside at
//! activation, and the marks task takes it after.

use anyhow::{Context, ensure};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tracing::{info, warn};

use engine_core::types::MarketId;
use tokio::sync::mpsc;

use crate::config::{BackfillSection, NodeConfig};
use crate::history::marks::store::{Frozen, MARKS_FILE, Store, discard_file, frozen_path};
use crate::history::marks::{Reader, Report, Retentions, Seeder, Source};
use crate::log::Offset;
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
    /// Whether the prior cache waits for the marks task (a member's start from its replica).
    deferred: bool,
    /// The cache an earlier run left, which `import_live` takes.
    prior: Option<PathBuf>,
    /// Whether recovery has chosen what the seed starts from (`import_live`, `import_frozen`).
    chosen: Cell<bool>,
    active: bool,
}

/// The hidden file recovery seeds into, before activation moves it to `MARKS_FILE`.
pub(crate) const SEED_FILE: &str = ".marks-seed";
/// Where a deferred start's activation sets the prior cache aside until the marks task takes it:
/// one a start finds holds everything the cache held, since nothing is written before it is taken.
pub(crate) const PRIOR_FILE: &str = ".marks-prior";

/// What activation returns: the store for the marks task, the reader the API serves from, the
/// task's backfill settings, and the prior cache a deferred start leaves the task to take.
pub(super) type Activated = (Store, Reader, Option<BackfillSection>, BTreeMap<MarketId, Source>, Option<PathBuf>);

/// Opens the cache on its hidden seed file in `cfg`'s history directory with its configured
/// retentions, authenticating rows with `mac_key` (the enclave key's marks derivation), and
/// resolves the backfill settings and the feed symbols a candles adapter serves. The seed holds
/// nothing until recovery chooses what it starts from (`import_live`, `import_frozen`). A
/// `deferred` start imports nothing: its readers answer not ready until the marks task has taken
/// the prior cache (`run::CatchUp`).
/// Error cases:
/// - the retentions do not parse, or the cache cannot be opened
pub(super) fn startup(cfg: &NodeConfig, mac_key: [u8; 32], deferred: bool) -> anyhow::Result<MarksStartup> {
    let retentions = Retentions::of(cfg.history.marks.as_ref())?;
    let created_dir = !cfg.history.dir.exists();
    // A seed file a killed startup left behind is transient: its rows are re-imported and
    // re-derived below, and keeping it could combine chains a prune has since moved.
    let seed = cfg.history.dir.join(SEED_FILE);
    if seed.exists() {
        discard_file(&seed).with_context(|| format!("clearing the leftover marks seed at {}", seed.display()))?;
    }
    let store = Store::create(&seed, retentions, mac_key)?;
    // A previous run's cache carries provider-seeded rows and backfill markers the log cannot
    // rebuild; recovery's re-derivation resets only the log-derived content over them. A prior set
    // aside by a deferred start that never took it holds all the cache did.
    let prior = [PRIOR_FILE, MARKS_FILE].map(|file| cfg.history.dir.join(file)).into_iter().find(|path| path.exists());
    if deferred {
        store.defer();
    }
    let backfill =
        cfg.history.marks.as_ref().and_then(|marks| marks.backfill.clone()).filter(|backfill| backfill.enabled);
    let sources = crate::history::marks::sources(&cfg.feeds);
    Ok(MarksStartup {
        shared: Some(Rc::new(RefCell::new(store))),
        dir: cfg.history.dir.clone(),
        created_dir,
        backfill,
        sources,
        deferred,
        prior,
        chosen: Cell::new(false),
        active: false,
    })
}

impl MarksStartup {
    /// Starts the seed from the cache an earlier run left, if any: its rows that authenticate and its backfill
    /// markers, for a replay of the log from its start, which re-derives the log-derived rows, or from a recovery
    /// cache, which keeps them. A cache whose chains then fail validation is host-altered, and the seed starts
    /// empty, paging Warning `marks_cache_rebuilt`: the log re-derives what it proves (a full recovery), a configured
    /// backfill re-seeds the rest, and recovery-cache deployments, which replay no log prefix into the cache, never
    /// inherit the hole.
    /// Error cases:
    /// - the prior cache cannot be read, or the seed cannot be cleared
    pub(super) fn import_live(&self) -> anyhow::Result<()> {
        self.chosen.set(true);
        let Some(prior) = &self.prior else { return Ok(()) };
        let shared = self.shared.as_ref().expect("the marks store opens at startup");
        let mut store = shared.borrow_mut();
        store.import_prior(prior).context("importing the prior marks cache")?;
        if let Err(e) = store.validate_chains() {
            warn!(alert = "marks_cache_rebuilt", "the prior marks cache fails validation and is rebuilt: {e:#}");
            store.clear().context("clearing the invalid prior marks")?;
        }
        Ok(())
    }

    /// Starts the seed from the frozen copy of the cache at the checkpoint of the daily snapshot a start goes on
    /// from, at `offset`, once every chain in it is the one `frozen`, the anchors the snapshot carries under its
    /// signature, names, and its rows are the ones their root covers, under whatever key this start holds
    /// (`Store::import_frozen`), for the replay of the log after it to extend (`extending_sink`).
    /// A snapshot that carries no anchors, a copy that is missing or fails the check, starts the seed empty, paging
    /// Warning `marks_cache_rebuilt`: the log-derived history starts at the snapshot, and the provider backfill
    /// fills what it can.
    pub(super) fn import_frozen(&self, offset: Offset, frozen: Option<&Frozen>) {
        self.chosen.set(true);
        let shared = self.shared.as_ref().expect("the marks store opens at startup");
        let copy = frozen_path(&self.dir, offset);
        let refused = match frozen {
            None => "the daily snapshot carries no marks anchors".to_owned(),
            Some(_) if !copy.exists() => format!("its frozen copy {} is missing", copy.display()),
            Some(frozen) => match shared.borrow_mut().import_frozen(&copy, frozen) {
                Ok(imported) => {
                    info!(seq = offset.seq, imported, "the marks cache starts from the daily snapshot's frozen copy");
                    return;
                }
                // The import is one transaction, so the seed it refused holds nothing.
                Err(e) => format!("its frozen copy {} is refused: {e:#}", copy.display()),
            },
        };
        warn!(
            alert = "marks_cache_rebuilt",
            seq = offset.seq,
            "the marks cache starts empty at the daily snapshot, so its log-derived history begins there: {refused}"
        );
    }

    /// Moves the seeded cache into place once startup commits — after custody and recovery
    /// authorization — returning the store for the marks task, the reader the API serves from,
    /// and the task's backfill settings. The open store keeps its file after the rename, with its
    /// authenticator. A deferred start first sets the prior cache aside as `PRIOR_FILE`, unless an
    /// earlier one never taken is there, and returns it for the marks task; any other start's
    /// import took it, and it goes.
    /// Error cases:
    /// - recovery chose nothing for a start that is not deferred, or the checkpoint, a rename, or the discard fails
    pub(super) fn activate(mut self) -> anyhow::Result<Activated> {
        // Moving an empty seed over the cache would drop what an earlier run left.
        ensure!(self.deferred || self.chosen.get(), "the marks cache is activated before recovery chose its start");
        let shared = self.shared.take().expect("activate runs once");
        let mut store = match Rc::try_unwrap(shared) {
            Ok(cell) => cell.into_inner(),
            Err(_) => unreachable!("the recovery seeder is dropped with its replay"),
        };
        let backfill = self.backfill.take();
        let sources = std::mem::take(&mut self.sources);
        store.checkpoint().context("checkpointing the marks cache")?;
        let from = store.path().to_owned();
        let to = self.dir.join(MARKS_FILE);
        let prior = self.dir.join(PRIOR_FILE);
        match self.deferred {
            true if !prior.exists() && to.exists() => move_file(&to, &prior)?,
            true => {}
            false if prior.exists() => {
                discard_file(&prior).with_context(|| format!("discarding the imported {}", prior.display()))?
            }
            false => {}
        }
        move_file(&from, &to)?;
        store.relocate(to);
        self.active = true;
        let reader = Store::reader(&store);
        let prior = Some(prior).filter(|prior| self.deferred && prior.exists());
        Ok((store, reader, backfill, sources, prior))
    }
}

/// Renames the SQLite file at `from` to `to`, with its `-wal` and `-shm` side files.
/// Error cases:
/// - a rename fails
fn move_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::rename(from, to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut side = from.as_os_str().to_owned();
        side.push(suffix);
        if Path::new(&side).exists() {
            let mut to_side = to.as_os_str().to_owned();
            to_side.push(suffix);
            std::fs::rename(&side, &to_side).context("moving the marks cache's side files")?;
        }
    }
    Ok(())
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
pub(crate) fn recovery_sink(
    shared: &Rc<RefCell<Store>>,
) -> (crate::history::marks::MarkSink, Rc<Cell<bool>>, Rc<Cell<bool>>) {
    let failed = Rc::new(Cell::new(false));
    let reset_done = Rc::new(Cell::new(false));
    let mut seeder = Seeder::new(shared.clone(), failed.clone(), reset_done.clone());
    (Box::new(move |market, ts, mark| seeder.sample((market, ts, mark))), failed, reset_done)
}

/// The sink a replay from a daily snapshot's checkpoint hands each changed mark to: it extends the cache the
/// snapshot's frozen copy started (`MarksStartup::import_frozen`) and resets nothing. `failed` is set as for
/// `recovery_sink`.
pub(crate) fn extending_sink(shared: &Rc<RefCell<Store>>) -> (crate::history::marks::MarkSink, Rc<Cell<bool>>) {
    let failed = Rc::new(Cell::new(false));
    let mut seeder = Seeder::extending(shared.clone(), failed.clone());
    (Box::new(move |market, ts, mark| seeder.sample((market, ts, mark))), failed)
}

/// Attaches the marks report channel to `sequencer`, returning the receiver the marks task drains.
pub(super) fn track(sequencer: &mut Sequencer) -> mpsc::Receiver<Report> {
    let (tx, rx) = mpsc::channel(crate::history::marks::run::QUEUE);
    sequencer.track_marks(tx);
    rx
}

/// The live-sample plumbing the marks task drains.
pub(super) fn live(rx: mpsc::Receiver<Report>) -> crate::history::marks::run::Live {
    crate::history::marks::run::Live::new(rx)
}

/// The supervised marks task: takes a member's earlier history (`catch_up`), if any, then writes live
/// mark reports and backfills provider history.
pub(super) async fn task(
    store: Store,
    sequencer: mpsc::WeakSender<Request>,
    backfill: Option<BackfillSection>,
    sources: BTreeMap<MarketId, Source>,
    live: crate::history::marks::run::Live,
    catch_up: crate::history::marks::run::CatchUp,
) -> (&'static str, anyhow::Result<()>) {
    ("marks", crate::history::marks::run::run(store, sequencer, backfill, sources, live, catch_up).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prior cache the host altered while the node was stopped — a hole the import would
    /// otherwise commit — fails startup's validation and is rebuilt from nothing, on both
    /// recovery paths, instead of refusing reads or stopping the node on a later write.
    #[tokio::test]
    async fn an_invalid_prior_cache_is_rebuilt_at_startup() {
        let mut cfg = crate::test_support::dev_config();
        cfg.history.dir = crate::test_support::scratch_dir("marks-invalid-prior").join("history");
        let key = crate::tee::EnclaveKey::test_key().marks_key();
        std::fs::create_dir_all(&cfg.history.dir).unwrap();
        {
            let mut prior = Store::open(&cfg.history.dir.join(MARKS_FILE), Retentions::defaults(), key).unwrap();
            let base = 1_792_489_860_000;
            prior
                .observe(&[
                    (1, base, "67000".parse().unwrap()),
                    (1, base + 60_000, "67100".parse().unwrap()),
                    (1, base + 120_000, "67200".parse().unwrap()),
                ])
                .unwrap();
            // The host deletes an interior bucket while the node is stopped.
            rusqlite::Connection::open(cfg.history.dir.join(MARKS_FILE))
                .unwrap()
                .execute("DELETE FROM marks WHERE market = 1 AND resolution = 1 AND bucket = ?1", [base + 60_000])
                .unwrap();
        }
        let marks = startup(&cfg, key, false).unwrap();
        marks.import_live().unwrap();
        let reader = marks.shared.as_ref().unwrap().borrow().reader();
        let page = reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        assert!(page.entries.is_empty(), "the invalid prior cache is not served");
        // The rebuilt cache accepts live writes.
        marks
            .shared
            .as_ref()
            .unwrap()
            .borrow_mut()
            .observe(&[(1, 1_792_489_860_000, "67000".parse().unwrap())])
            .unwrap();
        let page = reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        assert_eq!(page.entries.len(), 1);
    }

    /// A member's start from its replica imports nothing before its first command: its reader is not
    /// ready, activation sets the prior cache aside, and the marks task's catch-up takes it. A prior
    /// set aside and never taken, by a start that stopped first, is what the next start imports.
    #[tokio::test]
    async fn a_deferred_start_leaves_the_prior_cache_to_the_catch_up() {
        let mut cfg = crate::test_support::dev_config();
        cfg.history.dir = crate::test_support::scratch_dir("marks-deferred").join("history");
        let key = crate::tee::EnclaveKey::test_key().marks_key();
        std::fs::create_dir_all(&cfg.history.dir).unwrap();
        let base = 1_792_489_860_000;
        {
            let mut prior = Store::open(&cfg.history.dir.join(MARKS_FILE), Retentions::defaults(), key).unwrap();
            prior
                .observe(&[(1, base, "67000".parse().unwrap()), (1, base + 60_000, "67100".parse().unwrap())])
                .unwrap();
        }
        let minutes = async |reader: &Reader| reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap();

        // A start that stops after activation, before its catch-up, leaves the prior aside.
        let marks = startup(&cfg, key, true).unwrap();
        let reader = marks.shared.as_ref().unwrap().borrow().reader();
        assert!(!reader.ready(), "a deferred start's reader is ready before its catch-up");
        assert!(minutes(&reader).await.entries.is_empty(), "the start imported the prior cache");
        let (store, _, _, _, prior) = marks.activate().unwrap();
        assert_eq!(prior.as_deref(), Some(cfg.history.dir.join(PRIOR_FILE).as_path()), "the prior set aside");
        drop(store);

        // The next deferred start keeps that prior, and its catch-up takes it.
        let marks = startup(&cfg, key, true).unwrap();
        let (mut store, reader, _, _, prior) = marks.activate().unwrap();
        let prior = prior.expect("the prior never taken is kept");
        store.catch_up(Some(&prior), None).unwrap();
        assert!(reader.ready(), "the reader is not ready after the catch-up");
        assert_eq!(minutes(&reader).await.entries.len(), 2, "the catch-up did not take the prior cache");
        assert!(!prior.exists(), "the catch-up left the prior cache");
        drop(store);

        // Set aside again and never taken, it is what a full recovery's start imports.
        let (store, ..) = startup(&cfg, key, true).unwrap().activate().unwrap();
        drop(store);
        let marks = startup(&cfg, key, false).unwrap();
        marks.import_live().unwrap();
        let (_store, reader, ..) = marks.activate().unwrap();
        assert_eq!(minutes(&reader).await.entries.len(), 2, "the next start did not import the prior set aside");
        assert!(!cfg.history.dir.join(PRIOR_FILE).exists(), "the imported prior was kept");
    }

    /// A seed file a killed startup left behind is discarded: startup rebuilds from the prior
    /// cache and the log, never from chains a prune may have moved since.
    #[tokio::test]
    async fn a_leftover_seed_file_is_discarded_before_startup() {
        let mut cfg = crate::test_support::dev_config();
        cfg.history.dir = crate::test_support::scratch_dir("marks-leftover-seed").join("history");
        let key = crate::tee::EnclaveKey::test_key().marks_key();
        std::fs::create_dir_all(&cfg.history.dir).unwrap();
        {
            let mut leftover = Store::open(&cfg.history.dir.join(SEED_FILE), Retentions::defaults(), key).unwrap();
            leftover.observe(&[(7, 1_792_489_860_000, "67000".parse().unwrap())]).unwrap();
        }
        let marks = startup(&cfg, key, false).unwrap();
        marks.import_live().unwrap();
        let reader = marks.shared.as_ref().unwrap().borrow().reader();
        let page = reader.page(7, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        assert!(page.entries.is_empty(), "the leftover seed's rows are not served");
    }

    /// Activation moves the seeded cache to its final name: the store and reader it returns work
    /// at that path, and no seed file remains.
    #[tokio::test]
    async fn activation_serves_reads_and_writes_at_the_final_path() {
        let mut cfg = crate::test_support::dev_config();
        cfg.history.dir = crate::test_support::scratch_dir("marks-activate").join("history");
        let key = crate::tee::EnclaveKey::test_key().marks_key();
        let marks = startup(&cfg, key, false).unwrap();
        marks.import_live().unwrap();
        marks
            .shared
            .as_ref()
            .unwrap()
            .borrow_mut()
            .observe(&[(1, 1_792_489_860_000, "67000".parse().unwrap())])
            .unwrap();
        let (mut store, reader, ..) = marks.activate().unwrap();
        let final_path = cfg.history.dir.join(crate::history::marks::store::MARKS_FILE);
        assert!(final_path.exists(), "the cache moved into place");
        assert!(!cfg.history.dir.join(SEED_FILE).exists(), "no seed file remains");
        let page = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        assert_eq!(page.entries.len(), 1, "{page:?}");
        // The task's store still writes after the rename, and the reader sees it.
        store.observe(&[(1, 1_792_489_860_500, "67100".parse().unwrap())]).unwrap();
        let page = reader.page(1, Resolution::S1, i64::MIN, i64::MAX, 10, 10).await.unwrap();
        assert_eq!(page.entries[0].close, "67100".parse().unwrap());
    }

    /// A start from a daily snapshot takes the frozen copy the snapshot's anchors name, and starts the cache empty,
    /// paging `marks_cache_rebuilt`, for a copy rolled back to an earlier one of the same cache, a copy that is
    /// missing, and a snapshot that carries no anchors.
    #[tokio::test]
    async fn a_start_takes_only_the_frozen_copy_its_snapshot_anchors() {
        let mut cfg = crate::test_support::dev_config();
        cfg.history.dir = crate::test_support::scratch_dir("marks-frozen-start").join("history");
        let key = crate::tee::EnclaveKey::test_key().marks_key();
        let base = 1_792_489_860_000;
        let (earlier, later) = (Offset { seq: 4, index: 1 }, Offset { seq: 8, index: 1 });
        let frozen = {
            let mut live = Store::create(&cfg.history.dir.join(MARKS_FILE), Retentions::defaults(), key).unwrap();
            live.observe(&[(1, base, "67000".parse().unwrap())]).unwrap();
            live.freeze(earlier).unwrap();
            live.observe(&[(1, base + 60_000, "67100".parse().unwrap())]).unwrap();
            live.freeze(later).unwrap()
        };
        let minutes = async |frozen: Option<&Frozen>| {
            let marks = startup(&cfg, key, false).unwrap();
            marks.import_frozen(later, frozen);
            let reader = marks.shared.as_ref().unwrap().borrow().reader();
            reader.page(1, Resolution::M1, i64::MIN, i64::MAX, 10, 10).await.unwrap().entries.len()
        };
        assert_eq!(minutes(Some(&frozen)).await, 2, "the copy the anchors name is taken");

        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let copy = frozen_path(&cfg.history.dir, later);
        std::fs::copy(frozen_path(&cfg.history.dir, earlier), &copy).unwrap();
        assert_eq!(minutes(Some(&frozen)).await, 0, "a rolled back copy is not taken");
        assert_eq!(minutes(None).await, 0, "a snapshot with no anchors takes no copy");
        std::fs::remove_file(&copy).unwrap();
        assert_eq!(minutes(Some(&frozen)).await, 0, "a missing copy starts nothing");
        let rebuilt = std::iter::from_fn(|| alerts.try_recv().ok())
            .filter(|alert| alert["alert"] == "marks_cache_rebuilt" && alert["level"] == "WARN")
            .count();
        assert_eq!(rebuilt, 3, "each refusal pages");
    }

    use crate::history::marks::Resolution;
}
