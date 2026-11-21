//! What makes the load node one with `[snapshots]`, as `recovery_scale`'s paired node is: its log read
//! as a paired recovery reads one, its history owner publishing a recovery cache at every capture the
//! sequencer hands it, each checkpoint reported final at once, as a chain that keeps up would, so the
//! captures have the finalised witness they need, and the owner stopped after the sequencer's drain
//! has handed it the shutdown cache. A node started again on the same directory reads its newest
//! cache and the commands past it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::api::{Feed, MAX_BACKFILLS};
use crate::checkpoints::{Health, Work};
use crate::clock::SystemClock;
use crate::config::NodeConfig;
use crate::history::derived::Deriver;
use crate::history::runtime::Stopper;
use crate::history::{History, HistoryPlan};
use crate::log::{CheckpointParent, Manifest, ReplayWorkers};
use crate::sequencer::recovery_scale::flow::config;
use crate::sequencer::recovery_scale::paired::node_config;
use crate::sequencer::{Request, Sequencer, SequencerConfig, SignedWithdrawal, StreamMsg};
use crate::snapshot::sealing::Endpoint;
use crate::stats::Stats;
use crate::tee::EnclaveKey;

/// How long the owner has to publish the shutdown cache.
const PUBLISH_WITHIN: Duration = Duration::from_secs(120);
/// How long the owner has to answer a question of how far its index is behind.
const INSPECT: Duration = Duration::from_secs(5);

/// The node config of a durable run whose log is in `run`, caches every `interval` of sequenced time,
/// and the key that signs it and pins its caches.
pub(super) struct Durable {
    pub cfg: NodeConfig,
    pub key: Arc<EnclaveKey>,
}

impl Durable {
    /// The run in `run`, configured as `recovery_scale`'s paired node is (`paired::node_config`): its log
    /// in `run/log`, its caches, with the history index they pair with, in `run/snapshots`.
    pub(super) fn new(run: &Path, interval: Duration) -> Self {
        let key = Arc::new(EnclaveKey::generate());
        let cfg = node_config(run, interval, (AccountId(key.address()), ReplayWorkers::available()));
        Durable { cfg, key }
    }

    /// The sequencer's config for a node on `dir`, as `recovery_scale`'s node has it, and the queue
    /// of its checkpoints' work: the book's deployment, operator and screening source, no genesis and
    /// no ceilings of its own, the book's markets carrying both.
    pub(super) fn sequencer<'a>(dir: &'a Path, manifest: watch::Sender<Manifest>) -> (SequencerConfig<'a>, Reports) {
        let (sink, queued) = mpsc::channel(1 << 16);
        (config(dir, Arc::new(SystemClock), sink, manifest, None), Reports(queued))
    }

    /// The directory the caches and their history index are in.
    pub(super) fn snapshots(&self) -> &Path {
        &self.cfg.snapshots.as_ref().expect("[snapshots]").dir
    }
}

/// The history owner, planned and not yet running.
pub(super) struct Pending {
    history: History,
    sealing: Endpoint,
    stopper: Stopper,
    feed: broadcast::Receiver<Arc<StreamMsg>>,
}

/// The running history owner.
pub(super) struct Owner {
    task: JoinHandle<anyhow::Result<()>>,
    sealing: Endpoint,
    stopper: Stopper,
    manifest: watch::Receiver<Manifest>,
}

/// The work of the checkpoints the sequencer has written.
pub(super) struct Reports(mpsc::Receiver<Work>);

impl Pending {
    /// Runs the history owner on the stream and the log in `dir`, from where its index resumes, once
    /// `seq` has taken its first command.
    pub(super) fn run(self, seq: &Sequencer, dir: &Path, manifest: &watch::Receiver<Manifest>) -> Owner {
        let Pending { mut history, sealing, stopper, feed } = self;
        history.catch_up_to(seq.last_seq());
        let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
        let feed = Feed::new(feed, dir.to_owned(), manifest.clone(), backfills, Some(history.resume_at()));
        let deriver = Deriver::new(seq.derived_rows(), Some((dir.to_owned(), manifest.clone())));
        let running = async move {
            let ended = history.run(feed, deriver).await;
            if let Err(error) = &ended {
                println!("SOAK the history owner stopped: {error:#}");
            }
            ended
        };
        Owner { task: tokio::spawn(running), sealing, stopper, manifest: manifest.clone() }
    }
}

impl Owner {
    /// Stops the owner once the cache the sequencer's drain handed it is published, which it has
    /// `PUBLISH_WITHIN` to do.
    /// Error cases:
    /// - the owner failed, or had not published the cache in time, and was stopped where it stood
    pub(super) async fn stop(self) -> anyhow::Result<()> {
        if timeout(PUBLISH_WITHIN, self.sealing.idle()).await.is_err() {
            self.task.abort();
            let _ = self.task.await;
            anyhow::bail!("the history owner had not published the final cache after {PUBLISH_WITHIN:?}");
        }
        self.stopper.stop();
        shut_down(self.task.await.context("the history owner's task")?)
    }

    /// Whether the owner is running and takes captures: its task has not ended and its queue is open.
    pub(super) fn takes_captures(&self) -> bool {
        !self.task.is_finished() && !self.sealing.is_closed()
    }

    /// How many commands the log holds on disk that the owner's index has yet to take, or `None` when the
    /// owner does not answer within `INSPECT`.
    pub(super) async fn behind(&self) -> Option<u64> {
        let flushed = self.manifest.borrow().flushed.map_or(0, |at| at.seq);
        let state = timeout(INSPECT, self.sealing.inspect()).await.ok()?.ok()?;
        Some(flushed.saturating_sub(state.last_entry.map_or(0, |at| at.seq)))
    }

    /// Stops the owner as a crash does, with no wait for the shutdown cache: it ends its current turn.
    /// Error cases:
    /// - the owner had failed
    pub(super) async fn abort(self) -> anyhow::Result<()> {
        self.stopper.stop();
        shut_down(self.task.await.context("the history owner's task")?)
    }
}

/// Reads the log in `durable`'s run, as a paired recovery does, and recovers the sequencer on it under
/// `cfg`, with its history owner planned and a feed of the stream. A fresh run reads an empty log; a
/// recovering one reads its newest cache and the commands past it.
/// Error cases:
/// - the log or its caches do not read, a recovery has no cache, or the sequencer refuses the log
pub(super) fn open(
    durable: &Durable,
    mut cfg: SequencerConfig<'_>,
    (stats, recovering): (&Stats, bool),
    stream: &broadcast::Sender<Arc<StreamMsg>>,
    withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
) -> anyhow::Result<(Sequencer, Pending)> {
    let signer = AccountId(durable.key.address());
    // A gated log's first read, as `Sequencer::recover` reads one: from genesis, with no checkpoint.
    let (parent, read_stats) = match recovering {
        true => (None, Some(stats.clone())),
        false => (Some(CheckpointParent::default()), None),
    };
    let (log, plan) =
        Sequencer::read_paired_log(&durable.cfg, Some(signer), parent, Default::default(), read_stats, None, |_| {})?;
    ensure!(
        log.snapshot_sequence().is_some() == recovering,
        "a recovery starts from a cache, and a fresh run from none"
    );
    let feed = stream.subscribe();
    log.validate_recovery(&cfg, signer)?;
    let (log, history) = HistoryPlan::authenticated(plan)?.finish_with_log(log, durable.key.clone())?;
    let sealing = history.sealing().context("authenticated history takes captures")?;
    let stopper = history.stopper().context("authenticated history stops on request")?;
    cfg.history_sealing = Some(sealing.clone());
    let seq = Sequencer::recover_from(log, cfg, durable.key.clone(), stream.clone(), withdrawals)?;
    Ok((seq, Pending { history, sealing, stopper, feed }))
}

/// What the owner returned once it was stopped. A node whose sequencer has gone leaves the owner's input
/// closed, which it reports as it stops: that is the shutdown, not a failure. Any other error is one.
fn shut_down(ended: anyhow::Result<()>) -> anyhow::Result<()> {
    match ended {
        Err(error) if format!("{error:#}").contains("before draining its physical input") => Ok(()),
        other => other,
    }
}

/// Reports each checkpoint final to the sequencer as it is written, and takes the work of those it writes
/// while it drains, until the sequencer drops its queue: a receiver gone sooner would raise
/// `checkpoint_post_failed` at the drain's final checkpoint.
pub(super) fn report_final(mut reports: Reports, tx: mpsc::WeakSender<Request>) {
    tokio::spawn(async move {
        while let Some(work) = reports.0.recv().await {
            // Once the node's senders are gone the drain's own checkpoint arrives: it is taken, not reported.
            let Some(tx) = tx.upgrade() else { continue };
            let status = Request::CheckpointStatus {
                posted: Some(work.checkpoint.seq),
                latest: None,
                health: Some(Health::Healthy),
                lifecycle: None,
            };
            // A sequencer that has stopped answers nothing; the loop ends when its queue does.
            let _ = tx.send(status).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, anyhow};

    use super::*;

    #[test]
    fn an_owner_closing_with_its_input_is_a_shutdown_and_any_other_error_is_not() {
        assert!(shut_down(Ok(())).is_ok());
        let closed =
            Err::<(), _>(anyhow!("channel closed")).context("history owner stopped before draining its physical input");
        assert!(shut_down(closed).is_ok());
        let failed = Err::<(), _>(anyhow!("the index could not be written")).context("indexing");
        assert!(shut_down(failed).is_err());
    }
}
