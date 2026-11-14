//! The poster's v2 heartbeat while a turn names the operator: heads only where the liveness rule lets
//! it sign, binding the assignment whose keys the enclave holds, held heads through a hold, nothing
//! while the fence stops the enclave.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use engine_core::types::{AccountId, Millis};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::fake_chain::FakeChain;
use crate::clock::test_clock::TestClock;
use crate::liveness::operator::Signing;
use crate::liveness::{Gate, Released, Signable};
use crate::log::CheckpointDomain;
use crate::log_heads::{Head, Heartbeat, Hold, Poster, SignedHead, TurnHeads};
use crate::trusted_time::{Sample, TrustedClock};

const RELEASE: B256 = B256::repeat_byte(0x5e);
const GENERATION: u64 = 7;
const TRUE_MS: Millis = 1_800_000_000_000;

/// The poster's inputs a test drives.
struct Drive {
    released: watch::Sender<Released>,
    signing: watch::Sender<Signing>,
    hold: watch::Sender<Hold>,
    gate: Gate,
    signed: watch::Receiver<Option<SignedHead>>,
    clock: Arc<TestClock>,
    tip: watch::Sender<Head>,
    task: JoinHandle<anyhow::Result<()>>,
}

#[tokio::test(start_paused = true)]
async fn heads_go_out_only_at_the_seq_a_member_confirmed() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.release(8);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty(), "no member confirmed: the heartbeat stops");
    assert!(chain.posts().is_empty(), "nor does a v1 head go out under a turn");
    drive.confirm(5, 0xc1);
    drive.clock.advance(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(30)).await;
    let posted = chain.posts_v2();
    assert_eq!(posted.len(), 1, "{posted:?}");
    let (epoch, head) = posted[0];
    assert_eq!((epoch, head.head), (1, at(5)), "the confirmed seq, not the tip");
    assert_eq!(head.confirmer, Address::repeat_byte(0xc1));
    assert_eq!(head.stamp, stamp(5));
    assert_eq!(head.hold, Hold::Live);
    assert_eq!((head.turn, head.position, head.release, head.generation), (20_400, 2, RELEASE, GENERATION));
    assert_eq!(drive.signed.borrow().as_ref().map(|signed| signed.head), Some(head), "the stream carries it");
    drive.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_head_binds_no_assignment_whose_keys_the_enclave_does_not_hold() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION + 1));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.release(3);
    drive.confirm(3, 0xc1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let (_, head) = *chain.posts_v2().first().expect("a head went out");
    assert_eq!((head.turn, head.position, head.generation), (0, 0, 0), "another generation's assignment");
    assert_eq!(head.release, RELEASE, "the enclave's release still");
    drive.stop().await;
}

#[tokio::test(start_paused = true)]
async fn heads_go_out_at_least_twice_per_interval_each_newer_in_seq_and_signing_time() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    for seq in 1..=8 {
        drive.release(seq);
        drive.confirm(seq, 0xc1);
        drive.clock.advance(Duration::from_secs(15));
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
    let posted = chain.posts_v2();
    // Two minutes of a 60 s interval: at least four heads, each newer than the last.
    assert!(posted.len() >= 4, "{posted:?}");
    for pair in posted.windows(2) {
        assert!(pair[1].1.head.seq > pair[0].1.head.seq);
        assert!(pair[1].1.signed_at > pair[0].1.signed_at);
    }
    drive.stop().await;
}

#[tokio::test(start_paused = true)]
async fn through_a_hold_held_heads_repeat_the_confirmed_tip_on_newer_blocks() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.release(4);
    drive.confirm(4, 0xc1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(chain.posts_v2().len(), 1);
    // A trusted-time halt: the tip stands, and no fresh sample moves the signing time.
    drive.hold.send_replace(Hold::TimeHalt);
    for block in [110, 120, 130] {
        chain.change_turn(|turn| turn.block_number = block);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    let held: Vec<_> = chain.posts_v2().into_iter().skip(1).map(|(_, head)| head).collect();
    assert_eq!(held.len(), 3, "{held:?}");
    for (head, block) in held.iter().zip([110, 120, 130]) {
        assert_eq!((head.head, head.hold, head.block_number), (at(4), Hold::TimeHalt, block));
        assert_eq!(head.parent, Some(at(4)), "each repeats the head it extends");
    }
    // No newer block: no held head, which could only repeat one the chain holds.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(chain.posts_v2().len(), 4);
    drive.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_held_head_waits_for_the_confirmed_tip_and_a_block_no_older_than_the_mark() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.hold.send_replace(Hold::BenchmarkSync);
    drive.release(6);
    drive.confirm(5, 0xc1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty(), "the tip is not confirmed");
    chain.change_turn(|turn| turn.marked_block = 150);
    drive.confirm(6, 0xc1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty(), "the read's block is older than the latest mark's");
    chain.change_turn(|turn| turn.block_number = 151);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let (_, head) = *chain.posts_v2().first().expect("a held head");
    assert_eq!((head.head, head.hold, head.block_number), (at(6), Hold::BenchmarkSync, 151));
    drive.stop().await;
}

#[tokio::test(start_paused = true)]
async fn the_fence_signs_no_head() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.gate.fence(true);
    drive.release(2);
    drive.confirm(2, 0xc1);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(chain.posts_v2().is_empty());
    assert!(drive.signed.borrow().is_none(), "nothing signed reaches the stream either");
    drive.gate.fence(false);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(chain.posts_v2().len(), 1);
    drive.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_live_head_waits_for_a_signing_time_newer_than_the_chains() {
    let chain = FakeChain::new(None);
    let mut named = turn(GENERATION);
    // The chain's head was signed a minute ahead of this enclave's lower bound.
    named.signed_at = u64::try_from(TRUE_MS - 3_000 + 60_000).unwrap();
    chain.name_turn(named);
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.release(2);
    drive.confirm(2, 0xc1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty());
    drive.clock.advance(Duration::from_secs(61));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(chain.posts_v2().len(), 1);
    drive.stop().await;
}

// ── Helpers ──

impl Drive {
    /// Releases the log's tip at `seq`.
    fn release(&self, seq: u64) {
        self.released.send_replace(Released { head: at(seq), stamp: stamp(seq) });
        self.tip.send_replace(at(seq));
    }

    /// A member at `member` (its address bytes) confirmed `seq`.
    fn confirm(&self, seq: u64, member: u8) {
        let released = Released { head: at(seq), stamp: stamp(seq) };
        self.signing.send_replace(Signing::Confirmed(Signable { released, confirmer: Address::repeat_byte(member) }));
    }

    /// Stops the poster by dropping its released-head watch.
    async fn stop(self) {
        drop(self.tip);
        assert!(self.task.await.unwrap().is_ok());
    }
}

/// The poster over `chain`, with keys sealed in `keys`, a 60 s heartbeat interval and a sampled
/// trusted clock.
fn spawn(chain: FakeChain, keys: Option<(B256, u64)>) -> Drive {
    let (tip, rx) = watch::channel(Head::default());
    let (released, released_rx) = watch::channel(Released::default());
    let (signing, signing_rx) = watch::channel(Signing::Nothing);
    let (hold, hold_rx) = watch::channel(Hold::Live);
    let (signed_tx, signed) = watch::channel(None);
    let clock = Arc::new(TestClock::at(TRUE_MS));
    let sample = Sample {
        at: crate::clock::Clock::instant(clock.as_ref()),
        earliest_ms: TRUE_MS - 3_000,
        latest_ms: TRUE_MS + 3_000,
    };
    let trusted = TrustedClock::new(clock.clone(), watch::channel(Some(sample)).1, Duration::from_secs(10));
    let gate = Gate::default();
    let heartbeat = Heartbeat {
        keys,
        signing: signing_rx,
        released: released_rx,
        hold: hold_rx,
        gate: gate.clone(),
        signed: signed_tx,
        trusted: trusted.clone(),
    };
    let poster = Poster {
        chain,
        key: Arc::new(crate::tee::EnclaveKey::test_key()),
        domain: CheckpointDomain { chain_id: 1, vault: AccountId([2; 20]) },
        interval: Duration::from_secs(3600),
        retry: Duration::from_secs(1),
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        trusted: Some(trusted),
        prefix_ring: Default::default(),
        stall_intervals: 2,
        heartbeat: Some(heartbeat),
    };
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(poster.run(rx, sequencer));
    Drive { released, signing, hold, gate, signed, clock, tip, task }
}

/// Turn 20,400 at position 2 in `generation`, on a 60 s interval, read at block 100.
fn turn(generation: u64) -> TurnHeads {
    TurnHeads {
        interval: Duration::from_secs(60),
        turn: 20_400,
        position: 2,
        release: RELEASE,
        generation,
        signed_at: 0,
        bound_block: 0,
        block_number: 100,
        block_hash: B256::repeat_byte(0xb1),
        marked_block: 90,
    }
}

/// The head at `seq`.
fn at(seq: u64) -> Head {
    Head { seq, link: B256::from([u8::try_from(seq).unwrap(); 32]) }
}

/// The stamp at `seq`.
fn stamp(seq: u64) -> u64 {
    1_799_999_000_000 + seq * 100
}
