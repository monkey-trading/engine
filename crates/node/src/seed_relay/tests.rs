use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use alloy::primitives::utils::parse_ether;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::SeedRelaySection;
use crate::seed_relay::schedule::{draw_deadline, last_final_turn};
use crate::seed_relay::state::{prove_mix, synthetic_state};
use crate::test_support::{Alert, capture_alerts_and_resolutions};

use super::*;

mod anchor;
mod balance;
mod nonce;
mod scenario;

const GENESIS: u64 = 1_606_824_023;
/// The turn whose draw deadline last passed; the two after it are open.
const FINAL: u64 = 20_763;
const FULU: [u8; 4] = [6, 0, 0, 0];

#[tokio::test]
async fn open_turns_are_relayed_one_at_a_time_from_one_proven_state() {
    let fake = Fake::new(slot_past(FINAL + 2));
    let mut relayer = relayer(&fake);
    relayer.poll().await;
    // While the first relay waits for a block, nothing else goes out.
    relayer.poll().await;
    assert_eq!(fake.sent.lock().unwrap().len(), 1);
    let first = fake.hashes.lock().unwrap()[0];
    fake.receipts.lock().unwrap().insert(first, Some(true));
    relayer.poll().await;
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    let anchor = GENESIS + 12 * (fake.header.slot + 1);
    for ((input, value, replacing, nonce), turn) in sent.iter().zip([FINAL + 1, FINAL + 2]) {
        let call = ISeedRelay::relayCall::abi_decode(input).unwrap();
        let epoch = seed_epoch(turn, GENESIS).unwrap();
        assert_eq!(call.turn, turn);
        assert_eq!(call.witness.timestamp, anchor);
        assert_eq!(call.witness.header.stateRoot, fake.header.state_root);
        assert_eq!(call.witness.seed, B256::from(mix(epoch)));
        assert_eq!((call.witness.forkProof.len(), call.witness.seedProof.len()), (8, 22));
        assert_eq!(call.fees.gasLimit, U256::from(L2_GAS_LIMIT));
        assert_eq!(*value, SUBMISSION + U256::from(L2_GAS_LIMIT) * fee());
        assert_eq!((*replacing, *nonce), (None, 0));
    }
    assert_eq!(*fake.states.lock().unwrap(), 1, "both turns are proven from one download");
}

#[tokio::test]
async fn a_turn_whose_epoch_is_not_yet_final_waits_for_a_later_block() {
    let fake = Fake::new(slot_past(FINAL + 1));
    relayer(&fake).poll().await;
    let sent = fake.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(ISeedRelay::relayCall::abi_decode(&sent[0].0).unwrap().turn, FINAL + 1);
}

#[tokio::test(start_paused = true)]
async fn a_relay_waiting_for_a_block_is_replaced_at_its_nonce_and_one_not_on_arbitrum_goes_out_again() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    let mut relayer = relayer(&fake);
    let hour = Duration::from_secs(3601);
    relayer.poll().await;
    tokio::time::advance(hour).await;
    relayer.poll().await;
    let hashes = fake.hashes.lock().unwrap().clone();
    assert_eq!(fake.sent.lock().unwrap()[1].2, Some(hashes[0]), "the waiting relay is replaced at its nonce");
    let nonces: Vec<u64> = fake.sent.lock().unwrap().iter().map(|relay| relay.3).collect();
    assert_eq!(nonces[1], nonces[0]);
    assert!(named(&mut alerts, "seed_relay_failed").iter().any(|a| a["message"].contains("waited")));
    // Another member's relay of the first turn lands; the stuck one is replaced by the next turn's relay.
    fake.seeds(|seeds| seeds[1] = B256::repeat_byte(8));
    tokio::time::advance(hour).await;
    relayer.poll().await;
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(sent[2].2, Some(hashes[1]));
    assert_eq!(ISeedRelay::relayCall::abi_decode(&sent[2].0).unwrap().turn, FINAL + 2);
    // In a block but not on Arbitrum after `resend_after`, the turn goes out again at a new nonce.
    let mined = *fake.hashes.lock().unwrap().last().unwrap();
    fake.receipts.lock().unwrap().insert(mined, Some(true));
    relayer.poll().await;
    assert_eq!(fake.sent.lock().unwrap().len(), 3);
    tokio::time::advance(hour).await;
    relayer.poll().await;
    assert_eq!(fake.sent.lock().unwrap()[3].2, None);
    assert!(named(&mut alerts, "seed_relay_failed").iter().any(|a| a["message"].contains("has not reached Arbitrum")));
    // Once both turns have their seeds the episode ends.
    fake.seeds(|seeds| seeds[2] = B256::repeat_byte(9));
    relayer.poll().await;
    assert_eq!(named(&mut resolved, "seed_relay_failed").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_pending_transaction_the_node_does_not_track_holds_relays_back() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    *fake.nonces.lock().unwrap() = (4, 5);
    let mut relayer = relayer(&fake);
    relayer.poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed").is_empty(), "a pending transaction may just be slow");
    tokio::time::advance(Duration::from_secs(3601)).await;
    relayer.poll().await;
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("not tracking"));
    *fake.nonces.lock().unwrap() = (5, 5);
    relayer.poll().await;
    assert_eq!(fake.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_reverted_relay_alerts_and_is_retried_at_the_next_poll() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 1));
    let mut relayer = relayer(&fake);
    relayer.poll().await;
    let hash = *fake.hashes.lock().unwrap().last().unwrap();
    fake.receipts.lock().unwrap().insert(hash, Some(false));
    relayer.poll().await;
    assert!(named(&mut alerts, "seed_relay_failed").iter().any(|a| a["message"].contains("reverted")));
    assert_eq!(fake.sent.lock().unwrap().len(), 1, "the poll that found the revert sends nothing");
    relayer.poll().await;
    assert_eq!(fake.sent.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn seed_missing_waits_for_the_first_seed_then_fires_until_a_final_turn_has_one() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    let mut relayer = relayer(&fake);
    // Before any seed, no turn can draw, so none is missing.
    fake.seeds(|seeds| seeds[0] = B256::ZERO);
    fake.first(0);
    relayer.poll().await;
    assert!(named(&mut alerts, "seed_missing").is_empty());
    fake.first(FINAL - 3);
    relayer.poll().await;
    let missing = named(&mut alerts, "seed_missing");
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0]["level"], "ERROR");
    assert!(missing[0]["message"].contains(&format!("turn {FINAL}'s seed")));
    fake.seeds(|seeds| seeds[0] = B256::repeat_byte(7));
    relayer.poll().await;
    assert_eq!(named(&mut resolved, "seed_missing").len(), 1);
}

#[tokio::test]
async fn an_underfunded_relayer_warns_until_it_is_refilled() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    let mut relayer = relayer(&fake);
    *fake.balance.lock().unwrap() = parse_ether("0.01").unwrap();
    relayer.poll().await;
    let unfunded = named(&mut alerts, "seed_relayer_unfunded");
    assert_eq!(unfunded.len(), 1);
    assert_eq!(unfunded[0]["relayer"], format!("{}", Address::repeat_byte(0x22)));
    *fake.balance.lock().unwrap() = parse_ether("1").unwrap();
    relayer.poll().await;
    assert_eq!(named(&mut resolved, "seed_relayer_unfunded").len(), 1);
}

#[tokio::test]
async fn nothing_is_sent_through_a_relay_and_receiver_that_are_not_each_others() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let mut fake = Fake::at(slot_past(FINAL + 2));
    fake.identity.receiver = Address::repeat_byte(0x66);
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("the relay sends to"));
    let mut fake = Fake::at(slot_past(FINAL + 2));
    fake.identity.genesis = GENESIS + 384;
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("genesis is"));
    let fake = Fake::new(slot_past(FINAL + 2));
    fake.view.lock().unwrap().relay = Address::repeat_byte(0x67);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("the receiver takes seeds from"));
}

#[tokio::test]
async fn a_receiver_read_that_fails_alerts_and_relays_nothing() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let fake = Fake::new(slot_past(FINAL + 2));
    *fake.receiver_error.lock().unwrap() = Some("decoding a seed".to_owned());
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("reading the seed receiver failed"));
}

#[tokio::test]
async fn a_failed_send_reuses_its_proven_state_while_the_block_anchors_a_relay() {
    let fake = Fake::new(slot_past(FINAL + 2));
    let mut relayer = relayer(&fake);
    *fake.send_error.lock().unwrap() = Some("insufficient funds".to_owned());
    relayer.poll().await;
    relayer.poll().await;
    // A block finalized since, within a day of slots, does not bring a new download.
    let later = Fake::at(slot_past(FINAL + 2) + PROVEN_SLOTS - 1);
    *fake.later.lock().unwrap() = Some((later.header, later.state, later.roots));
    relayer.poll().await;
    assert_eq!(*fake.states.lock().unwrap(), 1, "one download while the proven block anchors a relay");
    let past = Fake::at(slot_past(FINAL + 2) + PROVEN_SLOTS);
    *fake.later.lock().unwrap() = Some((past.header, past.state, past.roots));
    *fake.send_error.lock().unwrap() = None;
    relayer.poll().await;
    assert_eq!(*fake.states.lock().unwrap(), 2);
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(ISeedRelay::relayCall::abi_decode(&sent[0].0).unwrap().witness.header.slot, past.header.slot);
}

/// Numbers the host's network supplies past what the clock holds fail the relay, never panic.
#[tokio::test]
async fn a_slot_or_turn_past_the_clock_fails_without_a_panic() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let mut fake = Fake::at(slot_past(FINAL + 2));
    fake.header.slot = u64::MAX;
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("past the clock"));
    let fake = Fake::new(slot_past(FINAL + 2));
    fake.view.lock().unwrap().final_turn = last_final_turn(u64::MAX);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed")[0]["message"].contains("no seed epoch"));
}

#[tokio::test]
async fn the_anchor_is_the_first_block_after_missed_slots_and_never_a_foreign_child() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let mut fake = Fake::at(slot_past(FINAL + 2));
    let root = fake.header.root();
    fake.roots = BTreeMap::from([(GENESIS + 12 * (fake.header.slot + 3), root)]);
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    let sent = fake.sent.lock().unwrap().clone();
    assert_eq!(
        ISeedRelay::relayCall::abi_decode(&sent[0].0).unwrap().witness.timestamp,
        GENESIS + 12 * (fake.header.slot + 3)
    );
    // A first block after it that names another parent means the beacon node's block is not the
    // providers' chain's.
    let mut fake = Fake::at(slot_past(FINAL + 2));
    fake.roots = BTreeMap::from([(GENESIS + 12 * (fake.header.slot + 1), B256::repeat_byte(1))]);
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed").iter().any(|a| a["message"].contains("as its parent")));
}

#[tokio::test]
async fn a_state_that_is_not_the_headers_or_a_fork_the_relay_refuses_is_not_sent() {
    let (_guard, mut alerts, _) = capture_alerts_and_resolutions();
    let mut fake = Fake::at(slot_past(FINAL + 2));
    fake.header.state_root = B256::repeat_byte(3);
    fake.roots = BTreeMap::from([(GENESIS + 12 * (fake.header.slot + 1), fake.header.root())]);
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
    assert!(named(&mut alerts, "seed_relay_failed").iter().any(|a| a["message"].contains("hashes to")));
    let mut fake = Fake::at(slot_past(FINAL + 2));
    fake.accepts = false;
    let fake = Arc::new(fake);
    relayer(&fake).poll().await;
    assert!(fake.sent.lock().unwrap().is_empty());
}

// ── Helpers ──

/// What the fake Inbox asks for a ticket's submission.
const SUBMISSION: U256 = U256::from_limbs([1_000_000, 0, 0, 0]);

/// A relay the fake sent: its input, value, the transaction it replaced, and its nonce.
type SentRelay = (Vec<u8>, U256, Option<B256>, u64);

/// A finalized block, its state and its EIP-4788 entries.
type Finalized = (BeaconBlockHeader, Vec<u8>, BTreeMap<u64, B256>);

/// Chains that answer from fixed values and record what is sent.
struct Fake {
    view: Mutex<ReceiverView>,
    identity: RelayIdentity,
    header: BeaconBlockHeader,
    state: Vec<u8>,
    roots: BTreeMap<u64, B256>,
    accepts: bool,
    balance: Mutex<U256>,
    states: Mutex<usize>,
    sent: Mutex<Vec<SentRelay>>,
    hashes: Mutex<Vec<B256>>,
    receipts: Mutex<BTreeMap<B256, Option<bool>>>,
    /// A later finalized block, its state and its EIP-4788 entries, once the beacon node moves on.
    later: Mutex<Option<Finalized>>,
    receiver_error: Mutex<Option<String>>,
    send_error: Mutex<Option<String>>,
    /// Record each send, then fail it, as when the endpoint takes a transaction but its answer is lost.
    lose_answer: Mutex<bool>,
    /// Fails the balance read with this message.
    balance_error: Mutex<Option<String>>,
    /// The relayer's nonce at the latest block, and counting its pending transactions.
    nonces: Mutex<(u64, u64)>,
}

impl Fake {
    /// The receiver one hour past the final turn's deadline, holding only its seed, and a beacon node
    /// whose finalized block is at `slot`.
    fn new(slot: u64) -> Arc<Self> {
        Arc::new(Self::at(slot))
    }

    fn at(slot: u64) -> Self {
        let state = synthetic_state(slot, FULU, mix);
        let state_root = prove_mix(&state, seed_epoch(FINAL + 1, GENESIS).unwrap()).unwrap().state_root;
        let header = BeaconBlockHeader {
            slot,
            proposer_index: 7,
            parent_root: B256::repeat_byte(1),
            state_root,
            body_root: B256::repeat_byte(2),
        };
        let cfg = cfg();
        Self {
            view: Mutex::new(ReceiverView {
                time: draw_deadline(FINAL).unwrap() + 3600,
                relay: cfg.relay,
                genesis: GENESIS,
                first: FINAL - 3,
                final_turn: FINAL,
                seeds: [B256::repeat_byte(7), B256::ZERO, B256::ZERO],
            }),
            identity: RelayIdentity { receiver: cfg.receiver, inbox: Address::repeat_byte(0x77), genesis: GENESIS },
            roots: BTreeMap::from([(GENESIS + 12 * (slot + 1), header.root())]),
            header,
            state,
            accepts: true,
            balance: Mutex::new(parse_ether("1").unwrap()),
            states: Mutex::new(0),
            sent: Mutex::new(Vec::new()),
            hashes: Mutex::new(Vec::new()),
            receipts: Mutex::new(BTreeMap::new()),
            later: Mutex::new(None),
            receiver_error: Mutex::new(None),
            send_error: Mutex::new(None),
            lose_answer: Mutex::new(false),
            balance_error: Mutex::new(None),
            nonces: Mutex::new((0, 0)),
        }
    }

    fn seeds(&self, change: impl FnOnce(&mut [B256; 3])) {
        change(&mut self.view.lock().unwrap().seeds);
    }

    fn first(&self, turn: u64) {
        self.view.lock().unwrap().first = turn;
    }
}

impl SeedChains for Arc<Fake> {
    async fn receiver(&self) -> anyhow::Result<ReceiverView> {
        if let Some(error) = self.receiver_error.lock().unwrap().clone() {
            bail!("{error}");
        }
        Ok(self.view.lock().unwrap().clone())
    }

    async fn identity(&self) -> anyhow::Result<RelayIdentity> {
        Ok(self.identity.clone())
    }

    async fn finalized_header(&self) -> anyhow::Result<BeaconBlockHeader> {
        Ok(self.later.lock().unwrap().as_ref().map_or(self.header, |later| later.0))
    }

    async fn beacon_root(&self, timestamp: u64) -> anyhow::Result<Option<B256>> {
        let later = self.later.lock().unwrap();
        Ok(later.as_ref().map_or(&self.roots, |later| &later.2).get(&timestamp).copied())
    }

    async fn state(&self, slot: u64) -> anyhow::Result<Vec<u8>> {
        *self.states.lock().unwrap() += 1;
        if let Some((header, state, _)) = self.later.lock().unwrap().as_ref() {
            assert_eq!(slot, header.slot);
            return Ok(state.clone());
        }
        assert_eq!(slot, self.header.slot);
        Ok(self.state.clone())
    }

    async fn accepts_fork(&self, version: [u8; 4]) -> anyhow::Result<bool> {
        Ok(self.accepts && version == FULU)
    }

    async fn submission_cost(&self, inbox: Address) -> anyhow::Result<U256> {
        assert_eq!(inbox, self.identity.inbox);
        Ok(SUBMISSION)
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        if let Some(error) = self.balance_error.lock().unwrap().clone() {
            bail!("{error}");
        }
        Ok(*self.balance.lock().unwrap())
    }

    async fn send(&self, input: Vec<u8>, value: U256, nonce: u64, replacing: Option<B256>) -> anyhow::Result<B256> {
        if let Some(error) = self.send_error.lock().unwrap().clone() {
            bail!("{error}");
        }
        let mut sent = self.sent.lock().unwrap();
        sent.push((input, value, replacing, nonce));
        let hash = B256::from(U256::from(sent.len()));
        self.hashes.lock().unwrap().push(hash);
        if *self.lose_answer.lock().unwrap() {
            bail!("the answer to eth_sendTransaction timed out");
        }
        Ok(hash)
    }

    async fn receipt(&self, hash: B256) -> anyhow::Result<Option<bool>> {
        Ok(self.receipts.lock().unwrap().get(&hash).copied().flatten())
    }

    async fn nonces(&self) -> anyhow::Result<(u64, u64)> {
        Ok(*self.nonces.lock().unwrap())
    }
}

/// A relayer over `fake` with the unit tests' section.
fn relayer(fake: &Arc<Fake>) -> SeedRelayer<Arc<Fake>> {
    let metrics = RelayMetrics {
        relays: Counter::default(),
        failures: Counter::default(),
        balance_eth: FloatGauge::default(),
        latest_turn: Gauge::default(),
    };
    SeedRelayer::new(&cfg(), fake.clone(), metrics)
}

fn cfg() -> SeedRelaySection {
    toml::from_str(
        "chain_id = 1\n\
         rpc_url = 'http://127.0.0.1:4'\n\
         relayer = '0x2222222222222222222222222222222222222222'\n\
         beacon_api = 'http://127.0.0.1:5052'\n\
         relay = '0x4444444444444444444444444444444444444444'\n\
         receiver = '0x5555555555555555555555555555555555555555'\n\
         poll_interval = '60s'\n\
         rpc_timeout = '10s'\n\
         state_timeout = '300s'\n\
         resend_after = '1h'\n\
         l2_max_fee_per_gas = '0.1'\n\
         min_balance = '0.05'\n\
         providers = []",
    )
    .unwrap()
}

fn fee() -> U256 {
    U256::from(100_000_000u64)
}

/// The first slot of the epoch after `turn`'s seed epoch, plus a few.
fn slot_past(turn: u64) -> u64 {
    (seed_epoch(turn, GENESIS).unwrap() + 1) * SLOTS_PER_EPOCH + 5
}

/// The synthetic state's mix for `epoch`.
fn mix(epoch: u64) -> [u8; 32] {
    let mut chunk = [0u8; 32];
    chunk[..8].copy_from_slice(&epoch.to_le_bytes());
    crate::seed_relay::ssz::hash(&[0x5e; 32], &chunk)
}

/// The alerts or resolutions named `name` that have arrived.
fn named(lines: &mut UnboundedReceiver<Alert>, name: &str) -> Vec<Alert> {
    let mut found = Vec::new();
    while let Ok(line) = lines.try_recv() {
        if line.get("alert").or(line.get("alert_resolved")).is_some_and(|value| value == name) {
            found.push(line);
        }
    }
    found
}
