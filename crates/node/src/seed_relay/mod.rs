//! The seed relayer: it proves on Ethereum the RANDAO mix that seeds each daily operator turn, relays
//! it to the seed receiver on Arbitrum (`contracts/src/SeedRelay.sol`, `SeedReceiver.sol`), and
//! watches that each turn's seed is there by its draw deadline. Any node may relay any turn, and every
//! relay of a turn carries the same seed.
//!
//! Each poll:
//! - reads the receiver through the custody providers, at one block of the custody credit tag
//!   (`latest` in production): the turn whose draw deadline last passed, the two after it, and the
//!   receiver's pairing with the relay;
//! - raises `seed_missing` (Critical) while that turn has no seed, once an earlier turn has had one;
//! - reads the relayer's balance and raises `seed_relayer_unfunded` (Warning) below `min_balance`, and
//!   `seed_relayer_balance_unreadable` (Warning) once it has stayed unreadable for ten minutes;
//! - relays the earlier of the two later turns that has no seed: from the beacon node's finalized
//!   header, the EIP-4788 entry that holds its root, found through the Ethereum quorum, and its state's
//!   proof. A failure raises `seed_relay_failed` (Warning), which ends once both turns have seeds.
//!
//! One relay transaction is in flight at a time, so none queues behind another, and every send names
//! its nonce: one still waiting for a block after `resend_after` is replaced at its own nonce, and a
//! pending transaction the node does not track, as after a restart, holds new relays back. Nothing
//! else needs to survive a restart.
//!
//! What bounds each read from the host's network: the receiver's state is a custody quorum's; the
//! EIP-4788 entry, the relay's pairing and the Inbox's fee are an Ethereum quorum's, and the base fee
//! and nonces are the signing endpoint's; the header counts only once that entry holds its root, and the
//! state only once it hashes to the header's; and `SeedRelay` checks all of it again on chain, so a
//! wrong answer costs a relay that reverts, never a wrong seed.

mod abi;
mod balance;
mod beacon;
mod chains;
mod ethereum;
mod header;
mod schedule;
mod settle;
mod ssz;
pub mod state;

use std::collections::BTreeMap;
use std::future::Future;
use std::mem::take;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use tokio::task::spawn_blocking;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::field::Empty;
use tracing::{Instrument, Span, debug, error, info, info_span, warn};

pub use header::BeaconBlockHeader;

use crate::config::{NodeConfig, SeedRelaySection};
use crate::custody::ContractQuorum;
use crate::metrics::{FloatGauge, Metrics};
use crate::seed_relay::abi::ISeedRelay;
use crate::seed_relay::beacon::BeaconApi;
use crate::seed_relay::chains::Chains;
use crate::seed_relay::ethereum::{Ethereum, RelayIdentity};
use crate::seed_relay::schedule::{SECONDS_PER_SLOT, SLOTS_PER_EPOCH, seed_epoch};
use crate::seed_relay::state::{MixProof, prove_mixes};

/// The Arbitrum gas a relay's ticket buys: `receiveSeed` costs at most about 70,000 (the forge gas
/// report), so twice that, with room.
const L2_GAS_LIMIT: u64 = 150_000;
/// `receiveSeed(uint64,uint64,bytes32)`'s calldata.
const MESSAGE_LENGTH: usize = 4 + 3 * 32;
/// How many slots past it a proven block still anchors a relay: a day, well inside EIP-4788's 8191.
const PROVEN_SLOTS: u64 = 7200;

/// What the relayer reads and sends through: the node's own `Chains`, or a test's.
pub(crate) trait SeedChains {
    /// The receiver at the custody providers' final block.
    fn receiver(&self) -> impl Future<Output = anyhow::Result<ReceiverView>> + Send;
    /// The relay's deployment.
    fn identity(&self) -> impl Future<Output = anyhow::Result<RelayIdentity>> + Send;
    /// The beacon node's finalized header.
    fn finalized_header(&self) -> impl Future<Output = anyhow::Result<BeaconBlockHeader>> + Send;
    /// The EIP-4788 entry for the block at `timestamp`.
    fn beacon_root(&self, timestamp: u64) -> impl Future<Output = anyhow::Result<Option<B256>>> + Send;
    /// The serialized state after the block at `slot`.
    fn state(&self, slot: u64) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send;
    /// Whether the relay accepts states of fork `version`.
    fn accepts_fork(&self, version: [u8; 4]) -> impl Future<Output = anyhow::Result<bool>> + Send;
    /// What a ticket's submission is offered: the Inbox's fee at twice the latest base fee.
    fn submission_cost(&self, inbox: Address) -> impl Future<Output = anyhow::Result<U256>> + Send;
    /// The relayer's balance on Ethereum.
    fn balance(&self) -> impl Future<Output = anyhow::Result<U256>> + Send;
    /// Sends `input` to the relay with `value` from the relayer at `nonce`, outbidding the pending
    /// transaction `replacing` when the endpoint still finds it.
    fn send(
        &self,
        input: Vec<u8>,
        value: U256,
        nonce: u64,
        replacing: Option<B256>,
    ) -> impl Future<Output = anyhow::Result<B256>> + Send;
    /// Whether a sent transaction succeeded, once it is in a block.
    fn receipt(&self, hash: B256) -> impl Future<Output = anyhow::Result<Option<bool>>> + Send;
    /// The relayer's nonce at the latest block, and counting the transactions it has pending.
    fn nonces(&self) -> impl Future<Output = anyhow::Result<(u64, u64)>> + Send;
}

/// The receiver at one final Arbitrum block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReceiverView {
    /// The block's time, in seconds.
    pub time: u64,
    /// The relay the receiver takes seeds from, and the genesis time it counts epochs from.
    pub relay: Address,
    pub genesis: u64,
    /// The earliest turn with a seed, zero before the first.
    pub first: u64,
    /// The turn whose draw deadline last passed at `time`.
    pub final_turn: u64,
    /// The seeds of the final turn and the two after it, zero where there is none.
    pub seeds: [B256; 3],
}

/// What the relayer exports.
pub(crate) struct RelayMetrics {
    pub relays: Counter,
    pub failures: Counter,
    pub balance_eth: FloatGauge,
    pub latest_turn: Gauge,
}

/// The relay transaction this node has waiting for a block, its nonce, and when it went out: at most
/// one, so none queues behind another.
#[derive(Clone, Copy)]
struct InFlight {
    hash: B256,
    turn: u64,
    nonce: u64,
    at: Instant,
}

/// The last finalized block proven, the time of the execution block whose EIP-4788 entry holds its
/// root, and its state's proofs by epoch. It serves for `PROVEN_SLOTS`, so a relay that keeps failing
/// does not download a state each epoch.
struct Proven {
    header: BeaconBlockHeader,
    timestamp: u64,
    proofs: BTreeMap<u64, MixProof>,
}

/// The relayer's loop state; nothing in it needs to survive a restart.
pub(crate) struct SeedRelayer<C> {
    chains: C,
    relay: Address,
    receiver: Address,
    relayer: Address,
    resend_after: Duration,
    l2_max_fee_per_gas: U256,
    min_balance: U256,
    metrics: RelayMetrics,
    in_flight: Option<InFlight>,
    /// Turns whose relay from this node is in a block, and since when, until their seed is on Arbitrum.
    mined: BTreeMap<u64, Instant>,
    /// Since when a pending transaction this node is not tracking has held its relays back.
    held_since: Option<Instant>,
    proven: Option<Proven>,
    missing: bool,
    failing: bool,
    unfunded: bool,
    /// Since when the relayer's balance has been unreadable, and whether that has been alerted.
    unreadable_since: Option<Instant>,
    unreadable: bool,
}

impl RelayMetrics {
    /// The relayer's handles into the node's registry.
    pub(crate) fn of(metrics: &Metrics) -> Self {
        Self {
            relays: metrics.seed_relays.clone(),
            failures: metrics.seed_relay_failures.clone(),
            balance_eth: metrics.seed_relayer_balance_eth.clone(),
            latest_turn: metrics.seed_latest_turn.clone(),
        }
    }
}

impl<C: SeedChains> SeedRelayer<C> {
    /// A relayer for `cfg` over `chains`, with nothing sent yet.
    pub(crate) fn new(cfg: &SeedRelaySection, chains: C, metrics: RelayMetrics) -> Self {
        Self {
            chains,
            relay: cfg.relay,
            receiver: cfg.receiver,
            relayer: cfg.relayer,
            resend_after: cfg.resend_after,
            l2_max_fee_per_gas: cfg.l2_max_fee_per_gas,
            min_balance: cfg.min_balance,
            metrics,
            in_flight: None,
            mined: BTreeMap::new(),
            held_since: None,
            proven: None,
            missing: false,
            failing: false,
            unfunded: false,
            unreadable_since: None,
            unreadable: false,
        }
    }

    /// One pass: watch the final turn, check the balance, and relay what the receiver lacks.
    pub(crate) async fn poll(&mut self) {
        let view = match self.chains.receiver().await {
            Ok(view) => view,
            // Nothing can be watched or relayed without it, whether the quorum failed or its answer did.
            Err(error) => return self.fail(format!("reading the seed receiver failed: {error:#}")),
        };
        Span::current().record("final_turn", view.final_turn);
        if let Some(latest) = (0..3).rev().find(|&i| !view.seeds[i].is_zero()) {
            self.metrics.latest_turn.set(i64::try_from(view.final_turn + latest as u64).unwrap_or(i64::MAX));
        }
        self.watch(&view);
        self.check_balance().await;
        let open: Vec<u64> = (1..3).filter(|&i| view.seeds[i].is_zero()).map(|i| view.final_turn + i as u64).collect();
        self.mined.retain(|turn, _| open.contains(turn));
        if open.is_empty() {
            if take(&mut self.failing) {
                info!(
                    alert_resolved = "seed_relay_failed",
                    "turns {} and {} have their seeds",
                    view.final_turn + 1,
                    view.final_turn + 2
                );
            }
            return;
        }
        if let Err(error) = self.relay(&view, &open).await {
            self.fail(format!("relaying the seeds of turns {open:?} failed: {error:#}"));
        }
    }

    /// Raises `seed_missing` while the final turn has no seed, and resolves it once a final turn has one.
    fn watch(&mut self, view: &ReceiverView) {
        let turn = view.final_turn;
        // No turn draws before the receiver's first seed, so nothing is missing until one arrives.
        if view.first != 0 && turn > view.first && view.seeds[0].is_zero() {
            self.missing = true;
            error!(
                alert = "seed_missing",
                "turn {turn}'s seed was not on Arbitrum by its draw deadline, a day before its boundary: the turn \
                 draws from the last relayed seed hashed with its number"
            );
        } else if self.missing && !view.seeds[0].is_zero() {
            self.missing = false;
            info!(alert_resolved = "seed_missing", "turn {turn}'s seed was on Arbitrum by its draw deadline");
        }
    }

    /// Relays the seeds of `open` turns this node has not relayed within `resend_after`.
    /// Error cases:
    /// - the relay and receiver are not each other's, a relay this node sent reverted, or a read,
    ///   proof or send fails
    async fn relay(&mut self, view: &ReceiverView, open: &[u64]) -> anyhow::Result<()> {
        let identity = self.chains.identity().await?;
        ensure!(identity.receiver == self.receiver, "the relay sends to {}, not {}", identity.receiver, self.receiver);
        ensure!(view.relay == self.relay, "the receiver takes seeds from {}, not {}", view.relay, self.relay);
        ensure!(
            identity.genesis == view.genesis,
            "the relay's genesis is {}, the receiver's {}",
            identity.genesis,
            view.genesis
        );
        let Some(slot) = self.settle().await? else { return Ok(()) };
        let mut due = Vec::new();
        for &turn in open {
            if let Some(at) = self.mined.get(&turn).copied() {
                if at.elapsed() < self.resend_after {
                    continue;
                }
                self.mined.remove(&turn);
                self.fail(format!(
                    "turn {turn}'s relay was in a block {:?} ago and has not reached Arbitrum: relaying it again; a \
                     sequencer that withholds it can be overridden by force inclusion",
                    at.elapsed()
                ));
            }
            due.push(turn);
        }
        if due.is_empty() {
            return Ok(());
        }
        let header = self.chains.finalized_header().await?;
        let mut ready = Vec::new();
        for turn in due {
            let epoch = seed_epoch(turn, view.genesis)
                .with_context(|| format!("turn {turn} has no seed epoch from genesis {}", view.genesis))?;
            let end = epoch.checked_add(1).and_then(|next| next.checked_mul(SLOTS_PER_EPOCH));
            if header.slot >= end.with_context(|| format!("epoch {epoch} is past the clock"))? {
                ready.push((turn, epoch));
            } else {
                debug!(turn, epoch, slot = header.slot, "waiting for a finalized block past the seed's epoch");
            }
        }
        // One relay at a time: the next goes out once this one is in a block.
        let Some(&(turn, epoch)) = ready.first() else { return Ok(()) };
        self.prove(header, view.genesis, ready.iter().map(|&(_, epoch)| epoch).collect()).await?;
        let proven = self.proven.as_ref().expect("prove keeps every ready epoch's proof");
        let (header, timestamp, proof) = (proven.header, proven.timestamp, proven.proofs[&epoch].clone());
        ensure!(self.chains.accepts_fork(proof.fork_version).await?, "the relay refuses fork {:?}", proof.fork_version);
        let submission = self.chains.submission_cost(identity.inbox).await?;
        let fees = ISeedRelay::Fees {
            maxSubmissionCost: submission,
            gasLimit: U256::from(L2_GAS_LIMIT),
            maxFeePerGas: self.l2_max_fee_per_gas,
        };
        let value = submission + U256::from(L2_GAS_LIMIT) * self.l2_max_fee_per_gas;
        let input = ISeedRelay::relayCall { turn, witness: witness(timestamp, &header, &proof), fees }.abi_encode();
        let hash = self.chains.send(input, value, slot.nonce, slot.replacing).await?;
        info!(relayed_turn = turn, epoch, seed = %proof.mix, tx = %hash, "relayed the turn's seed to Arbitrum");
        self.metrics.relays.inc();
        self.in_flight = Some(InFlight { hash, turn, nonce: slot.nonce, at: Instant::now() });
        Ok(())
    }

    /// Keeps a proven block for `epochs`: the one already proven while it serves and holds them all,
    /// else `header`'s, anchored and proven from its state, which must hash to the header's state root.
    /// Error cases:
    /// - no execution block anchors `header`, or its state does not download, prove, or hash to its
    ///   state root
    async fn prove(&mut self, header: BeaconBlockHeader, genesis: u64, epochs: Vec<u64>) -> anyhow::Result<()> {
        let serves = self.proven.as_ref().is_some_and(|proven| {
            header.slot.saturating_sub(proven.header.slot) < PROVEN_SLOTS
                && epochs.iter().all(|epoch| proven.proofs.contains_key(epoch))
        });
        if serves {
            return Ok(());
        }
        let timestamp = self.anchor(&header, header.root(), genesis).await?;
        let state = self.chains.state(header.slot).await?;
        // Hashing a mainnet state takes seconds of CPU, kept off the runtime's workers.
        let wanted = epochs.clone();
        let proofs = spawn_blocking(move || prove_mixes(&state, &wanted)).await.context("the proof task")??;
        for proof in &proofs {
            ensure!(
                proof.state_root == header.state_root,
                "the state at slot {} hashes to {}, not the header's {}",
                header.slot,
                proof.state_root,
                header.state_root
            );
        }
        self.proven = Some(Proven { header, timestamp, proofs: epochs.into_iter().zip(proofs).collect() });
        Ok(())
    }

    /// The time of the execution block whose EIP-4788 entry is `root`: the first block after
    /// `header`, which may follow missed slots.
    /// Error cases:
    /// - the first block after it names another parent, or none does within an epoch
    async fn anchor(&self, header: &BeaconBlockHeader, root: B256, genesis: u64) -> anyhow::Result<u64> {
        for next in 1..=SLOTS_PER_EPOCH {
            // The slot is the beacon node's word until the EIP-4788 entry confirms its block.
            let timestamp = header
                .slot
                .checked_add(next)
                .and_then(|slot| slot.checked_mul(SECONDS_PER_SLOT))
                .and_then(|elapsed| elapsed.checked_add(genesis))
                .with_context(|| format!("slot {} is past the clock", header.slot))?;
            match self.chains.beacon_root(timestamp).await? {
                Some(found) if found == root => return Ok(timestamp),
                Some(found) => bail!("the block after slot {} names {found} as its parent, not {root}", header.slot),
                None => continue,
            }
        }
        bail!("no execution block in the epoch after slot {} names the finalized block {root}", header.slot)
    }

    /// Counts a failure and raises `seed_relay_failed`.
    fn fail(&mut self, message: String) {
        self.metrics.failures.inc();
        self.failing = true;
        warn!(alert = "seed_relay_failed", "{message}");
    }
}

/// The relayer's task for `cfg`, if it has a `[seed_relay]` section whose keys are all delivered.
/// Error cases:
/// - a client does not build
pub(crate) fn task(
    cfg: &NodeConfig,
    metrics: &Metrics,
) -> anyhow::Result<Option<impl Future<Output = anyhow::Result<()>> + use<>>> {
    let Some(relay) = &cfg.seed_relay else { return Ok(None) };
    if !relay.delivered() {
        // `secret_missing` names each undelivered key.
        warn!("the seed relayer is off until every key its URLs name is delivered");
        return Ok(None);
    }
    let chains = Chains {
        ethereum: Ethereum::new(relay)?,
        beacon: BeaconApi::new(relay)?,
        arbitrum: ContractQuorum::connect(&cfg.custody, "seed_relay")?,
        receiver: relay.receiver,
    };
    let relayer = SeedRelayer::new(relay, chains, RelayMetrics::of(metrics));
    Ok(Some(run(relayer, relay.poll_interval)))
}

/// Polls every `every`, for as long as the node runs.
async fn run<C: SeedChains>(mut relayer: SeedRelayer<C>, every: Duration) -> anyhow::Result<()> {
    let mut ticks = interval(every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        relayer.poll().instrument(info_span!("seed_relay", final_turn = Empty)).await;
    }
}

/// `SeedRelay.relay`'s witness for `proof`, anchored at the block at `timestamp`.
fn witness(timestamp: u64, header: &BeaconBlockHeader, proof: &MixProof) -> ISeedRelay::Witness {
    ISeedRelay::Witness {
        timestamp,
        header: ISeedRelay::BeaconBlockHeader {
            slot: header.slot,
            proposerIndex: header.proposer_index,
            parentRoot: header.parent_root,
            stateRoot: header.state_root,
            bodyRoot: header.body_root,
        },
        forkVersion: proof.fork_version.into(),
        forkProof: proof.fork_proof.clone(),
        seed: proof.mix,
        seedProof: proof.mix_proof.clone(),
    }
}

#[cfg(test)]
mod tests;
