//! Marks-cache startup: open the derived cache before recovery so the replay seeds it, and hand
//! the store, the API's reader, and the backfill settings to the task and router that use them.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use engine_core::types::MarketId;
use tokio::sync::mpsc;

use crate::config::{BackfillSection, NodeConfig};
use crate::history::marks::store::Store;
use crate::history::marks::{MarkSample, Reader, Retentions, Seeder, Source};
use crate::sequencer::{Request, Sequencer};

/// What startup builds for the marks cache: the shared store recovery seeds through its replay's
/// sink, the reader the API serves from, and the marks task's backfill settings.
pub(super) struct MarksStartup {
    /// The store, shared with the recovery seeder until `into_task_store` moves it to the task.
    pub shared: Rc<RefCell<Store>>,
    /// A reader of the store for the API, on its own read-only connections.
    pub reader: Reader,
    /// The provider backfill settings, when configured and enabled.
    pub backfill: Option<BackfillSection>,
    /// The markets the feeds map to a candles adapter, one source per market.
    pub sources: BTreeMap<MarketId, Source>,
}

/// Opens the cache in `cfg`'s history directory with its configured retentions, and resolves the
/// backfill settings and the feed symbols a candles adapter serves.
/// Error cases:
/// - the retentions do not parse, or the cache cannot be opened
pub(super) fn startup(cfg: &NodeConfig) -> anyhow::Result<MarksStartup> {
    let retentions = Retentions::of(cfg.history.marks.as_ref())?;
    let store = Store::open_in(&cfg.history.dir, retentions)?;
    let shared = Rc::new(RefCell::new(store));
    let reader = shared.borrow().reader();
    let backfill =
        cfg.history.marks.as_ref().and_then(|marks| marks.backfill.clone()).filter(|backfill| backfill.enabled);
    let sources = crate::history::marks::sources(&cfg.feeds);
    Ok(MarksStartup { shared, reader, backfill, sources })
}

/// Moves the seeded store out for the marks task. The recovery seeder's sink is dropped with its
/// replay before this runs, so the share count is one.
pub(super) fn into_task_store(shared: Rc<RefCell<Store>>) -> Store {
    match Rc::try_unwrap(shared) {
        Ok(cell) => cell.into_inner(),
        Err(_) => unreachable!("the recovery seeder is dropped with its replay"),
    }
}

/// The sink recovery's replay hands each changed mark to; it buffers batches into the store.
pub(super) fn recovery_sink(shared: &Rc<RefCell<Store>>) -> crate::history::marks::MarkSink {
    let mut seeder = Seeder::new(shared.clone());
    Box::new(move |market, ts, mark| seeder.sample((market, ts, mark)))
}

/// Attaches the marks-sample channel to `sequencer`, returning the receiver the marks task drains.
pub(super) fn track(sequencer: &mut Sequencer) -> mpsc::Receiver<MarkSample> {
    let (tx, rx) = mpsc::channel(crate::history::marks::run::QUEUE);
    sequencer.track_marks(tx);
    rx
}

/// The supervised marks task: writes live mark reports and backfills provider history.
pub(super) async fn task(
    store: Store,
    samples: mpsc::Receiver<MarkSample>,
    backfill: Option<BackfillSection>,
    sources: BTreeMap<MarketId, Source>,
    sequencer: mpsc::WeakSender<Request>,
) -> (&'static str, anyhow::Result<()>) {
    ("marks", crate::history::marks::run::run(store, samples, backfill, sources, sequencer).await)
}
