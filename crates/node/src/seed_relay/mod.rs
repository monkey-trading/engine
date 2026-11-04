//! The seed relayer: it proves on Ethereum the RANDAO mix that seeds each daily operator turn, relays
//! it to the seed receiver on Arbitrum (`contracts/src/SeedRelay.sol`, `SeedReceiver.sol`), and
//! watches that each turn's seed is there by its draw deadline. Any node may relay any turn, and every
//! relay of a turn carries the same seed, so the relayer keeps nothing across a restart: it reads the
//! receiver again and relays what it lacks, at the cost of at most one more relay.
//!
//! Each poll:
//! - reads the receiver through the custody providers at one final block: the turn whose draw
//!   deadline last passed, the two after it, and the receiver's pairing with the relay;
//! - raises `seed_missing` (Critical) while that turn has no seed, once an earlier turn has had one;
//! - reads the relayer's balance and raises `seed_relayer_unfunded` (Warning) below `min_balance`;
//! - for each of the two later turns without a seed, unless this node relayed it within
//!   `resend_after`: reads the beacon node's finalized header, finds through the Ethereum providers
//!   the EIP-4788 entry that holds its root, downloads its state, proves the turn's mix, and sends the
//!   relay. A failure raises `seed_relay_failed` (Warning), which ends once both turns have seeds.
//!
//! What bounds each read from the host's network: the receiver's state is a custody quorum's; the
//! EIP-4788 entry, the relay's pairing and the fees are an Ethereum quorum's; the header counts only
//! once that entry holds its root, and the state only once it hashes to the header's; and `SeedRelay`
//! checks all of it again on chain, so a wrong answer costs a relay that reverts, never a wrong seed.

mod abi;
mod beacon;
mod ethereum;
mod header;
mod schedule;
mod ssz;
pub mod state;

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::field::Empty;
use tracing::{Instrument, Span, debug, error, info, info_span, warn};

pub use header::BeaconBlockHeader;

use crate::config::{NodeConfig, SeedRelaySection};
use crate::custody::ContractQuorum;
use crate::metrics::{FloatGauge, Metrics};
use crate::seed_relay::abi::{ISeedReceiver, ISeedRelay};
use crate::seed_relay::beacon::BeaconApi;
use crate::seed_relay::ethereum::{Ethereum, RelayIdentity};
use crate::seed_relay::schedule::{SECONDS_PER_SLOT, SLOTS_PER_EPOCH, draw_deadline, last_final_turn, seed_epoch};
use crate::seed_relay::state::{MixProof, prove_mix};

/// The Arbitrum gas a relay's ticket buys: `receiveSeed` costs at most about 70,000 (the forge gas
/// report), so twice that, with room.
const L2_GAS_LIMIT: u64 = 150_000;
/// `receiveSeed(uint64,uint64,bytes32)`'s calldata.
const MESSAGE_LENGTH: usize = 4 + 3 * 32;

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
    /// Sends `input` to the relay with `value` from the relayer.
    fn send(&self, input: Vec<u8>, value: U256) -> impl Future<Output = anyhow::Result<B256>> + Send;
    /// Whether a sent transaction succeeded, once it is in a block.
    fn receipt(&self, hash: B256) -> impl Future<Output = anyhow::Result<Option<bool>>> + Send;
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

/// The node's chains: Ethereum and the beacon node through the relayer's endpoints, and Arbitrum
/// through the custody providers.
pub(crate) struct Chains {
    ethereum: Ethereum,
    beacon: BeaconApi,
    arbitrum: ContractQuorum,
    receiver: Address,
}

/// What the relayer exports.
pub(crate) struct RelayMetrics {
    pub relays: Counter,
    pub failures: Counter,
    pub balance_eth: FloatGauge,
    pub latest_turn: Gauge,
}

/// A relay this node sent, and when.
#[derive(Clone, Copy)]
struct Sent {
    hash: B256,
    at: Instant,
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
    sent: BTreeMap<u64, Sent>,
    missing: bool,
    failing: bool,
    unfunded: bool,
}

impl SeedChains for Chains {
    async fn receiver(&self) -> anyhow::Result<ReceiverView> {
        let seed = |turn: u64| ISeedReceiver::relayedSeedCall { turn }.abi_encode().into();
        let (time, answers) = self
            .arbitrum
            .read(self.receiver, |time| {
                let turn = last_final_turn(time);
                vec![
                    ("seed_receiver_relay", ISeedReceiver::relayCall {}.abi_encode().into()),
                    ("seed_receiver_genesis", ISeedReceiver::beaconGenesisTimeCall {}.abi_encode().into()),
                    ("seed_receiver_first", ISeedReceiver::firstRelayedTurnCall {}.abi_encode().into()),
                    ("seed_final_turn", seed(turn)),
                    ("seed_next_turn", seed(turn + 1)),
                    ("seed_turn_after", seed(turn + 2)),
                ]
            })
            .await?;
        let [relay, genesis, first, seeds @ ..] = answers.as_slice() else { bail!("the receiver read lost a call") };
        let seeds = seeds
            .iter()
            .map(|answer| ISeedReceiver::relayedSeedCall::abi_decode_returns(answer).context("decoding a seed"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(ReceiverView {
            time,
            relay: ISeedReceiver::relayCall::abi_decode_returns(relay).context("decoding the receiver's relay")?,
            genesis: ISeedReceiver::beaconGenesisTimeCall::abi_decode_returns(genesis).context("decoding genesis")?,
            first: ISeedReceiver::firstRelayedTurnCall::abi_decode_returns(first).context("decoding the first turn")?,
            final_turn: last_final_turn(time),
            seeds: seeds.try_into().map_err(|_| anyhow::anyhow!("the receiver read lost a seed"))?,
        })
    }

    async fn identity(&self) -> anyhow::Result<RelayIdentity> {
        self.ethereum.identity().await
    }

    async fn finalized_header(&self) -> anyhow::Result<BeaconBlockHeader> {
        self.beacon.finalized_header().await
    }

    async fn beacon_root(&self, timestamp: u64) -> anyhow::Result<Option<B256>> {
        self.ethereum.beacon_root(timestamp).await
    }

    async fn state(&self, slot: u64) -> anyhow::Result<Vec<u8>> {
        self.beacon.state(slot).await
    }

    async fn accepts_fork(&self, version: [u8; 4]) -> anyhow::Result<bool> {
        self.ethereum.accepts_fork(version).await
    }

    async fn submission_cost(&self, inbox: Address) -> anyhow::Result<U256> {
        // The Inbox charges at the base fee of the block that includes the relay, which may rise
        // while it waits; what the ticket does not spend is refunded on Arbitrum.
        let base_fee = self.ethereum.base_fee().await?.saturating_mul(U256::from(2));
        self.ethereum.submission_fee(inbox, MESSAGE_LENGTH, base_fee).await
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        self.ethereum.balance().await
    }

    async fn send(&self, input: Vec<u8>, value: U256) -> anyhow::Result<B256> {
        self.ethereum.send(input, value).await
    }

    async fn receipt(&self, hash: B256) -> anyhow::Result<Option<bool>> {
        self.ethereum.receipt(hash).await
    }
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
            sent: BTreeMap::new(),
            missing: false,
            failing: false,
            unfunded: false,
        }
    }

    /// One pass: watch the final turn, check the balance, and relay what the receiver lacks.
    pub(crate) async fn poll(&mut self) {
        let view = match self.chains.receiver().await {
            Ok(view) => view,
            // The custody quorum raises its own provider and quorum alerts under `seed_relay`.
            Err(error) => return warn!("reading the seed receiver failed: {error:#}"),
        };
        Span::current().record("turn", view.final_turn);
        if let Some(latest) = (0..3).rev().find(|&i| !view.seeds[i].is_zero()) {
            self.metrics.latest_turn.set(i64::try_from(view.final_turn + latest as u64).unwrap_or(i64::MAX));
        }
        self.watch(&view);
        self.check_balance().await;
        let open: Vec<u64> = (1..3).filter(|&i| view.seeds[i].is_zero()).map(|i| view.final_turn + i as u64).collect();
        self.sent.retain(|turn, _| open.contains(turn));
        if open.is_empty() {
            if std::mem::take(&mut self.failing) {
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
            self.fail(format!("relaying turn {open:?}'s seed failed: {error:#}"));
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
                "turn {turn}'s seed was not on Arbitrum by its draw deadline at {}: the turn draws from the last \
                 relayed seed hashed with its number",
                draw_deadline(turn)
            );
        } else if self.missing && !view.seeds[0].is_zero() {
            self.missing = false;
            info!(alert_resolved = "seed_missing", "turn {turn}'s seed was on Arbitrum by its draw deadline");
        }
    }

    /// Exports the relayer's balance and warns while it is below `min_balance`. A read that fails
    /// changes nothing: the relay that needs the same providers fails and alerts on its own.
    async fn check_balance(&mut self) {
        let balance = match self.chains.balance().await {
            Ok(balance) => balance,
            Err(error) => return debug!("the seed relayer's balance is unreadable: {error:#}"),
        };
        let ether = format_ether(balance);
        self.metrics.balance_eth.set(ether.parse().unwrap_or(f64::NAN));
        if balance < self.min_balance {
            self.unfunded = true;
            warn!(
                alert = "seed_relayer_unfunded",
                relayer = %self.relayer,
                "the seed relayer holds {ether} ETH, below {}: relays stop when it cannot pay for one",
                format_ether(self.min_balance)
            );
        } else if std::mem::take(&mut self.unfunded) {
            info!(alert_resolved = "seed_relayer_unfunded", relayer = %self.relayer, "the seed relayer holds {ether} ETH");
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
        let mut due = Vec::new();
        for &turn in open {
            if let Some(Sent { hash, at }) = self.sent.get(&turn).copied() {
                if self.chains.receipt(hash).await? == Some(false) {
                    self.sent.remove(&turn);
                    bail!("turn {turn}'s relay {hash} reverted");
                }
                if at.elapsed() < self.resend_after {
                    continue;
                }
                self.fail(format!(
                    "turn {turn}'s relay {hash} has not reached Arbitrum in {:?}: relaying it again; a sequencer that \
                     withholds it can be overridden by force inclusion",
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
            let epoch = seed_epoch(turn, view.genesis).with_context(|| format!("turn {turn} is before genesis"))?;
            if header.slot >= (epoch + 1) * SLOTS_PER_EPOCH {
                ready.push((turn, epoch));
            } else {
                debug!(turn, epoch, slot = header.slot, "waiting for a finalized block past the seed's epoch");
            }
        }
        if ready.is_empty() {
            return Ok(());
        }
        let root = header.root();
        let timestamp = self.anchor(&header, root, view.genesis).await?;
        let state = self.chains.state(header.slot).await?;
        let submission = self.chains.submission_cost(identity.inbox).await?;
        let fees = ISeedRelay::Fees {
            maxSubmissionCost: submission,
            gasLimit: U256::from(L2_GAS_LIMIT),
            maxFeePerGas: self.l2_max_fee_per_gas,
        };
        let value = submission + U256::from(L2_GAS_LIMIT) * self.l2_max_fee_per_gas;
        for (turn, epoch) in ready {
            let proof = prove_mix(&state, epoch)?;
            ensure!(
                proof.state_root == header.state_root,
                "the state at slot {} hashes to {}, not the header's {}",
                header.slot,
                proof.state_root,
                header.state_root
            );
            ensure!(
                self.chains.accepts_fork(proof.fork_version).await?,
                "the relay refuses fork {:?}",
                proof.fork_version
            );
            let witness = witness(timestamp, &header, &proof);
            let input = ISeedRelay::relayCall { turn, witness, fees: fees.clone() }.abi_encode();
            let hash = self.chains.send(input, value).await?;
            info!(turn, epoch, seed = %proof.mix, tx = %hash, "relayed the turn's seed to Arbitrum");
            self.metrics.relays.inc();
            self.sent.insert(turn, Sent { hash, at: Instant::now() });
        }
        Ok(())
    }

    /// The time of the execution block whose EIP-4788 entry is `root`: the first block after
    /// `header`, which may follow missed slots.
    /// Error cases:
    /// - the first block after it names another parent, or none does within an epoch
    async fn anchor(&self, header: &BeaconBlockHeader, root: B256, genesis: u64) -> anyhow::Result<u64> {
        for next in 1..=SLOTS_PER_EPOCH {
            let timestamp = genesis + SECONDS_PER_SLOT * (header.slot + next);
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
        relayer.poll().instrument(info_span!("seed_relay", turn = Empty)).await;
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
