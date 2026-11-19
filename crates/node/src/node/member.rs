//! A rotation-set member's life (#593). Between its turns the member follows the operator in effect
//! into a copy of the log with a live replica (`standby::Follower`), re-resolving the operator at each
//! turn boundary and whenever a stint fails. Once trusted time reaches the boundary of a turn the
//! finalised schedule names its release for, it reads everything its start needs from the chain and
//! takes a trusted time sample (`ready`), then takes only the tail past its tip from the turn's source
//! (`handover::tail`), and the node starts sequencing from the replica. Once it hands the keys to the
//! next turn's member, it follows that one, its replica rebuilt from the copy (`turn`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use engine_core::types::{AccountId, Millis};
use tracing::info;

use crate::checkpoints::CheckpointRpc;
use crate::config::{NodeConfig, TimeSection};
use crate::handover::assigned::Assigned;
use crate::history::HistoryPlan;
use crate::log_check::answer::{Answering, CheckedHeads};
use crate::log_heads::{Head, PostedHeads};
use crate::metrics::Metrics;
use crate::node::keys::{KeyStart, attestor_for, sealer};
use crate::node::price_feeds::Warm;
use crate::node::ready::Start;
use crate::node::secrets::Delivered;
use crate::standby::Follower;
use crate::stats::Stats;
use crate::tee::Attestor;
use crate::tee::clock::GuestClock;
use crate::tee::sealing::{KeyStore, Release, Sealer};
use crate::trusted_time::Sample;

mod prepare;
pub(crate) mod promoted;
pub(crate) mod standby;
pub(crate) mod turn;
mod warm;

/// How a node's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ended {
    /// A shutdown signal stopped it.
    Stopped,
    /// A successor confirmed the handover of its keys.
    HandedOver(Handed),
    /// The vault named another key than its enclave's, as a promotion does (#595): the generation it
    /// held is no longer its to hold in this process, and a member follows again, or starts as the
    /// operator if the promoted key is its own standby key.
    Replaced(Handed),
}

/// What a confirmed handover gave away: the generation the keys were sealed in, and the time trusted
/// time had reached, read once the stop the successor's admission triggered has drained the node,
/// which is at or past the successor's turn's boundary. The member never holds that generation again,
/// and its reach never falls below that time, so a later sample with a wider radius cannot give it
/// back the turn it handed over. Its log's last command, with the key that sealed it, is what its
/// member link answers until its replica is built: a successor's start check, in a set of two, has no
/// other member to ask. With a reader of the latest agreed block, `to` is the assignment the successor
/// bound, as the holder read it last before the keys went (#596): a skip a reorg later removes gives
/// the drawn turn back by it, whatever the member reads after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a handover's record names whom the turn went to"]
pub(crate) struct Handed {
    pub generation: Option<u64>,
    pub reached: Option<Millis>,
    pub head: Option<(Head, AccountId)>,
    pub to: Option<Assigned>,
}

impl Handed {
    /// What a run whose keys `store` sealed, if any, gives away, trusted time having reached `reached`,
    /// its log ending at `last`, a command those keys sealed.
    pub(super) fn of(store: Option<&KeyStore>, reached: Option<Millis>, last: Option<Head>) -> Self {
        let sealer = store.and_then(|store| store.keys().enclave_key().ok()).map(|key| AccountId(key.address()));
        Self { generation: store.map(KeyStore::generation), reached, head: last.zip(sealer), to: None }
    }

    /// What a run that ended before it sequenced gives away: the generation `store` sealed, if any.
    pub(super) fn sealed(store: Option<&KeyStore>) -> Self {
        Self::of(store, None, None)
    }
}

/// What a member's wait for its turn gives the node it starts.
pub(crate) enum Turn {
    /// The keys for this holding are sealed, or the turns do not apply: the node starts as it always
    /// has, from the log on disk.
    Hold,
    /// The tail extended the follower's copy: the node starts from its replica, with what the member
    /// read before connecting.
    Replica(Box<Start>),
    /// A shutdown signal arrived while the member followed.
    Stop,
}

/// A follower whose replica a node starts from, with what recovery would have rebuilt beside it.
pub(crate) struct Replica {
    pub follower: Follower,
    /// The dashboard record the replica fed.
    pub stats: Stats,
    /// The history index's check against the copy, made as the replica was built.
    pub history: HistoryPlan,
    /// The marks the replica folded, which the marks cache takes once the node starts.
    pub marks: std::rc::Rc<std::cell::RefCell<crate::history::marks::fold::MarkFold>>,
}

/// Whether this node runs as a rotation-set member: rotation on, sealed keys with a handover
/// listener, and no start flag that names its keys.
pub(super) fn is_member(cfg: &NodeConfig, start: &KeyStart) -> bool {
    cfg.operator.rotation_enabled
        && cfg.tee.sealing.as_ref().is_some_and(|sealing| sealing.handover.is_some())
        && !start.new_key
        && start.predecessor.is_none()
}

/// Sets a member's copy of the log aside before a whole-log take, whatever brings its start there:
/// the copy's segment files are its own writer's, so the holder's archive and segments land only on
/// a log directory that holds none. A node that is no member takes onto its log as it is.
/// Error cases:
/// - the copy cannot be moved (see `turn::set_copy_aside`)
pub(super) fn before_whole_log_take(cfg: &NodeConfig, start: &KeyStart) -> anyhow::Result<()> {
    if is_member(cfg, start) {
        turn::set_copy_aside(&cfg.node.log_dir)?;
    }
    Ok(())
}

/// Runs the node, and, for a member, follows between its holdings: each `wait` gives the turn the
/// next `serve` starts from, and a run a successor's confirmation or a replacement ended goes back to
/// `wait`, so the incumbent follows its successor. Both learn what the last such end gave away. A node
/// that is no member serves once.
/// Error cases:
/// - `wait` or `serve` fails
pub(super) async fn run_turns(
    member: bool,
    mut wait: impl AsyncFnMut(Option<Handed>) -> anyhow::Result<Turn>,
    mut serve: impl AsyncFnMut(Option<Box<Start>>, Option<Handed>) -> anyhow::Result<Ended>,
) -> anyhow::Result<()> {
    let mut handed = None;
    loop {
        let replica = match member {
            true => match wait(handed).await? {
                Turn::Hold => None,
                Turn::Replica(start) => Some(start),
                Turn::Stop => return Ok(()),
            },
            false => None,
        };
        match serve(replica, handed).await? {
            Ended::HandedOver(gave) if member => {
                info!(generation = ?gave.generation, reached = ?gave.reached, "handed the keys over: following the successor");
                handed = Some(gave);
            }
            Ended::Replaced(gave) if member => {
                info!(generation = ?gave.generation, reached = ?gave.reached, "the vault names another key: following again");
                handed = Some(gave);
            }
            Ended::HandedOver(_) | Ended::Replaced(_) | Ended::Stopped => return Ok(()),
        }
    }
}

/// Where a member's trusted time samples come from.
pub(crate) trait Sampling {
    /// A fresh sample.
    /// Error cases:
    /// - no sample can be taken
    async fn sample(&self) -> anyhow::Result<Sample>;
}

/// The Roughtime servers `[time]` names.
pub(crate) struct Roughtime(TimeSection);

impl Sampling for Roughtime {
    async fn sample(&self) -> anyhow::Result<Sample> {
        let clock: Arc<dyn crate::clock::Clock> = Arc::new(crate::clock::SystemClock);
        crate::trusted_time::sample_once(&self.0, &clock).await
    }
}

/// Where a member reads the latest posted log head.
pub(crate) trait PostedHead {
    /// The newest head the vault's current signer epoch holds, if any.
    /// Error cases:
    /// - the read fails
    async fn posted_head(&self) -> anyhow::Result<Option<Head>>;
}

impl PostedHead for PostedHeads {
    async fn posted_head(&self) -> anyhow::Result<Option<Head>> {
        self.latest().await
    }
}

/// A member between its holdings: what it reads the vault, the posted heads and trusted time
/// through, its keys' sealing, and its last trusted time sample.
pub(crate) struct Member<'a, C = CheckpointRpc, H = PostedHeads, T = Roughtime> {
    cfg: &'a NodeConfig,
    metrics: Arc<Metrics>,
    vault: C,
    heads: Option<H>,
    time: T,
    sealer: Box<dyn Sealer>,
    attestor: Box<dyn Attestor>,
    release: Release,
    sample: Option<Sample>,
    /// The operator's start flags, which open the sealed certificate as a start does.
    start: KeyStart,
    /// What this member's last confirmed handover gave away.
    handed: Option<Handed>,
    /// The alert episodes its following opened, kept across its waits.
    episodes: turn::Episodes,
    /// Set by a shutdown signal while it waits, which a replica's build, holding its future while it
    /// replays, checks at every entry.
    stopping: Arc<AtomicBool>,
    /// The checkpoints its replica verified, kept across rebuilds, which its log check answers read.
    checked: CheckedHeads,
    /// Its answers to the operator's log checks while it follows, without `[tee.sealing.handover]`
    /// none.
    answering: Option<Arc<Answering>>,
    /// What it reads a turn's reassignments through, and whom it follows past them (#596).
    fallback: turn::Fallback,
    /// Held for each send from the liveness caller's account, shared with the process's fallback fixer.
    sends: Arc<crate::liveness::fallback::Sends>,
    /// The delivered secrets its venue feeds authenticate with, where it opens them before its turn.
    delivered: Option<&'a Delivered>,
    /// The venue feeds it opened for its own next turn, which the node it starts takes (`warm`).
    warm: Option<Warm>,
}

impl<'a> Member<'a> {
    /// The member `cfg` runs, with `start`'s stand-in measurement under the mock TEE.
    /// Error cases:
    /// - the vault, the posted-head reader, the sealing or the attestor cannot be built
    pub(super) fn new(
        cfg: &'a NodeConfig,
        start: &KeyStart,
        metrics: Arc<Metrics>,
        sends: Arc<crate::liveness::fallback::Sends>,
    ) -> anyhow::Result<Self> {
        let vault = crate::handover::vault_reader(cfg)?.context("a rotation-set member reads the vault")?;
        let sealer = sealer(cfg.tee.provider, start)?;
        let release = Release { launch_digest: sealer.platform()?.launch_digest, config_sha256: cfg.file_sha256 };
        let attestor = attestor_for(cfg.tee.provider, &GuestClock::default())?;
        let (heads, time) = (PostedHeads::new(cfg)?, Roughtime(cfg.time.clone()));
        let checked = CheckedHeads::default();
        let quoting = attestor_for(cfg.tee.provider, &GuestClock::default())?;
        let answering = Answering::new(cfg, quoting, sealer.platform()?, checked.clone())?.map(Arc::new);
        let fallback = turn::Fallback::for_config(cfg)?;
        Ok(Self {
            cfg,
            metrics,
            vault,
            heads,
            time,
            sealer,
            attestor,
            release,
            sample: None,
            start: start.clone(),
            handed: None,
            episodes: Default::default(),
            stopping: Arc::default(),
            checked,
            answering,
            fallback,
            sends,
            delivered: None,
            warm: None,
        })
    }

    /// This member, opening its venue feeds a lead before each of its turns with `delivered`'s
    /// credentials (`warm`).
    pub(super) fn with_feeds(self, delivered: &'a Delivered) -> Self {
        Self { delivered: Some(delivered), ..self }
    }
}

impl<'a, T: Sampling> Member<'a, CheckpointRpc, PostedHeads, T> {
    /// The member `cfg` runs under the mock TEE's `sealer`, reading trusted time through `time` and no
    /// posted heads: the daily rotation scenario's, on its test clock.
    #[cfg(test)]
    pub(crate) fn on_clock(cfg: &'a NodeConfig, metrics: Arc<Metrics>, sealer: Box<dyn Sealer>, time: T) -> Self {
        let vault = crate::handover::vault_reader(cfg).unwrap().expect("a member reads the vault");
        let release =
            Release { launch_digest: sealer.platform().unwrap().launch_digest, config_sha256: cfg.file_sha256 };
        let attestor: Box<dyn Attestor> = Box::new(crate::tee::MockAttestor);
        let start = KeyStart { mock_launch_digest: Some(release.launch_digest), ..Default::default() };
        let episodes = Default::default();
        let checked = CheckedHeads::default();
        let platform = sealer.platform().unwrap();
        let answering = Answering::new(cfg, Box::new(crate::tee::MockAttestor), platform, checked.clone()).unwrap();
        Self {
            cfg,
            metrics,
            vault,
            heads: None,
            time,
            sealer,
            attestor,
            release,
            sample: None,
            start,
            handed: None,
            episodes,
            stopping: Arc::default(),
            checked,
            answering: answering.map(Arc::new),
            fallback: Default::default(),
            sends: Arc::default(),
            delivered: None,
            warm: None,
        }
    }
}

impl<C: crate::checkpoints::CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// Records what a confirmed handover gave away, which this member never holds again.
    pub(super) fn handed_over(&mut self, handed: Handed) {
        self.handed = Some(handed);
    }

    /// `until_turn`, or `Turn::Stop` once a shutdown signal arrives.
    /// Error cases:
    /// - see `until_turn`, or the shutdown signal cannot be installed
    pub(super) async fn wait(&mut self) -> anyhow::Result<Turn> {
        // On a task of its own, so a signal is seen while a replica's build holds this future.
        let mut watching = tokio::task::JoinSet::new();
        let stopping = self.stopping.clone();
        watching.spawn(async move {
            if crate::node::shutdown::shutdown_signal().await.is_ok() {
                stopping.store(true, Ordering::Release);
            }
        });
        tokio::select! {
            turn = self.until_turn_answering() => turn,
            signal = crate::node::shutdown::shutdown_signal() => signal.map(|()| {
                info!("shutting down while following");
                Turn::Stop
            }),
        }
    }

    /// `until_turn`, answering the operator's log checks on the handover endpoint meanwhile
    /// (`log_check::answer`); the endpoint is free again once it returns. Where the liveness rule's
    /// standby side runs, its member link holds the endpoint and answers them (`liveness::link`).
    /// Error cases:
    /// - see `until_turn`
    pub(crate) async fn until_turn_answering(&mut self) -> anyhow::Result<Turn> {
        let answering = self.answering.clone().filter(|_| !standby::applies(self.cfg));
        let Some(answering) = answering else { return self.until_turn().await };
        tokio::select! {
            turn = self.until_turn() => turn,
            never = answering.run() => match never {},
        }
    }
}

#[cfg(test)]
mod tests;
