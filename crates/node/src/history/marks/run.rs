//! The marks task: keeps the derived cache current from the sequencer's live mark reports, and
//! backfills provider history for listed markets the feeds map to a candles adapter. Supervised
//! like every other task: an error stops the node, since the cache shares the history directory's
//! disk. Samples a slow disk drops are re-derived at the next full recovery without `[snapshots]`, never roots.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::{MarketId, Millis};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval};
use tracing::{info, warn};

use crate::config::BackfillSection;
use crate::history::marks::backfill::{self, Candles, Source};
use crate::history::marks::fold::MarkFold;
use crate::history::marks::store::{Coverage, Store};
use crate::history::marks::{Freeze, MarkSample, Report};
use crate::sequencer::{Query, Request};

/// Samples buffered before one write.
pub(crate) const BATCH: usize = 512;
/// How many samples the sequencer may have in flight; over it, a sample is dropped, never the
/// sequencer delayed.
pub(crate) const QUEUE: usize = 4096;
/// How often buffered samples flush, so a quiet market's latest mark reaches the cache.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// How often a due backfill is attempted, one market at a time.
const BACKFILL_EVERY: Duration = Duration::from_secs(10);
/// How long after a failed backfill the market is retried.
const RETRY_AFTER: Duration = Duration::from_secs(300);

/// The live-sample plumbing the task owns: the queue's receiver, the write buffer, and the daily snapshots'
/// freezes taken from the queue and not yet written. A backfill holds the task for minutes; it takes `&mut Live`
/// and drains between page fetches, so the queue cannot fill and drop samples while it works.
pub(crate) struct Live {
    pub samples: mpsc::Receiver<Report>,
    pub buffered: Vec<MarkSample>,
    /// When the buffer last flushed; a backfill keeps this deadline between pages.
    pub flushed_at: Instant,
    /// The freezes the task writes once it next can, in order, each after the samples buffered before it and with
    /// the samples received after it and before the next, which are written only once it is (`frozen`).
    pub freezes: Vec<(Freeze, Vec<MarkSample>)>,
}

/// What a member's start leaves its marks cache to take once the node sequences: the cache its earlier
/// holding left, moved aside at activation, and the marks its replica folded while it followed.
#[derive(Default)]
pub(crate) struct CatchUp {
    pub prior: Option<PathBuf>,
    pub folded: Option<MarkFold>,
}

/// Runs until the sequencer's samples end: first taking a member's earlier history (`catch_up`)
/// beside the sequencer, after its first command; then flushing live mark reports, and backfilling
/// each listed mapped market once per process (a restart, or the markers on disk, skip completed
/// ones).
/// Error cases:
/// - a cache write fails, or the listed markets cannot be queried
pub async fn run(
    store: Store,
    // A weak sender: the task must never keep the sequencer's request channel open, or a shutdown
    // deadlocks — the sequencer would wait for this task's sender to drop while this task waits for
    // the sequencer's marks sender to drop.
    sequencer: mpsc::WeakSender<Request>,
    backfill: Option<BackfillSection>,
    sources: BTreeMap<MarketId, Source>,
    mut live: Live,
    catch_up: CatchUp,
) -> anyhow::Result<()> {
    let mut store = caught_up(store, catch_up, &mut live).await?;
    let candles = backfill.as_ref().map(|cfg| Candles::new(&cfg.base_url)).transpose()?;
    let mut flush = interval(FLUSH_EVERY);
    let mut backfill_tick = interval(BACKFILL_EVERY);
    let mut retry_at: BTreeMap<MarketId, Instant> = BTreeMap::new();
    loop {
        if !live.freezes.is_empty() {
            store = frozen(store, &mut live).await?;
        }
        tokio::select! {
            report = live.samples.recv() => {
                // The sequencer is gone: flush and end, as a shutdown does.
                let Some(report) = report else {
                    write(&mut store, &mut live.buffered)?;
                    return Ok(());
                };
                live.take(report);
                if live.buffered.len() >= BATCH {
                    write(&mut store, &mut live.buffered)?;
                    live.flushed_at = Instant::now();
                }
            }
            _ = flush.tick() => {
                if !live.buffered.is_empty() {
                    write(&mut store, &mut live.buffered)?;
                    live.flushed_at = Instant::now();
                }
            }
            _ = backfill_tick.tick(), if candles.is_some() && backfill.is_some() => {
                backfill_due(
                    &mut store,
                    candles.as_ref().expect("armed by the guard"),
                    backfill.as_ref().expect("armed by the guard"),
                    &sources,
                    &sequencer,
                    &mut retry_at,
                    &mut live,
                )
                .await?;
            }
        }
    }
}

/// Takes `work`, a member's earlier history, into `store` on a blocking thread, receiving the live
/// samples into the buffer meanwhile, so the sequencer's queue never fills and drops one, and writing
/// none of them before it: they extend the chains the catch-up writes below them. The backlog then
/// goes out a batch a write, the queue drained between writes.
/// Error cases:
/// - the catch-up fails or panics (see `Store::catch_up`), or a backlog write fails
async fn caught_up(mut store: Store, work: CatchUp, live: &mut Live) -> anyhow::Result<Store> {
    if work.prior.is_none() && work.folded.is_none() {
        return Ok(store);
    }
    let mut job = tokio::task::spawn_blocking(move || {
        store.catch_up(work.prior.as_deref(), work.folded)?;
        anyhow::Ok(store)
    });
    let mut open = true;
    let mut store = loop {
        tokio::select! {
            done = &mut job => break done.context("the marks catch-up stopped")??,
            report = live.samples.recv(), if open => match report {
                Some(report) => live.take(report),
                None => open = false,
            },
        }
    };
    while live.buffered.len() >= BATCH {
        let rest = live.buffered.split_off(BATCH);
        write(&mut store, &mut live.buffered)?;
        live.buffered = rest;
        while let Ok(report) = live.samples.try_recv() {
            live.take(report);
        }
        tokio::task::yield_now().await;
    }
    live.flushed_at = Instant::now();
    Ok(store)
}

/// Writes the frozen copy each freeze in `live` asks for (`Store::freeze`), once the samples buffered before it are
/// written and before any received after it is, and hands the cache's committed anchors then to the freeze's end:
/// the copy holds the marks of the commands through its checkpoint and none after, however long a backfill or a
/// catch-up held the samples. The copy is written on a blocking thread, the queue received into the buffer
/// meanwhile, so the sequencer's queue never fills and drops a sample. A copy that cannot be written is logged, and
/// its end gets nothing: the snapshot carries no marks anchors.
/// Error cases:
/// - the buffered samples cannot be written, or the copy's thread panics
async fn frozen(mut store: Store, live: &mut Live) -> anyhow::Result<Store> {
    write(&mut store, &mut live.buffered)?;
    live.flushed_at = Instant::now();
    let freezes = std::mem::take(&mut live.freezes);
    let mut job = tokio::task::spawn_blocking(move || {
        write_freezes(&mut store, freezes)?;
        anyhow::Ok(store)
    });
    let mut open = true;
    loop {
        tokio::select! {
            done = &mut job => return done.context("the marks freeze stopped")?,
            report = live.samples.recv(), if open => match report {
                Some(report) => live.take(report),
                None => open = false,
            },
        }
    }
}

/// Writes each of `freezes`' frozen copies in order (`Store::freeze`), handing its end the anchors, then the
/// samples received after it, before the next. A copy that cannot be written is logged, and its end gets nothing.
/// Error cases:
/// - the samples after a freeze cannot be written
pub(super) fn write_freezes(store: &mut Store, freezes: Vec<(Freeze, Vec<MarkSample>)>) -> anyhow::Result<()> {
    for (Freeze { offset, reply }, mut after) in freezes {
        match store.freeze(offset) {
            Ok(frozen) => drop(reply.try_send(frozen)),
            Err(e) => warn!(seq = offset.seq, "no frozen marks cache for the daily snapshot at {offset}: {e:#}"),
        }
        write(store, &mut after)?;
    }
    Ok(())
}

impl Live {
    /// Nothing buffered yet from `samples`.
    pub(crate) fn new(samples: mpsc::Receiver<Report>) -> Self {
        Self { samples, buffered: Vec::new(), flushed_at: Instant::now(), freezes: Vec::new() }
    }

    /// Buffers `report`'s sample, held behind the last freeze kept, if any, or keeps its freeze for the task to write
    /// after the samples received before it and before those after it.
    pub(crate) fn take(&mut self, report: Report) {
        match (report, self.freezes.last_mut()) {
            (Report::Sample(sample), Some((_, after))) => after.push(sample),
            (Report::Sample(sample), None) => self.buffered.push(sample),
            (Report::Freeze(freeze), _) => self.freezes.push((freeze, Vec::new())),
        }
    }
}

/// Flushes buffered samples into the cache, one transaction.
/// Error cases:
/// - the cache rejects the write
fn write(store: &mut Store, buffered: &mut Vec<MarkSample>) -> anyhow::Result<()> {
    store.observe(buffered).context("writing live marks")?;
    buffered.clear();
    Ok(())
}

/// Backfills one listed, mapped market whose recorded coverage is not the depths the config asks
/// for now, and retries a failed one after `RETRY_AFTER`. Which market is unspecified: the
/// listing order decides, one per tick.
/// Error cases:
/// - the listed markets cannot be queried, a completed backfill's marker cannot be written, or the
///   cache rejects a write during the backfill
async fn backfill_due(
    store: &mut Store,
    candles: &Candles,
    cfg: &BackfillSection,
    sources: &BTreeMap<MarketId, Source>,
    sequencer: &mpsc::WeakSender<Request>,
    retry_at: &mut BTreeMap<MarketId, Instant>,
    live: &mut Live,
) -> anyhow::Result<()> {
    // The sequencer is gone (shutdown): no listing to query, and nothing else to do.
    let Some(sequencer) = sequencer.upgrade() else { return Ok(()) };
    let (listed, engine_now) = listed_markets(&sequencer).await?;
    // No command sequenced yet (genesis waits for trusted time): no proven time to anchor on.
    if engine_now == 0 {
        return Ok(());
    }
    let coverage = coverage(cfg, store.retentions())?;
    let done = store.backfilled().context("reading backfilled markets")?;
    for market in listed {
        // A market is done when the recorded backfill covers the depths and retentions the config
        // now asks for; a wider retention re-runs it, because the original seed pruned more.
        let covered = done.get(&market).is_some_and(|recorded| *recorded == coverage);
        if covered || retry_at.get(&market).is_some_and(|at| Instant::now() < *at) {
            continue;
        }
        let Some(source) = sources.get(&market) else { continue };
        match backfill::seed_market(candles, source, market, cfg, engine_now, store, live).await {
            Ok(seeded) => {
                info!(market, venue = source.venue.name(), symbol = %source.symbol, seeded, "marks backfilled from the provider");
                store
                    .mark_backfilled(market, source.venue.name(), &source.symbol, engine_now, coverage)
                    .context("recording the backfill")?;
                retry_at.remove(&market);
            }
            Err(e) if e.downcast_ref::<backfill::StoreFault>().is_some() => return Err(e),
            Err(e) => {
                warn!(alert = "marks_backfill_failed", market, venue = source.venue.name(), "{e:#}");
                retry_at.insert(market, Instant::now() + RETRY_AFTER);
            }
        }
        return Ok(());
    }
    Ok(())
}

/// What a backfill under `cfg` covers once `retentions` have pruned it.
/// Error cases:
/// - a depth is out of range
pub(crate) fn coverage(cfg: &BackfillSection, r: crate::history::marks::Retentions) -> anyhow::Result<Coverage> {
    let ms =
        |depth: Duration, name: &str| Millis::try_from(depth.as_millis()).with_context(|| format!("backfill {name}"));
    Ok(Coverage {
        depths: (ms(cfg.depth_1m, "depth_1m")?, ms(cfg.depth_1h, "depth_1h")?, ms(cfg.depth_1d, "depth_1d")?),
        retentions: (r.s1, r.m1, r.h1, r.d1),
    })
}

/// The markets the engine lists now, in id order, with the engine's time: the admitted stamp the
/// backfill anchors its windows and pruning on, never the host clock.
/// Error cases:
/// - the sequencer does not answer
async fn listed_markets(sequencer: &mpsc::Sender<Request>) -> anyhow::Result<(Vec<MarketId>, Millis)> {
    let (reply, rx) = oneshot::channel();
    let query: Query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send((engine.markets().keys().copied().collect::<Vec<_>>(), engine.now()));
    });
    sequencer
        .send(Request::Query(query))
        .await
        .map_err(|_| anyhow::anyhow!("querying listed markets: the sequencer is gone"))?;
    rx.await.map_err(|_| anyhow::anyhow!("querying listed markets: the sequencer is gone"))
}
