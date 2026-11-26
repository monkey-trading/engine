//! The custody quorum's reads at one block of what a holder's admission rests on: the vault's release,
//! its generation and fold, the turn the finalised schedule names as the checkpoint endpoint reads it,
//! and the rotation switch. The comparison with the endpoint is held end to end by
//! `predecessor::tests::quorum`.

use alloy::primitives::B256;

use super::*;
use crate::handover::assigned::tests::{REGISTRY, SCHEDULE, Scripted, VAULT};
use crate::liveness::chain::rotationStateReturn;
use crate::standby_key::chain::{CandidateReturn, TurnReturn, turnAtReturn};

const TURN: u64 = 20_800;
const MEMBER: Address = Address::repeat_byte(0xc1);

#[tokio::test]
async fn the_quorum_reads_the_vaults_release_its_fold_and_the_turn_the_schedule_names() {
    let mut block = Scripted::scheduled();
    script_release(&mut block);
    let now = boundary() + 60_000;
    script_turn(&mut block, now);
    let held = held_at(&&block, VAULT, Some(now)).await.unwrap();
    assert_eq!(held.current, CurrentRelease { id: [0x22; 32], generation: 7 }, "the vault's current release");
    assert_eq!(held.synced, [0x21; 32], "the release its generation folded");
    let turn = TurnFacts {
        turn: TURN,
        boundary: boundary(),
        candidate: AccountId(MEMBER.0.0),
        admin_key: AccountId([0xd1; 20]),
        release: [0x33; 32],
        generation: 12,
        set: 4,
    };
    assert_eq!(held.turn, Some(turn), "the turn, with its member's registered key and release");
}

#[tokio::test]
async fn no_time_no_registry_no_schedule_or_no_drawn_turn_reads_as_no_turn() {
    let now = boundary() + 60_000;
    let mut block = Scripted::scheduled();
    script_release(&mut block);
    assert_eq!(held_at(&&block, VAULT, None).await.unwrap().turn, None, "a turn read with no time");
    let undrawn = TurnReturn { candidate: Address::ZERO, ..drawn() };
    let timestamp = u64::try_from(now / 1000).unwrap();
    block.answer(SCHEDULE, turnAtCall { timestamp }, &turnAtReturn { turn: 0, _1: undrawn });
    assert_eq!(held_at(&&block, VAULT, Some(now)).await.unwrap().turn, None, "an undrawn turn");

    let mut unscheduled = Scripted::default();
    unscheduled.answer(VAULT, registryCall {}, &REGISTRY).answer(REGISTRY, scheduleCall {}, &Address::ZERO);
    script_release(&mut unscheduled);
    assert_eq!(held_at(&&unscheduled, VAULT, Some(now)).await.unwrap().turn, None, "no schedule");

    let mut unregistered = Scripted::default();
    unregistered.answer(VAULT, registryCall {}, &Address::ZERO);
    script_release(&mut unregistered);
    let held = held_at(&&unregistered, VAULT, Some(now)).await.unwrap();
    assert_eq!((held.current.id, held.turn), ([0x22; 32], None), "no registry");
}

#[tokio::test]
async fn a_turn_an_installation_ended_reads_as_no_turn() {
    let now = boundary() + 60_000;
    let mut block = Scripted::scheduled();
    script_release(&mut block);
    script_turn(&mut block, now);
    let until = u64::try_from((boundary() + TURN_MS) / 1000).unwrap();
    let ended = rotationStateReturn { enabled: false, release: B256::ZERO, generation: 0, until };
    block.answer(REGISTRY, rotationStateCall {}, &ended);
    assert_eq!(held_at(&&block, VAULT, Some(now)).await.unwrap().turn, None, "a turn an installation ended");
}

#[tokio::test]
async fn the_rotation_switch_reads_off_with_no_registry_and_as_the_registry_holds_it_otherwise() {
    let mut block = Scripted::default();
    block.answer(VAULT, registryCall {}, &Address::ZERO);
    assert!(!rotation_at(&&block, VAULT).await.unwrap(), "a vault with no registry");
    let mut block = Scripted::default();
    block.answer(VAULT, registryCall {}, &REGISTRY).answer(REGISTRY, rotationEnabledCall {}, &true);
    assert!(rotation_at(&&block, VAULT).await.unwrap(), "a registry with rotation on");
}

// ── Helpers ──

/// The vault names release `0x22…` in generation 7, its generation having folded `0x21…`.
fn script_release(block: &mut Scripted) {
    block
        .answer(VAULT, currentReleaseCall {}, &B256::repeat_byte(0x22))
        .answer(VAULT, releaseGenerationCall {}, &7)
        .answer(VAULT, syncedReleaseCall {}, &B256::repeat_byte(0x21));
}

/// The schedule names `TURN`, drawn for `MEMBER` from set 4 in generation 12, at `now`, and the
/// registry `MEMBER`'s admin key `0xd1…` and release `0x33…`.
fn script_turn(block: &mut Scripted, now: Millis) {
    let timestamp = u64::try_from(now / 1000).unwrap();
    block.answer(SCHEDULE, turnAtCall { timestamp }, &turnAtReturn { turn: TURN, _1: drawn() });
    let member = CandidateReturn {
        adminKey: Address::repeat_byte(0xd1),
        enclave: Address::repeat_byte(0xe1),
        bond: alloy::primitives::U256::from(1),
        release: B256::repeat_byte(0x33),
        registration: 1,
        apiHostname: "member.test".into(),
        handoverEndpoint: "member.test:7000".into(),
    };
    block.answer(REGISTRY, candidateOfCall { candidate: MEMBER }, &member);
}

/// `TURN`'s draw for `MEMBER`.
fn drawn() -> TurnReturn {
    TurnReturn {
        candidate: MEMBER,
        position: 0,
        reason: 0,
        setId: 4,
        generation: 12,
        prior: TURN - 1,
        seed: B256::ZERO,
        source: Address::repeat_byte(0xa1),
    }
}

/// `TURN`'s boundary.
fn boundary() -> Millis {
    engine_core::turns::boundary(TURN).unwrap()
}
