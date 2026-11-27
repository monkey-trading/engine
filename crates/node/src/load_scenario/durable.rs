//! What makes the load node one with `[snapshots]`, as `recovery_scale`'s paired node is: its log read
//! as a paired recovery reads one, its history owner publishing a recovery cache at every capture the
//! sequencer hands it, each checkpoint reported final at once, as a chain that keeps up would, so the
//! captures have the finalised witness they need, and the owner stopped after the sequencer's drain
//! has handed it the shutdown cache. A node started again on the same directory reads its newest
//! cache and the commands past it.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::api::{Feed, MAX_BACKFILLS};
use crate::checkpoints::{Health, Work};
use crate::clock::SystemClock;
use crate::common::{CHAIN_ID, VAULT};
use crate::config::{HistorySection, NodeConfig};
use crate::daily_snapshots::{Bounds, NoRoots, snapshot_start_against};
use crate::history::derived::Deriver;
use crate::history::runtime::Stopper;
use crate::history::{History, HistoryPlan};
use crate::load_scenario::archived::Archived;
use crate::log::{CheckpointDomain, CheckpointParent, Manifest, ReplayWorkers};
use crate::sequencer::recovery_scale::flow::config;
use crate::sequencer::recovery_scale::paired::node_config;
use crate::sequencer::{ReplayedLog, Request, Sequencer, SequencerConfig, SignedWithdrawal, StreamMsg};
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
    /// The newest checkpoint reported final, which a chain that keeps up would give a restart as the vault's
    /// finalized one: the parent a recovery reads its log under.
    pub finalized: Arc<Mutex<CheckpointParent>>,
    /// The bucket and vault of the launch configuration, when the node runs it in place of `[snapshots]`.
    pub archived: Option<Arc<Archived>>,
}

impl Durable {
    /// The run in `run`, configured as `recovery_scale`'s paired node is (`paired::node_config`): its log
    /// in `run/log`, its caches, with the history index they pair with, in `run/snapshots`.
    pub(super) fn new(run: &Path, interval: Duration) -> Self {
        let key = Arc::new(EnclaveKey::generate());
        let cfg = node_config(run, interval, (AccountId(key.address()), ReplayWorkers::available()));
        Durable { cfg, key, finalized: Arc::default(), archived: None }
    }

    /// This run as the launch configuration has it, its snapshots counted by days of `archived.day`: no
    /// `[snapshots]`, so its caches and history index are not run, its daily snapshots beside the log in `run`.
    pub(super) fn launching(mut self, run: &Path, archived: Arc<Archived>) -> Self {
        archived.configure(&mut self.cfg, run);
        self.archived = Some(archived);
        self
    }

    /// The sequencer's config for a node on `dir`, as `recovery_scale`'s node has it, and the queue
    /// of its checkpoints' work: the book's deployment, operator and screening source, no genesis and
    /// no ceilings of its own, the book's markets carrying both.
    pub(super) fn sequencer<'a>(dir: &'a Path, manifest: watch::Sender<Manifest>) -> (SequencerConfig<'a>, Reports) {
        let (sink, queued) = mpsc::channel(1 << 16);
        (config(dir, Arc::new(SystemClock), sink, manifest, None), Reports(queued))
    }

    /// The directory the caches and their history index are in, which the launch configuration has none of.
    pub(super) fn snapshots(&self) -> Option<&Path> {
        self.cfg.snapshots.as_ref().map(|snapshots| snapshots.dir.as_path())
    }

    /// The directory the daily snapshots are in, under the launch configuration.
    pub(super) fn daily_snapshots(&self) -> Option<&Path> {
        self.cfg.daily_snapshots.as_ref().map(|daily| daily.dir.as_path())
    }
}

/// The history owner, planned and not yet running. The paired index takes captures and stops on request; the
/// launch configuration's unpaired index (`indexed`) does neither and carries the section its progress is read from.
pub(super) struct Pending {
    history: History,
    sealing: Option<Endpoint>,
    stopper: Option<Stopper>,
    feed: broadcast::Receiver<Arc<StreamMsg>>,
    unpaired: Option<HistorySection>,
}

/// The running history owner.
pub(super) struct Owner {
    task: JoinHandle<anyhow::Result<()>>,
    sealing: Option<Endpoint>,
    stopper: Option<Stopper>,
    manifest: watch::Receiver<Manifest>,
    unpaired: Option<HistorySection>,
}

/// The work of the checkpoints the sequencer has written.
pub(super) struct Reports(mpsc::Receiver<Work>);

impl Pending {
    /// The unpaired history index `history` of the launch configuration, taking `feed`, whose progress is read from
    /// `section`'s directory.
    pub(super) fn unpaired(
        history: History,
        feed: broadcast::Receiver<Arc<StreamMsg>>,
        section: HistorySection,
    ) -> Self {
        Pending { history, sealing: None, stopper: None, feed, unpaired: Some(section) }
    }

    /// Runs the history owner on the stream and the log in `dir`, from where its index resumes, once
    /// `seq` has taken its first command.
    pub(super) fn run(self, seq: &Sequencer, dir: &Path, manifest: &watch::Receiver<Manifest>) -> Owner {
        let Pending { mut history, sealing, stopper, feed, unpaired } = self;
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
        Owner { task: tokio::spawn(running), sealing, stopper, manifest: manifest.clone(), unpaired }
    }
}

impl Owner {
    /// Stops the owner once the cache the sequencer's drain handed it is published, which it has
    /// `PUBLISH_WITHIN` to do. An unpaired index ends with its feed, which closes with the sequencer, and has the
    /// same time to take the log's last commands.
    /// Error cases:
    /// - the owner failed, or had not published the cache or taken the log in time, and was stopped where it stood
    pub(super) async fn stop(mut self) -> anyhow::Result<()> {
        let Some(sealing) = &self.sealing else { return self.finish("taken the log's last commands").await };
        if timeout(PUBLISH_WITHIN, sealing.idle()).await.is_err() {
            self.task.abort();
            let _ = self.task.await;
            anyhow::bail!("the history owner had not published the final cache after {PUBLISH_WITHIN:?}");
        }
        if let Some(stopper) = &self.stopper {
            stopper.stop();
        }
        shut_down(self.task.await.context("the history owner's task")?)
    }

    /// Whether the owner is running and takes captures: its task has not ended and, for the paired index, its
    /// queue is open.
    pub(super) fn takes_captures(&self) -> bool {
        !self.task.is_finished() && !self.sealing.as_ref().is_some_and(Endpoint::is_closed)
    }

    /// How many commands the log holds on disk that the owner's index has yet to take, or `None` when the
    /// owner does not answer within `INSPECT`, or an unpaired index's progress cannot be read.
    pub(super) async fn behind(&self) -> Option<u64> {
        let flushed = self.manifest.borrow().flushed.map_or(0, |at| at.seq);
        let indexed = match (&self.sealing, &self.unpaired) {
            (Some(sealing), _) => {
                let state = timeout(INSPECT, sealing.inspect()).await.ok()?.ok()?;
                state.last_entry.map_or(0, |at| at.seq)
            }
            (None, Some(section)) => {
                let section = HistorySection { dir: section.dir.clone(), retention: section.retention, marks: None };
                let plan = tokio::task::spawn_blocking(move || HistoryPlan::open(&section)).await.ok()?.ok()?;
                plan.last_seq().unwrap_or(0)
            }
            (None, None) => return None,
        };
        Some(flushed.saturating_sub(indexed))
    }

    /// Stops the owner as a crash does, with no wait for the shutdown cache: it ends its current turn. An
    /// unpaired index has no turn to end and takes what its feed still holds, as a stop does.
    /// Error cases:
    /// - the owner had failed
    pub(super) async fn abort(mut self) -> anyhow::Result<()> {
        let Some(stopper) = &self.stopper else { return self.finish("taken what its feed held").await };
        stopper.stop();
        shut_down(self.task.await.context("the history owner's task")?)
    }

    /// Waits `PUBLISH_WITHIN` for the unpaired index's task to end with its feed, which it does once it has taken the
    /// log's last commands.
    /// Error cases:
    /// - the index failed, or had not ended in time and was stopped where it stood
    async fn finish(&mut self, what: &str) -> anyhow::Result<()> {
        match timeout(PUBLISH_WITHIN, &mut self.task).await {
            Ok(ended) => shut_down(ended.context("the history owner's task")?),
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                anyhow::bail!("the history index had not {what} after {PUBLISH_WITHIN:?}")
            }
        }
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
    // A fresh log's first read is from genesis, with no checkpoint; a recovery's is under the last one
    // reported final, as a start reads it under the vault's, and its sequencer is gated on it.
    let (parent, read_stats) = match recovering {
        true => (*durable.finalized.lock().unwrap_or_else(PoisonError::into_inner), Some(stats.clone())),
        false => (CheckpointParent::default(), None),
    };
    cfg.checkpoint_parent = parent;
    let (log, plan) = Sequencer::read_paired_log(
        &durable.cfg,
        Some(signer),
        Some(parent),
        Default::default(),
        read_stats,
        None,
        |_| {},
    )?;
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
    Ok((seq, Pending { history, sealing: Some(sealing), stopper: Some(stopper), feed, unpaired: None }))
}

/// Recovers the sequencer on the log in `dir` as the launch configuration starts one: the log read from the newest
/// daily snapshot whose checkpoint the vault has finalized, signed by this node's key, and the log after it, or,
/// for a fresh run, an empty log. The log before that snapshot may be off the host.
/// Error cases:
/// - no daily snapshot serves the start, or the log after it does not read, or the sequencer refuses it
pub(super) async fn open_launch(
    durable: &Durable,
    mut cfg: SequencerConfig<'_>,
    (stats, recovering): (&Stats, bool),
    stream: &broadcast::Sender<Arc<StreamMsg>>,
    withdrawals: mpsc::UnboundedSender<SignedWithdrawal>,
) -> anyhow::Result<Sequencer> {
    if !recovering {
        return Sequencer::recover(cfg, durable.key.clone(), stream.clone(), withdrawals);
    }
    let parent = *durable.finalized.lock().unwrap_or_else(PoisonError::into_inner);
    cfg.checkpoint_parent = parent;
    let domain = CheckpointDomain { chain_id: CHAIN_ID, vault: VAULT };
    let log = read_launch_log(durable, (cfg.log_dir, domain), parent, stats).await?;
    Sequencer::recover_from(log, cfg, durable.key.clone(), stream.clone(), withdrawals)
}

/// Reads the log in `log_dir`, checkpointed under `domain`, from the newest daily snapshot the vault's finalized checkpoint `parent` allows, with
/// `stats` holding the dashboard record the snapshot carries, or one started over when it carries none, for the
/// replay of the log after it to record into.
/// Error cases:
/// - no daily snapshot serves the start, or the log after it does not read
async fn read_launch_log(
    durable: &Durable,
    (log_dir, domain): (&Path, CheckpointDomain),
    parent: CheckpointParent,
    stats: &Stats,
) -> anyhow::Result<ReplayedLog> {
    let signer = AccountId(durable.key.address());
    let daily = durable.daily_snapshots().context("[daily_snapshots]")?;
    let bounds = Bounds { finalized: parent.seq, before: None, keep_from: None };
    let signers = BTreeSet::from([signer]);
    let mut start = snapshot_start_against(None, (daily, log_dir), (domain, &signers), bounds, &NoRoots)
        .await
        .context("no daily snapshot serves the start")?;
    match crate::daily_snapshots::History::read(start.snapshot.history.take().as_deref()).stats {
        Some(record) => stats.restore(record),
        None => stats.restart(),
    }
    let trust = (Some(domain), Some(signer), Some(parent));
    Sequencer::read_log_starting(
        start,
        log_dir,
        trust,
        Default::default(),
        |_| {},
        None,
        Some(stats.clone()),
        None,
        ReplayWorkers::available(),
    )
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
pub(super) fn report_final(
    mut reports: Reports,
    tx: mpsc::WeakSender<Request>,
    finalized: Arc<Mutex<CheckpointParent>>,
    archived: Option<Arc<Archived>>,
) {
    // A restarted node's gate starts with its collateral unvalidated and withholds every checkpoint until
    // custody says it is not: the check a node's poster makes at its start, which the parent's passes.
    let revalidation = revalidation(&finalized.lock().unwrap_or_else(PoisonError::into_inner));
    tokio::spawn(async move {
        if let (Some(status), Some(tx)) = (revalidation, tx.upgrade()) {
            let _ = tx.send(status).await;
        }
        while let Some(work) = reports.0.recv().await {
            note_final(&finalized, work.checkpoint.seq, &work.checkpoint.root);
            if let Some(archived) = &archived {
                archived.finalize_later(work.checkpoint.seq, work.checkpoint.root.clone());
            }
            // Once the node's senders are gone the drain's own checkpoint arrives: it is taken, not reported.
            let Some(tx) = tx.upgrade() else { continue };
            // A sequencer that has stopped answers nothing; the loop ends when its queue does.
            let _ = tx.send(final_status(work.checkpoint.seq)).await;
        }
    });
}

/// What a restart reports before anything else: its parent final and the collateral healthy, when it has one.
fn revalidation(parent: &CheckpointParent) -> Option<Request> {
    parent.has_checkpoint.then(|| final_status(parent.seq))
}

/// The report that the checkpoint at `seq` is final and custody's collateral healthy.
fn final_status(seq: u64) -> Request {
    Request::CheckpointStatus { posted: Some(seq), latest: None, health: Some(Health::Healthy), lifecycle: None }
}

/// Records the checkpoint at `seq` with state root `root` as the newest final, if it is newer than the one
/// held and its root parses.
fn note_final(finalized: &Mutex<CheckpointParent>, seq: u64, root: &str) {
    let Ok(root) = root.parse() else { return };
    let mut held = finalized.lock().unwrap_or_else(PoisonError::into_inner);
    if !held.has_checkpoint || seq > held.seq {
        *held = CheckpointParent { has_checkpoint: true, seq, root };
    }
}

#[cfg(test)]
mod tests {
    use anyhow::{Context, anyhow};

    use super::*;

    /// A restart from a daily snapshot goes on from the dashboard record the snapshot carries, so the series and
    /// activity from before it survive, with the log before the snapshot off the host.
    #[tokio::test]
    async fn a_launch_restart_restores_the_dashboard_record_its_daily_snapshot_carries() {
        use crate::node::recovery::history_tests::{Written, remove_segments_before};

        let written = Written::run("soak-launch-restart-record").await;
        remove_segments_before(&written.cfg.node.log_dir, written.snapshot.seq);
        let root = written.snapshot.root.parse().expect("the snapshot's checkpoint root");
        let parent = CheckpointParent { has_checkpoint: true, seq: written.snapshot.seq, root };
        // The domain `Written`'s node checkpoints under.
        let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
        let (stats, log_dir) = (Stats::new(&written.cfg).expect("stats"), written.cfg.node.log_dir.clone());
        let durable =
            Durable { cfg: written.cfg, key: written.key, finalized: Arc::new(Mutex::new(parent)), archived: None };
        let log = read_launch_log(&durable, (&log_dir, domain), parent, &stats).await.expect("the start");
        assert_eq!(log.snapshot_sequence(), Some(written.snapshot.seq), "the start went on from the snapshot");
        assert_eq!(stats.dump(), written.stats.dump(), "the dashboard record");
    }

    #[test]
    fn an_owner_closing_with_its_input_is_a_shutdown_and_any_other_error_is_not() {
        assert!(shut_down(Ok(())).is_ok());
        let closed =
            Err::<(), _>(anyhow!("channel closed")).context("history owner stopped before draining its physical input");
        assert!(shut_down(closed).is_ok());
        let failed = Err::<(), _>(anyhow!("the index could not be written")).context("indexing");
        assert!(shut_down(failed).is_err());
    }

    #[test]
    fn a_restart_revalidates_under_its_parent_and_a_fresh_run_has_none_to_report() {
        assert!(revalidation(&CheckpointParent::default()).is_none());
        let parent = CheckpointParent { has_checkpoint: true, seq: 5, root: Default::default() };
        let Some(Request::CheckpointStatus { posted, health, .. }) = revalidation(&parent) else { panic!("a report") };
        assert_eq!((posted, health), (Some(5), Some(Health::Healthy)));
    }

    #[test]
    fn a_status_names_its_checkpoint_final_and_the_collateral_healthy() {
        let Request::CheckpointStatus { posted, health, .. } = final_status(7) else { panic!("a status") };
        assert_eq!((posted, health), (Some(7), Some(Health::Healthy)));
    }

    #[test]
    fn the_newest_final_checkpoint_is_the_recoverys_parent_and_never_goes_back() {
        let root = |byte: u8| format!("0x{}", format!("{byte:02x}").repeat(32));
        let finalized = Mutex::new(CheckpointParent::default());
        note_final(&finalized, 1_000, &root(1));
        note_final(&finalized, 2_000, &root(2));
        note_final(&finalized, 1_500, &root(3));
        note_final(&finalized, 3_000, "not a root");
        let held = *finalized.lock().unwrap();
        assert_eq!(
            (held.has_checkpoint, held.seq),
            (true, 2_000),
            "a later one replaces, an older or unreadable one not"
        );
        assert_eq!(held.root.to_string(), root(2));
    }
}
