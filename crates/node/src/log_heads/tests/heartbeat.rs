//! The poster's v2 heartbeat while a turn names the operator: heads only where the liveness rule lets
//! it sign, binding the assignment whose keys the enclave holds, held heads through a hold, nothing
//! while the fence stops the enclave.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::log_heads::v2::HeldKeys;
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
    trusted: TrustedClock,
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

/// A running holder whose keys the sequencer reseals into a fresh generation for its next turn binds
/// that turn's assignment from its next head: the generation is read at each head, not at start.
#[tokio::test(start_paused = true)]
async fn a_reseal_while_running_binds_the_new_generations_assignment() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION + 1));
    let generation = Arc::new(AtomicU64::new(GENERATION));
    let held = generation.clone();
    let drive = spawn_reading(chain.clone(), Some(Arc::new(move || (RELEASE, held.load(Ordering::SeqCst)))));
    drive.release(3);
    drive.confirm(3, 0xc1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let (_, head) = *chain.posts_v2().last().expect("a head went out");
    assert_eq!((head.turn, head.generation), (0, 0), "before the reseal the turn's generation is not held");
    generation.store(GENERATION + 1, Ordering::SeqCst);
    drive.release(4);
    drive.confirm(4, 0xc1);
    drive.clock.advance(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(30)).await;
    let (_, head) = *chain.posts_v2().last().expect("a later head went out");
    assert_eq!(head.head, at(4));
    assert_eq!((head.turn, head.position, head.generation), (20_400, 2, GENERATION + 1), "the resealed assignment");
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

/// A checkpoint retry as long as the heartbeat interval still lets heads out at least twice per
/// interval: the heartbeat paces the reads a post waits for.
#[tokio::test(start_paused = true)]
async fn heads_go_out_twice_per_interval_whatever_the_checkpoint_retry() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let keys: HeldKeys = Arc::new(|| (RELEASE, GENERATION));
    let drive = spawn_retrying(chain.clone(), Some(keys), Duration::from_secs(60));
    for seq in 1..=12 {
        drive.release(seq);
        drive.confirm(seq, 0xc1);
        drive.clock.advance(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let posted = chain.posts_v2();
    // Two minutes of a 60 s interval with a 60 s retry: the retry alone would allow two heads.
    assert!(posted.len() >= 4, "{posted:?}");
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

/// A confirmation counts only under the signer epoch and rotation set it was given in: once the turn in
/// effect is drawn from another set, a former member's confirmation signs no head until a member of
/// the new set confirms.
#[tokio::test(start_paused = true)]
async fn a_confirmation_from_another_set_signs_no_head() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.change_turn(|turn| turn.set = 2);
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.release(5);
    drive.confirm(5, 0xc1);
    drive.clock.advance(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty(), "set 1's confirmation signed a head for a turn of set 2");
    let released = Released { head: at(5), stamp: stamp(5) };
    let confirmer = Address::repeat_byte(0xc2);
    drive.signing.send_replace(Signing::Confirmed(Signable {
        released,
        confirmer,
        scope: Some((1, 2)),
        until: u64::MAX,
    }));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(chain.posts_v2().last().map(|(_, head)| head.confirmer), Some(confirmer), "set 2's member's");
    drive.stop().await;
}

/// A one-member set's exemption signs at the tip only under its own signer epoch and set: once the turn
/// in effect is drawn from another set the rule has not read, the tip signs no head.
#[tokio::test(start_paused = true)]
async fn a_tip_from_another_set_signs_no_head() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    chain.change_turn(|turn| turn.set = 2);
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.release(5);
    drive.signing.send_replace(Signing::Tip(Some((1, 1)), u64::MAX));
    drive.clock.advance(Duration::from_secs(1));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty(), "set 1's exemption signed the tip for a turn of set 2");
    drive.signing.send_replace(Signing::Tip(Some((1, 2)), u64::MAX));
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(chain.posts_v2().last().map(|(_, head)| head.confirmer), Some(Address::ZERO), "set 2's tip");
    drive.stop().await;
}

/// A confirmation, or a one-member set's exemption, signs a head only before its own boundary from which
/// another set may apply, as it lets checkpoints pass: once a read past that boundary renewed the gate
/// for a set whose members have yet to answer, and with the poster's own turn read still pinned before
/// it, no head goes out at the old permission until a round gives the permission the next boundary.
#[tokio::test(start_paused = true)]
async fn past_its_boundary_a_permission_signs_no_head_whatever_read_renewed_the_gate() {
    let confirmed = |seq, until| {
        let (released, confirmer) = (Released { head: at(seq), stamp: stamp(seq) }, Address::repeat_byte(0xc1));
        Signing::Confirmed(Signable { released, confirmer, scope: Some((1, 1)), until })
    };
    let exempt = |_, until| Signing::Tip(Some((1, 1)), until);
    let cases: [(&str, &dyn Fn(u64, u64) -> Signing); 2] = [("a confirmation", &confirmed), ("an exempt tip", &exempt)];
    for (case, signing) in cases {
        let chain = FakeChain::new(None);
        chain.name_turn(turn(GENERATION));
        let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
        let boundary = u64::try_from(TRUE_MS).unwrap() + 60_000;
        drive.gate.deadline(Some(u64::MAX - 1), &drive.trusted);
        // A read past the boundary renewed the gate for the next day's set.
        drive.gate.holds_until(boundary + 86_400_000);
        drive.release(5);
        drive.signing.send_replace(signing(5, boundary));
        drive.clock.advance(Duration::from_secs(1));
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(chain.posts_v2().len(), 1, "{case}: before the boundary");
        // Past it by trusted time's upper bound: nothing at the old permission, at a newer seq included.
        drive.clock.advance(Duration::from_secs(60));
        drive.release(6);
        drive.signing.send_replace(signing(6, boundary));
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(chain.posts_v2().len(), 1, "{case}: a head past the boundary");
        // A round under a read that names this set past the boundary gives it the next one.
        drive.signing.send_replace(signing(6, boundary + 86_400_000));
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(chain.posts_v2().last().map(|(_, head)| head.head), Some(at(6)), "{case}: once renewed");
        drive.stop().await;
    }
}

/// A node whose start check has not passed stays stopped, heads included: with a member's
/// confirmation, through a hold on newer blocks, no head goes out until the check passes.
#[tokio::test(start_paused = true)]
async fn no_head_goes_out_before_the_start_check_passes_even_through_a_hold() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.gate.checked(false);
    drive.hold.send_replace(Hold::BenchmarkSync);
    drive.release(4);
    drive.confirm(4, 0xc1);
    for block in [110, 120, 130] {
        chain.change_turn(|turn| turn.block_number = block);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    assert!(chain.posts_v2().is_empty(), "a head went out before the start check passed");
    drive.gate.checked(true);
    chain.change_turn(|turn| turn.block_number = 140);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(chain.posts_v2().len(), 1, "once the check passed, the held head goes out");
    drive.stop().await;
}

/// A held head signs at the newest confirmed seq, though the tip past it reached no member (as through
/// a trusted-time halt, when no entry can be framed), once the block read is no older than the mark's.
#[tokio::test(start_paused = true)]
async fn a_held_head_signs_at_the_confirmed_seq_on_a_block_no_older_than_the_mark() {
    let chain = FakeChain::new(None);
    chain.name_turn(turn(GENERATION));
    let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
    drive.hold.send_replace(Hold::BenchmarkSync);
    drive.release(6);
    drive.confirm(5, 0xc1);
    chain.change_turn(|turn| turn.marked_block = 150);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(chain.posts_v2().is_empty(), "the read's block is older than the latest mark's");
    chain.change_turn(|turn| turn.block_number = 151);
    tokio::time::sleep(Duration::from_secs(30)).await;
    let (_, head) = *chain.posts_v2().first().expect("a held head");
    assert_eq!((head.head, head.hold, head.block_number), (at(5), Hold::BenchmarkSync, 151));
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

/// Two copies of one key: a restored copy beside the holder, with an identical prefix and open reads,
/// freezes under the heartbeat at its first read after the holder's conflicting head is posted, at a seq
/// it holds under another link or past its tip, including when that head's inclusion is delayed past
/// the copy's own heads.
#[tokio::test(start_paused = true)]
async fn a_restored_copy_freezes_at_its_first_read_of_the_holders_conflicting_head() {
    let holders = [
        ("another link at the copy's tip", Head { seq: 8, link: B256::repeat_byte(0xee) }, false, "foreign"),
        ("past the copy's tip", Head { seq: 12, link: B256::repeat_byte(0xee) }, false, "overtakes"),
        ("included after the copy's own head", Head { seq: 8, link: B256::repeat_byte(0xee) }, true, "foreign"),
    ];
    for (case, holder, delayed, refusal) in holders {
        let chain = FakeChain::new(None);
        chain.name_turn(turn(GENERATION));
        let drive = spawn(chain.clone(), Some((RELEASE, GENERATION)));
        drive.release(8);
        drive.confirm(8, 0xc1);
        drive.clock.advance(Duration::from_secs(1));
        if delayed {
            tokio::time::sleep(Duration::from_secs(30)).await;
            assert_eq!(chain.posts_v2().last().map(|(_, head)| head.head), Some(at(8)), "{case}: posted first");
        }
        chain.set_posted(holder);
        // The poster reads every retry (1 s): the freeze comes at the first read after the post.
        let ended = drive.ended(Duration::from_secs(2)).await.unwrap_or_else(|| panic!("{case}: did not freeze"));
        let error = format!("{:#}", ended.expect_err("a freeze stops the poster"));
        assert!(error.contains(refusal), "{case}: {error}");
    }
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
        let confirmer = Address::repeat_byte(member);
        self.signing.send_replace(Signing::Confirmed(Signable {
            released,
            confirmer,
            scope: Some((1, 1)),
            until: u64::MAX,
        }));
    }

    /// How the poster ended within `within`, its inputs kept open meanwhile; `None` if it ran on.
    async fn ended(self, within: Duration) -> Option<anyhow::Result<()>> {
        let Drive { task, released: _released, signing: _signing, hold: _hold, tip: _tip, .. } = self;
        tokio::time::timeout(within, task).await.ok().map(|joined| joined.unwrap())
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
    spawn_reading(
        chain,
        keys.map(|held| {
            let read: HeldKeys = Arc::new(move || held);
            read
        }),
    )
}

/// `spawn`, reading the keys' release and generation from `keys` at each head.
fn spawn_reading(chain: FakeChain, keys: Option<HeldKeys>) -> Drive {
    spawn_retrying(chain, keys, Duration::from_secs(1))
}

/// `spawn_reading`, with the checkpoint retry at `retry`.
fn spawn_retrying(chain: FakeChain, keys: Option<HeldKeys>, retry: Duration) -> Drive {
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
        retry,
        freeze: crate::log_heads::FreezeLatch::default(),
        confirmed: None,
        anchored: None,
        trusted: Some(trusted.clone()),
        prefix_ring: Default::default(),
        stall_intervals: 2,
        heartbeat: Some(heartbeat),
    };
    let (sequencer, _from_poster) = tokio::sync::mpsc::channel(8);
    let task = tokio::spawn(poster.run(rx, sequencer));
    Drive { released, signing, hold, gate, signed, clock, trusted, tip, task }
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
        set: 1,
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
