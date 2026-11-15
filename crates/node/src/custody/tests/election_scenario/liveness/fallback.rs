//! The successor-fallback dev scenario (#596), which `scripts/dev-daily-rotation.sh` runs. On Anvil,
//! with the chain's blocks as Arbitrum's and the enclaves' trusted time on the chain's clock, A, B, C
//! and D register with vouched standby keys and a close adopts them; the schedule's take window is two
//! 30 s slots, and a promotion needs three missed heads. The key holder runs the real head poster and
//! liveness rule, asking the other members' real member links for their copies, and every member runs
//! the real liveness caller, which witnesses, skips and promotes. A handover is the holder's poster
//! stopping and the next one's starting with the keys sealed in the assignment's generation, as the
//! handover's own tests take them (`handover::predecessor::tests::assignment`). It checks:
//! 1. B is stopped before its boundary: through the take window A, the holder of the turn before,
//!    trades with its log confirmed and nothing promoted; the callers skip the turn to C; the latest
//!    agreed block's read shows the skip and the newest head's release names A, whom C asks first;
//!    and once C holds the keys its head latches C's assignment;
//! 2. at the next boundary B takes the keys late from C, its first head landing only after a skip
//!    names D, passing over C, the turn's source: that head latches nothing, and D's latches D's
//!    assignment;
//! 3. block production pauses across the next boundary and resumes: no skip or promotion is due in
//!    the first resumed block, and the turn's member, A, takes the turn;
//! 4. at the next boundary A refuses every successor: the callers skip past B to C, A beats on and
//!    stops by its own clock before a promotion past it becomes legal, the promotion moves the vault's
//!    signer to D's standby key within `heartbeatMisses + 1` slots of the boundary plus a poll, and a
//!    checkpoint signed by A's key is refused.

use std::time::Duration;

use alloy::primitives::Address;
use alloy::sol;
use alloy::sol_types::SolError;
use engine_core::types::AccountId;
use serde_json::json;

use super::super::{DIGEST_A, DIGEST_B};
use super::rig::warp;
use super::{
    BOUNDARY_OFFSET_SECS, DAY_SECS, DIGEST_C, SET_EFFECTIVE_SECS, checkpointSignerCall, latchesCall, turnOfCall,
};
use crate::handover::assigned::Reason;
use crate::standby_key::chain::StandbyChain;

sol! {
    struct TakeWindowArg { uint16 slots; uint16 minimum; uint16 maximum; }
    function skipDueAt() external view returns (uint64);
    function headRelease(uint64 epoch) external view returns (bytes32);
    function memberAt(uint32 setId, bytes32 seed, uint64 turn, uint16 position) external view returns (address);
    function skip() external returns (uint64 turn, uint16 position);
    function promote() external returns (uint64 turn, uint16 position);
    function postCheckpoint(uint64 seq, bytes32 root, bool hasParent, uint64 parentSeq, bytes32 parentRoot, bytes signature) external;
    error InvalidCheckpointSigner();
    function seedReceiver() external view returns (address);
    function signerSince() external view returns (uint64);
    struct AssignmentArg {
        address candidate;
        address adminKey;
        uint32 setId;
        uint8 reason;
        address standbyKey;
        uint64 signerEpoch;
        uint64 headSeq;
        bytes32 headLink;
        uint64 checkpointSeq;
        bytes32 checkpointRoot;
        uint64 at;
    }
    struct RevisionArg { bytes32 release; uint64 generation; }
    function assignmentOf(uint64 turn, uint16 position) external view returns (AssignmentArg memory, RevisionArg[] memory);
}

/// The members' admin keys, digests and stakes: A, B, C and D.
const KEYS: [[u8; 32]; 4] = [[0x42; 32], [0x55; 32], [0x56; 32], [0x57; 32]];
const DIGESTS: [[u8; 48]; 4] = [DIGEST_A, DIGEST_B, DIGEST_C, [0xd1; 48]];
const STAKES: [u64; 4] = [40_000, 30_000, 20_000, 10_000];
const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
const D: usize = 3;
/// The heartbeat rule: 30 s slots, 20 s tolerance, 3 misses, 10 blocks, so a promotion past an untaken
/// turn is due five slots after its boundary, two past the take window's first skip and one before its
/// second.
const RULE: (u64, u64, u16, u16) = (30, 20, 3, 10);
/// The take window, in slots, its bounds, and the heartbeat rule's bounds under a 5 s switch budget: one
/// slot past the minimum's first, 30 s at the shortest interval, exceeds the budget plus the longest
/// tolerance, 20 s.
const TAKE: u16 = 2;
const MINIMUMS: (u64, u64, u16, u16) = (30, 5, 2, 10);
const MAXIMUMS: (u64, u64, u16, u16) = (120, 20, 9, 120);
const SWITCH_BUDGET_SECS: u64 = 5;
/// How often the scenario extends the holder's log and mines a block.
const STEP: Duration = Duration::from_secs(4);

mod world;
use world::World;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-daily-rotation.sh runs it"]
async fn dev_scenario_a_down_successor_falls_back_to_the_next_member() {
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .finish(),
    );
    let mut world = World::start().await;
    let first = (SET_EFFECTIVE_SECS + DAY_SECS - BOUNDARY_OFFSET_SECS) / DAY_SECS + 1;
    let turns = [first, first + 1, first + 2, first + 3, first + 4];
    // Each turn's seed, relayed before its draw deadline, picks its order's first positions.
    let orders: [&[usize]; 5] = [&[A], &[B, C], &[B, D], &[A], &[B, C, D]];
    for (turn, order) in turns.into_iter().zip(orders) {
        world.relay(turn, order).await;
    }

    // A holds turn 1: its heads latch it.
    world.draw_at_deadline(turns[0]).await;
    warp(&world.chain, &world.clock, boundary(turns[0]) + 1).await;
    world.hold(A, turns[0]).await;
    world.until_latched(turns[0]).await;
    world.draw(turns[1]).await;
    println!("PASS: A holds turn {} and its first head latched it", turns[0]);

    // 1. B is stopped: A trades through the take window, the callers skip to C, C takes the keys.
    warp(&world.chain, &world.clock, boundary(turns[1]) + 1).await;
    let due = world.chain.view(world.domain.standby.schedule, skipDueAtCall {}).await;
    assert_eq!(due, (boundary(turns[1]) / RULE.0 + u64::from(TAKE) + 1) * RULE.0, "the window from the boundary");
    let skipped_at = world.until_skipped(turns[1], C, true).await;
    assert!(skipped_at >= due, "skipped at {skipped_at}, before its window passed at {due}");
    assert_eq!(world.epoch().await, 1, "nothing was promoted while A beat");
    let reader =
        crate::custody::StandbyReader::connect_named(&world.cfg_of(A).custody, world.domain.log_heads, "s").unwrap();
    let block = reader.latest().await.unwrap();
    let (chain_id, vault) = (world.domain.standby.chain_id, world.domain.standby.vault);
    let assigned = crate::handover::assigned::read(&block, chain_id, vault, world.domain.log_heads).await.unwrap();
    let assigned = assigned.expect("a turn names the operator");
    assert_eq!((assigned.reason, assigned.candidate), (Reason::Skip, AccountId(world.members[C].candidate.0.0)));
    let members = crate::handover::discovery::members(&block, chain_id, vault, &assigned).await.unwrap();
    drop(block);
    let holder = crate::handover::discovery::holder(assigned.head_release, &members, &[]).map(|h| h.candidate);
    assert_eq!(holder, Some(AccountId(world.members[A].candidate.0.0)), "C asks A, whose head is newest, first");
    world.hold(C, turns[1]).await;
    world.until_latched(turns[1]).await;
    let release = world.chain.view(world.domain.log_heads, headReleaseCall { epoch: 1 }).await;
    assert_eq!(release, world.release(C), "the newest head names C's release");
    println!(
        "PASS: with B stopped, A traded through the take window, the callers skipped turn {} to C at {skipped_at} \
         (window passed at {due}), C asked A first, and C's head latched its assignment",
        turns[1]
    );

    // 2. B takes the keys late from C, the holder, and its first head lands after a skip names D, past C,
    //    the turn's source.
    world.draw(turns[2]).await;
    let drawn = world.chain.view(world.domain.standby.schedule, turnOfCall { turn: turns[2] }).await.generation;
    warp(&world.chain, &world.clock, boundary(turns[2]) + 1).await;
    let skipped_at = world.until_skipped(turns[2], D, false).await;
    world.hold_in(B, drawn).await;
    world.beat(2).await;
    let latch = world.chain.view(world.domain.log_heads, latchesCall { turn: turns[2], position: 0 }).await;
    assert_eq!(latch.at, 0, "B's late head latched its superseded assignment");
    assert_eq!(world.chain.view(world.domain.log_heads, headReleaseCall { epoch: 1 }).await, world.release(B));
    world.hold(D, turns[2]).await;
    let latched_at = world.until_latched(turns[2]).await;
    assert!(latched_at < skipped_at + RULE.0, "D took the turn {} s after the skip", latched_at - skipped_at);
    println!(
        "PASS: B took the keys late, its head latched nothing, and D's latched turn {} {} s after the skip",
        turns[2],
        latched_at - skipped_at
    );

    // 3. Blocks stop before the next boundary and resume past it: nothing is due in the first resumed
    //    block, and A, the turn's member, takes it.
    world.draw(turns[3]).await;
    warp(&world.chain, &world.clock, boundary(turns[3]) - 20).await;
    world.chain.rpc("evm_setAutomine", json!([false])).await;
    let resume = boundary(turns[3]) + 5 * RULE.0;
    world.pause_until(resume).await;
    // The first resumed block, with whatever the members sent meanwhile.
    world.chain.mine_at(resume).await;
    world.chain.rpc("evm_setAutomine", json!([true])).await;
    let schedule = world.domain.standby.schedule;
    let skip_due = world.chain.view(schedule, skipDueAtCall {}).await;
    let promotion_due = world.chain.view(schedule, super::promotionDueAtCall {}).await;
    assert!(skip_due > resume && promotion_due > resume, "due in the first resumed block: {skip_due}, {promotion_due}");
    world.hold(A, turns[3]).await;
    world.until_latched(turns[3]).await;
    assert_eq!(world.chain.view(schedule, turnOfCall { turn: turns[3] }).await.reason, 0, "no skip");
    assert_eq!(world.epoch().await, 1, "no promotion");
    println!("PASS: across a paused chain nothing was due in the first resumed block, and A took turn {}", turns[3]);

    // 4. A refuses every successor: skipped past B to C, then promoted past while it still beats.
    world.draw(turns[4]).await;
    warp(&world.chain, &world.clock, boundary(turns[4]) + 1).await;
    let (promoted_at, fenced_at) = world.until_promoted().await;
    let window = (u64::from(RULE.2) + 2) * RULE.0;
    assert!(fenced_at < promoted_at, "A fenced at {fenced_at}, not before the promotion at {promoted_at}");
    assert!(
        promoted_at <= boundary(turns[4]) + window + 15,
        "promoted {} s after the boundary",
        promoted_at - boundary(turns[4])
    );
    let named = world.chain.view(schedule, turnOfCall { turn: turns[4] }).await;
    assert_eq!((named.candidate, named.reason), (world.members[D].candidate, 2), "D was promoted");
    let signer = world.chain.view(world.chain.vault, checkpointSignerCall {}).await;
    assert_eq!(signer, Address::from(world.members[D].standby.address().0));
    let refused = world.refused_checkpoint().await;
    assert_eq!(&refused[..4], InvalidCheckpointSigner::SELECTOR, "A's checkpoint after the promotion");
    println!(
        "PASS: A refused every successor; the callers skipped turn {} to C, A fenced at {fenced_at}, the promotion \
         past it landed at {promoted_at}, {} s after the boundary, and A's checkpoint was refused",
        turns[4],
        promoted_at - boundary(turns[4])
    );
    world.stop();
}

/// When `turn` starts, in Unix seconds.
fn boundary(turn: u64) -> u64 {
    turn * DAY_SECS + BOUNDARY_OFFSET_SECS
}
