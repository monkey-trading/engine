//! The elected operator key from `V37`, through `Engine::apply`: the operator registry's rotation
//! switch and elections mirrored once under their chain references, the elected key taking effect
//! at the election's effective time or seven days after it is logged, `RotateOperator` refused while
//! rotation is on, and a cancelled election never taking effect.

mod common;

use common::{BTC, Client, Harness, operator};
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::election::{Election, OperatorElection, RegistryElection};
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, PendingRotation};
use engine_core::types::{AccountId, MAX_STAMP_MS, Millis};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The seed of the elected candidate's admin key.
const ELECTED_SEED: u8 = 0x66;
const CANDIDATE: AccountId = AccountId([0xc1; 20]);

fn elected() -> AccountId {
    Client::new(ELECTED_SEED).account
}

/// Mirrors the registry's rotation switch under `chain_ref`, returning its events.
fn switch(h: &mut Harness, enabled: bool, election_cancelled: bool, chain_ref: &str) -> Vec<Event> {
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled, election_cancelled, block, chain_ref: chain_ref.into() })
}

/// Mirrors an election of `admin_key` at `epoch`, taking effect at `effective_at` on chain.
fn elect(h: &mut Harness, epoch: u64, admin_key: AccountId, effective_at: Millis, chain_ref: &str) -> Vec<Event> {
    let block = h.log.len() as u64;
    h.cmd(Command::ElectOperator(RegistryElection {
        epoch,
        candidate: CANDIDATE,
        admin_key,
        effective_at,
        block,
        chain_ref: chain_ref.into(),
    }))
}

/// `elect`, taking effect on chain seven days from now.
fn elect_soon(h: &mut Harness, epoch: u64, admin_key: AccountId, chain_ref: &str) -> Vec<Event> {
    let effective_at = h.ts + NOTICE_MS;
    elect(h, epoch, admin_key, effective_at, chain_ref)
}

/// Records the staking mirror's finalised frontier at the next block, stamped `time` on chain.
fn finalised(h: &mut Harness, time: Millis) -> Vec<Event> {
    let block = h.log.len() as u64;
    h.cmd(Command::StakingProgress {
        block,
        block_hash: alloy_primitives::B256::repeat_byte(9),
        block_time: Some(time),
    })
}

/// `v37` with rotation on.
fn rotating() -> Harness {
    let mut h = Harness::v37();
    assert_eq!(
        switch(&mut h, true, false, "0xon:0"),
        vec![Event::RotationSwitched { enabled: true, election_cancelled: false }]
    );
    h
}

fn refused(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::ElectionRejected { reason: r, .. }) if r == reason), "{events:?}");
}

/// The outcome of a reduce-only change in BTC signed by the key from `seed`.
fn signed_by(h: &mut Harness, seed: u8, reduce_only: bool) -> Option<Event> {
    h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only }).pop()
}

#[test]
fn registry_mirrors_need_v37_and_initialised_staking() {
    let mut h = Harness::v35();
    refused(&switch(&mut h, true, false, "0xon:0"), "operator elections need protocol V37");
    let mut h = Harness::v35();
    h.admin(AdminCommand::ActivateProtocol { version: engine_core::protocol::ProtocolVersion::V37 });
    refused(&switch(&mut h, true, false, "0xon:0"), "staking is not initialised");
    refused(&elect_soon(&mut h, 1, elected(), "0xelect:0"), "staking is not initialised");
    assert!(h.engine.operator_election().is_default());
}

#[test]
fn rotation_switches_once_each_way_and_credits_its_reference_once() {
    let mut h = Harness::v37();
    refused(&switch(&mut h, false, false, "0xoff:0"), "rotation is already off");
    refused(&switch(&mut h, true, true, "0xon:0"), "turning rotation on cancels no election");
    assert_eq!(
        switch(&mut h, true, false, "0xon:0"),
        vec![Event::RotationSwitched { enabled: true, election_cancelled: false }]
    );
    assert!(h.engine.operator_election().rotation_enabled);
    assert!(h.engine.is_staking_credited("0xon:0"));
    assert_eq!(h.engine.staking().unwrap().mirrored_through, Some(h.log.len() as u64 - 1));
    refused(&switch(&mut h, false, false, "0xon:0"), "chain reference 0xon:0 is already mirrored");
    refused(&switch(&mut h, true, false, "0xon:1"), "rotation is already on");
    // A cancel needs an election waiting to take effect.
    refused(&switch(&mut h, false, true, "0xoff:1"), "no election waits to take effect");
    assert_eq!(
        switch(&mut h, false, false, "0xoff:1"),
        vec![Event::RotationSwitched { enabled: false, election_cancelled: false }]
    );
    assert!(h.engine.operator_election().is_default());
}

#[test]
fn an_election_while_rotation_is_off_or_naming_nothing_is_refused() {
    let mut h = Harness::v37();
    refused(&elect_soon(&mut h, 1, elected(), "0xelect:0"), "an election while rotation is off");
    switch(&mut h, true, false, "0xon:0");
    refused(&elect_soon(&mut h, 1, AccountId([0; 20]), "0xelect:0"), "an election names a nonzero candidate and key");
    refused(
        &elect(&mut h, 1, elected(), MAX_STAMP_MS + 1, "0xelect:0"),
        &format!("effective time {} is outside 0 to {MAX_STAMP_MS}", MAX_STAMP_MS + 1),
    );
    assert_eq!(h.engine.operator_election().last, None);
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert!(!h.engine.is_staking_credited("0xelect:0"));
}

#[test]
fn an_elected_key_takes_effect_at_the_elections_effective_time() {
    let mut h = rotating();
    let effective_at = h.ts + NOTICE_MS + DAY_MS;
    let events = elect(&mut h, 4, elected(), effective_at, "0xelect:0");
    assert_eq!(
        events.last(),
        Some(&Event::OperatorElected { epoch: 4, candidate: CANDIDATE, admin_key: elected(), effective_at })
    );
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: elected(), effective_at }));
    assert_eq!(
        h.engine.operator_election().last,
        Some(Election {
            epoch: 4,
            candidate: CANDIDATE,
            admin_key: elected(),
            effective_at,
            registry_effective_at: effective_at,
            cancelled: false
        })
    );
    // Until then the deployer's key signs and the elected one does not.
    h.advance_to(effective_at - 1);
    assert_eq!(
        signed_by(&mut h, ELECTED_SEED, true),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    // At its time the key still waits for the staking mirror's finalised frontier to reach it, so
    // no cancellation the registry made before then can arrive after the handover.
    h.advance_to(effective_at);
    finalised(&mut h, effective_at - 1);
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_eq!(h.engine.due_rotation(h.ts), None);
    finalised(&mut h, effective_at);
    assert_eq!(h.engine.due_rotation(h.ts).map(|rotation| rotation.address), Some(elected()));
    // From the next command the elected key signs and the deployer's does not.
    assert_eq!(
        signed_by(&mut h, common::OPERATOR_SEED, false),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(signed_by(&mut h, ELECTED_SEED, false), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator().map(|o| o.address), Some(elected()));
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator_bounds().rotations, 1);
}

#[test]
fn an_election_logged_late_takes_effect_seven_days_after_it_is_logged() {
    let mut h = rotating();
    let logged = h.ts;
    let events = elect(&mut h, 1, elected(), logged + DAY_MS, "0xelect:0");
    let effective_at = logged + NOTICE_MS;
    assert_eq!(
        events.last(),
        Some(&Event::OperatorElected { epoch: 1, candidate: CANDIDATE, admin_key: elected(), effective_at })
    );
    h.advance_to(logged + DAY_MS);
    finalised(&mut h, logged + DAY_MS);
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    h.advance_to(effective_at);
    assert_eq!(signed_by(&mut h, ELECTED_SEED, false), Some(Event::AdminApplied));
}

#[test]
fn an_election_naming_the_key_in_effect_records_no_rotation() {
    let mut h = rotating();
    elect_soon(&mut h, 1, operator().address, "0xelect:0");
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator_election().last.map(|last| last.admin_key), Some(operator().address));
    h.advance(NOTICE_MS);
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_bounds().rotations, 0);
}

#[test]
fn elections_move_forward_and_a_later_one_replaces_the_pending_rotation() {
    let mut h = rotating();
    elect_soon(&mut h, 3, elected(), "0xelect:0");
    refused(&elect_soon(&mut h, 3, elected(), "0xelect:1"), "election epoch 3 is not after the last one");
    let other = Client::new(0x67).account;
    let effective_at = h.ts + 2 * NOTICE_MS;
    elect(&mut h, 5, other, effective_at, "0xelect:2");
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: other, effective_at }));
    // An election back to the key in effect drops the pending rotation.
    elect_soon(&mut h, 6, operator().address, "0xelect:3");
    assert_eq!(h.engine.operator_bounds().rotation, None);
}

#[test]
fn rotate_operator_is_refused_while_rotation_is_on_and_spends_its_nonce() {
    let mut h = rotating();
    let rotation = AdminCommand::RotateOperator { address: elected() };
    let nonce = h.engine.next_request_nonce(operator().address);
    assert_eq!(
        h.operate(rotation.clone()).pop(),
        Some(Event::AdminRejected { reason: "while rotation is on, only an election moves the operator key".into() })
    );
    assert_eq!(h.engine.next_request_nonce(operator().address), nonce + 1);
    assert_eq!(h.engine.operator_bounds().rotation, None);
    switch(&mut h, false, false, "0xoff:0");
    h.operator_admin(rotation);
    assert_eq!(h.engine.operator_bounds().rotation.map(|r| r.address), Some(elected()));
}

#[test]
fn turning_rotation_on_drops_a_signed_rotation_still_pending() {
    let mut h = Harness::v37();
    let signed = Client::new(0x67);
    h.operator_admin(AdminCommand::RotateOperator { address: signed.account });
    switch(&mut h, true, false, "0xon:0");
    assert_eq!(h.engine.operator_bounds().rotation, None);
    h.advance(NOTICE_MS + DAY_MS);
    assert_eq!(signed_by(&mut h, 0x67, true), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
}

#[test]
fn a_rotation_signed_before_rotation_turned_on_gives_way_to_an_election() {
    let mut h = Harness::v37();
    h.operator_admin(AdminCommand::RotateOperator { address: Client::new(0x67).account });
    switch(&mut h, true, false, "0xon:0");
    let effective_at = h.ts + NOTICE_MS + DAY_MS;
    elect(&mut h, 1, elected(), effective_at, "0xelect:0");
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: elected(), effective_at }));
}

#[test]
fn a_signed_rotation_cannot_replace_an_elected_handover_even_with_rotation_off() {
    let mut h = rotating();
    // The election already took effect on chain, so turning rotation off cancels nothing; its
    // handover still waits out the local notice, and only it may move the key from then.
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, elected(), effective_at, "0xelect:0");
    assert_eq!(
        switch(&mut h, false, false, "0xoff:0"),
        vec![Event::RotationSwitched { enabled: false, election_cancelled: false }]
    );
    let signed = Client::new(0x67).account;
    assert_eq!(
        h.operate_as(common::OPERATOR_SEED, AdminCommand::RotateOperator { address: signed }).pop(),
        Some(Event::AdminRejected { reason: "while an elected handover waits, only it moves the operator key".into() })
    );
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: elected(), effective_at }));
    // Once the handover has applied, a signed rotation moves the key again as V35 allows.
    h.advance_to(effective_at);
    finalised(&mut h, effective_at);
    assert_eq!(signed_by(&mut h, ELECTED_SEED, true), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(
        h.operate_as(ELECTED_SEED, AdminCommand::RotateOperator { address: signed }).pop(),
        Some(Event::AdminApplied)
    );
}

#[test]
fn cancelling_a_successor_election_restores_the_handover_it_superseded() {
    let mut h = rotating();
    // A catch-up mirrors both elections before either handover applies: the first took office on
    // chain and the second was cancelled before its effective time.
    let first_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, elected(), first_at, "0xelect:0");
    let successor = Client::new(0x68).account;
    let second_at = first_at + DAY_MS;
    elect(&mut h, 2, successor, second_at, "0xelect:1");
    assert_eq!(
        h.engine.operator_bounds().rotation,
        Some(PendingRotation { address: successor, effective_at: second_at })
    );
    // The successor's cancellation restores the handover it superseded, as the registry then
    // names the first winner again.
    assert_eq!(
        switch(&mut h, false, true, "0xoff:0"),
        vec![Event::RotationSwitched { enabled: false, election_cancelled: true }]
    );
    assert_eq!(
        h.engine.operator_bounds().rotation,
        Some(PendingRotation { address: elected(), effective_at: first_at })
    );
    assert_eq!(
        h.engine.operator_election().last,
        Some(Election {
            epoch: 1,
            candidate: CANDIDATE,
            admin_key: elected(),
            effective_at: first_at,
            registry_effective_at: first_at,
            cancelled: false
        })
    );
    assert_eq!(h.engine.operator_election().prior, None);
    // The restored handover takes effect at its time once the mirror passes it.
    h.advance_to(first_at);
    finalised(&mut h, first_at);
    assert_eq!(signed_by(&mut h, ELECTED_SEED, true), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator().map(|o| o.address), Some(elected()));
}

#[test]
fn turning_rotation_off_cancels_an_election_not_yet_in_effect_even_when_mirrored_after_its_time() {
    let mut h = rotating();
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, elected(), effective_at, "0xelect:0");
    // The registry cancelled before its effective time; the mirror delivers that after the
    // engine's, which held the key until the mirror passed the registry's time.
    h.advance_to(effective_at + DAY_MS);
    finalised(&mut h, effective_at - 1);
    assert_eq!(
        switch(&mut h, false, true, "0xoff:0").last(),
        Some(&Event::RotationSwitched { enabled: false, election_cancelled: true })
    );
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator_election().last.map(|last| last.cancelled), Some(true));
    h.advance(DAY_MS);
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator().map(|o| o.address), Some(operator().address));
    // A cancelled election cannot be cancelled again once rotation is back on.
    switch(&mut h, true, false, "0xon:1");
    refused(&switch(&mut h, false, true, "0xoff:1"), "no election waits to take effect");
}

#[test]
fn a_cancel_after_the_mirror_passed_the_effective_time_is_refused() {
    let mut h = rotating();
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, elected(), effective_at, "0xelect:0");
    finalised(&mut h, effective_at);
    h.advance_to(effective_at);
    refused(&switch(&mut h, false, true, "0xoff:0"), "no election waits to take effect");
    assert!(h.engine.operator_election().rotation_enabled);
    assert_eq!(h.engine.operator().map(|o| o.address), Some(elected()));
}

#[test]
fn turning_rotation_off_keeps_an_election_in_effect_on_chain_but_deferred_here() {
    let mut h = rotating();
    let logged = h.ts;
    elect(&mut h, 1, elected(), logged + DAY_MS, "0xelect:0");
    // On chain the election took effect after a day; the engine waits out its seven days.
    h.advance_to(logged + 2 * DAY_MS);
    switch(&mut h, false, false, "0xoff:0");
    finalised(&mut h, logged + 2 * DAY_MS);
    h.advance_to(logged + NOTICE_MS);
    assert_eq!(signed_by(&mut h, ELECTED_SEED, true), Some(Event::AdminApplied));
}

#[test]
fn the_mirrored_registry_is_in_the_state_root_and_survives_a_snapshot() {
    let root_with = |enable: bool| {
        let mut h = Harness::v37();
        if enable {
            switch(&mut h, true, false, "0xon:0");
        } else {
            // Same seq and time, a different command that changes nothing.
            h.cmd(Command::Tick);
        }
        h.engine.state_root()
    };
    assert_ne!(root_with(true), root_with(false));

    let mut h = rotating();
    let effective_at = h.ts + NOTICE_MS;
    elect(&mut h, 1, elected(), effective_at, "0xelect:0");
    finalised(&mut h, effective_at);
    let mut restored: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    restored.restore_indexes();
    assert_eq!(restored.operator_election(), h.engine.operator_election());
    assert_ne!(*h.engine.operator_election(), OperatorElection::default());
    h.advance_to(effective_at);
    let next = engine_core::command::Sequenced {
        seq: h.log.len() as u64,
        ts: h.ts,
        cmd: Client { expires_at: h.ts + 60_000, ..Client::new(ELECTED_SEED) }
            .admin(0, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true }),
    };
    let from_restored = restored.apply(&next).unwrap();
    assert_eq!(from_restored, h.cmd(next.cmd));
    assert_eq!(from_restored.last(), Some(&Event::AdminApplied));
    assert_eq!(restored.state_root(), h.engine.state_root());
}

#[test]
fn registry_mirrors_advance_the_staking_frontier_that_progress_extends() {
    let mut h = rotating();
    let block = h.log.len() as u64;
    elect_soon(&mut h, 1, elected(), "0xelect:0");
    assert_eq!(h.engine.staking().unwrap().mirrored_through, Some(block));
    let hash = alloy_primitives::B256::repeat_byte(9);
    let progress = |block, block_time| Command::StakingProgress { block, block_hash: hash, block_time };
    let rejected = |events: Vec<Event>| matches!(events.last(), Some(Event::AdminRejected { .. }));
    assert!(rejected(h.cmd(progress(block - 1, None))));
    assert!(!rejected(h.cmd(progress(block, None))));
    // From `V37` the frontier carries its block time, which never moves back.
    assert!(!rejected(h.cmd(progress(block + 1, Some(h.ts)))));
    assert!(rejected(h.cmd(progress(block + 2, Some(h.ts - 1)))));
    assert!(!rejected(h.cmd(progress(block + 2, None))));
    let reached = h.engine.staking().unwrap().completed_through.and_then(|checkpoint| checkpoint.time);
    assert_eq!(reached, Some(h.ts), "a frontier without a time keeps the time reached");
    // Before `V37` a frontier carries no time.
    let mut older = Harness::staking(engine_core::Fixed::from_int(1_000_000), DAY_MS, 3 * DAY_MS);
    older.admin(AdminCommand::ActivateProtocol { version: engine_core::protocol::ProtocolVersion::V27 });
    let block = older.log.len() as u64;
    assert!(rejected(older.cmd(progress(block, Some(older.ts)))));
    assert!(!rejected(older.cmd(progress(block, None))));
}
