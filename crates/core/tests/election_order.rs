//! From `V88`, through `Engine::apply`: an election still waiting to take effect when its successor is
//! mirrored takes effect first. The registry elects only once the election before has taken effect on
//! chain, and the node mirrors a chunk's `Elected` event before the frontier that chunk reaches, so a
//! successor made in the first blocks at or after its predecessor's effective time arrives while the
//! predecessor still waits. The predecessor's key and candidate then hold the role from its time to the
//! successor's, as the registry names them; before `V88` the successor superseded it.

mod common;
#[allow(dead_code)]
#[path = "staking/support.rs"]
mod support;

use common::{BTC, Client, Harness, operator};
use engine_core::command::{AdminCommand, Command};
use engine_core::election::{RegistryElection, RegistryInstallation};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, PendingRotation};
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::{AccountId, Millis};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The registry's notice and finality margin at the launch parameters: eight days.
const REGISTRY_NOTICE_MS: Millis = NOTICE_MS + DAY_MS;
const FIRST_SEED: u8 = 0x66;
const SECOND_SEED: u8 = 0x68;
const THIRD_SEED: u8 = 0x6a;
const FOURTH_SEED: u8 = 0x6c;
const HOUR_MS: Millis = 60 * 60 * 1000;

#[test]
fn a_successor_mirrored_before_the_frontier_passes_its_predecessors_time_waits_for_it() {
    let mut h = rotating(ProtocolVersion::V88);
    let first_at = first_election_waits(&mut h);
    // At the first's time the registry seats it and elects the second, whose event the node mirrors
    // before its chunk's frontier.
    h.advance_to(first_at + 60_000);
    let second_at = first_at + REGISTRY_NOTICE_MS;
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), second_at);
    assert_eq!(
        h.engine.operator_bounds().rotation,
        Some(PendingRotation { address: key(FIRST_SEED), effective_at: first_at }),
        "the first's handover still waits"
    );
    finalised(&mut h, first_at + 60_000);

    // A day into the first's term on chain, its key signs and the second's does not.
    h.advance_to(first_at + DAY_MS);
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_candidate(), Some(candidate(FIRST_SEED)));
    assert_eq!(signed_by(&mut h, SECOND_SEED), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(
        h.engine.operator_bounds().rotation,
        Some(PendingRotation { address: key(SECOND_SEED), effective_at: second_at }),
        "the second's handover waits from the first's time"
    );

    // The second takes over at its own time once the mirror passes it.
    h.advance_to(second_at);
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminApplied), "the frontier has not passed it");
    finalised(&mut h, second_at);
    assert_eq!(signed_by(&mut h, SECOND_SEED), Some(Event::AdminApplied));
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(h.engine.operator_candidate(), Some(candidate(SECOND_SEED)));
}

#[test]
fn an_incumbent_elected_again_at_its_successors_time_hands_over_and_back() {
    let mut h = rotating(ProtocolVersion::V88);
    let first_at = first_election_waits(&mut h);
    // The incumbent the first election ousted wins the second, which names the key in effect.
    h.advance_to(first_at + 60_000);
    let second_at = first_at + REGISTRY_NOTICE_MS;
    elect(&mut h, 2, common::OPERATOR_SEED, operator().address, second_at);
    finalised(&mut h, first_at + 60_000);

    h.advance_to(first_at + DAY_MS);
    let rejected = Some(Event::AdminRejected { reason: "invalid signature".into() });
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED), rejected, "the registry replaced the incumbent");
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminApplied));

    h.advance_to(second_at);
    finalised(&mut h, second_at);
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED), Some(Event::AdminApplied));
    assert_eq!(signed_by(&mut h, FIRST_SEED), rejected);
}

#[test]
fn cancelling_the_successor_leaves_the_waiting_election_to_take_effect() {
    let mut h = rotating(ProtocolVersion::V88);
    let first_at = first_election_waits(&mut h);
    h.advance_to(first_at + 60_000);
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), first_at + REGISTRY_NOTICE_MS);
    // Rotation turned off cancels the second, which the registry has not seated.
    let block = h.log.len() as u64;
    let events =
        h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref: "0xoff:0".into() });
    assert_eq!(events, vec![Event::RotationSwitched { enabled: false, election_cancelled: true }]);
    let last = h.engine.operator_election().last.expect("an election is recorded");
    assert_eq!((last.epoch, last.cancelled), (1, false), "the first is the last again");
    assert!(h.engine.operator_election().waiting.is_empty());
    finalised(&mut h, first_at + 60_000);
    h.advance_to(first_at + DAY_MS);
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_bounds().rotation, None, "no handover follows the first");
}

/// A catch-up mirrors two elections the chain seated long ago, so each takes effect seven days after it
/// is logged, a moment apart. The first command after both hands over to the first and then the second,
/// so it is the second's key that signs it, with the second's candidate seated.
#[test]
fn a_command_after_two_caught_up_elections_are_due_is_the_seconds() {
    let mut h = rotating(ProtocolVersion::V88);
    let logged = h.ts;
    elect(&mut h, 1, FIRST_SEED, key(FIRST_SEED), logged - 2 * DAY_MS);
    h.advance_to(logged + 1);
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), logged - DAY_MS);
    assert!(!h.engine.operator_election().waiting.is_empty(), "the first waits for its time");
    let now = h.ts;
    finalised(&mut h, now);

    h.advance_to(logged + 1 + NOTICE_MS + 60_000);
    let due = h.engine.due_key(h.ts).map(|due| due.address);
    assert_eq!(due, Some(key(SECOND_SEED)), "the key a command now takes, as the API reads it");
    assert_eq!(signed_by(&mut h, SECOND_SEED), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_candidate(), Some(candidate(SECOND_SEED)));
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(h.engine.operator_bounds().rotation, None);
}

/// A catch-up mirrors three elections before the first takes effect: the chain seated the first and the
/// second, then elected a third, which it cancelled before its time. The second is the registry's operator
/// again, so it follows the first.
#[test]
fn cancelling_the_third_of_three_caught_up_elections_leaves_the_second_to_follow_the_first() {
    let mut h = rotating(ProtocolVersion::V88);
    let logged = h.ts;
    elect(&mut h, 1, FIRST_SEED, key(FIRST_SEED), logged - 2 * DAY_MS);
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), logged - DAY_MS);
    elect(&mut h, 3, THIRD_SEED, key(THIRD_SEED), logged + REGISTRY_NOTICE_MS);
    let now = h.ts;
    finalised(&mut h, now);
    let block = h.log.len() as u64;
    let events =
        h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref: "0xoff:0".into() });
    assert_eq!(events, vec![Event::RotationSwitched { enabled: false, election_cancelled: true }]);
    let last = h.engine.operator_election().last.expect("an election is recorded");
    assert_eq!((last.epoch, last.cancelled), (2, false), "the second is the last again");

    h.advance_to(logged + NOTICE_MS + 60_000);
    assert_eq!(signed_by(&mut h, SECOND_SEED), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_candidate(), Some(candidate(SECOND_SEED)));
    let rejected = Some(Event::AdminRejected { reason: "invalid signature".into() });
    assert_eq!(signed_by(&mut h, FIRST_SEED), rejected);
    assert_eq!(signed_by(&mut h, THIRD_SEED), rejected);
}

/// A catch-up mirrors three elections; the registry turns rotation off, cancelling the third, which leaves the
/// second waiting behind the first; and the timelock then installs an operator: the installation's key takes
/// over at its time, and neither election's does, though the first's time comes before it.
#[test]
fn an_installation_while_an_election_waits_takes_over_at_its_time() {
    let mut h = rotating(ProtocolVersion::V88);
    let logged = h.ts;
    caught_up_third_cancelled(&mut h, FIRST_SEED);
    h.advance_to(logged + HOUR_MS);
    let block = h.log.len() as u64;
    let installation = RegistryInstallation {
        candidate: candidate(THIRD_SEED),
        admin_key: key(THIRD_SEED),
        block,
        chain_ref: "0xinstall:0".into(),
    };
    let events = h.cmd(Command::InstallOperator(installation));
    assert!(matches!(events.last(), Some(Event::OperatorInstalled { .. })), "{events:?}");
    let installed_at = h.ts + NOTICE_MS;
    // The first election's time comes, and the next command finds it due.
    h.advance_to(logged + NOTICE_MS + 60_000);
    h.cmd(Command::Tick);
    let pending = h.engine.operator_bounds().rotation.map(|rotation| rotation.address);
    assert_eq!(pending, Some(key(THIRD_SEED)), "the installation's handover still waits");

    h.advance_to(installed_at + 60_000);
    assert_eq!(signed_by(&mut h, THIRD_SEED), Some(Event::AdminApplied));
    let rejected = Some(Event::AdminRejected { reason: "invalid signature".into() });
    assert_eq!(signed_by(&mut h, FIRST_SEED), rejected);
    assert_eq!(signed_by(&mut h, SECOND_SEED), rejected);
}

/// A catch-up mirrors three elections a day apart, each before the first takes effect: each takes over at
/// its own time, with its key and candidate, and names a successor until then.
#[test]
fn three_caught_up_elections_take_effect_in_turn() {
    let mut h = rotating(ProtocolVersion::V88);
    let logged = h.ts;
    elect(&mut h, 1, FIRST_SEED, key(FIRST_SEED), logged - 30 * DAY_MS);
    h.advance_to(logged + DAY_MS);
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), logged - 20 * DAY_MS);
    h.advance_to(logged + 2 * DAY_MS);
    elect(&mut h, 3, THIRD_SEED, key(THIRD_SEED), logged - 10 * DAY_MS);
    assert_eq!(h.engine.operator_election().waiting.len(), 2);
    assert!(!h.engine.awaited_election_keys().any(|awaited| awaited == key(FOURTH_SEED)), "a key no election names");
    let now = h.ts;
    finalised(&mut h, now);
    let seeds = [FIRST_SEED, SECOND_SEED, THIRD_SEED];
    // A successor: an election waiting, or the last behind them, or the pending rotation.
    let named = |h: &Harness| {
        let pending = h.engine.operator_bounds().rotation.map(|rotation| rotation.address);
        seeds.map(|seed| {
            h.engine.awaited_election_keys().any(|awaited| awaited == key(seed)) || pending == Some(key(seed))
        })
    };
    for (day, seed) in (0..).zip(seeds) {
        assert_eq!(named(&h), seeds.map(|later| later >= seed), "before day {day}");
        h.advance_to(logged + day * DAY_MS + NOTICE_MS + 60_000);
        assert_eq!(signed_by(&mut h, seed), Some(Event::AdminApplied), "day {day}");
        assert_eq!(h.engine.operator_candidate(), Some(candidate(seed)), "day {day}");
        assert_eq!(named(&h), seeds.map(|later| later > seed), "after day {day}");
    }
    assert!(h.engine.operator_election().waiting.is_empty());
}

/// Elections waiting in turn and a daily turn whose boundary has come hand over in one command, the
/// elections' handovers first as a pending rotation's are, so the turn's key signs it and no election's key
/// follows at the next.
#[test]
fn a_turn_due_with_caught_up_elections_keeps_its_key() {
    let mut h = rotating(ProtocolVersion::V88);
    let logged = h.ts;
    elect(&mut h, 1, FIRST_SEED, key(FIRST_SEED), logged - 2 * DAY_MS);
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), logged - DAY_MS);
    let member = SetMember { candidate: candidate(THIRD_SEED), admin_key: key(THIRD_SEED), weight: TokenUnits(1) };
    let (block, chain_ref) = (h.log.len() as u64, "0xset:1".to_owned());
    let effective_at = logged + REGISTRY_NOTICE_MS;
    let set =
        RegistryRotationSet { set: 1, epoch: 1, effective_at, quorum: true, members: vec![member], block, chain_ref };
    assert!(matches!(h.cmd(Command::AdoptRotationSet(set)).last(), Some(Event::RotationSetAdopted { set: 1, .. })));
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > effective_at).expect("a later turn");
    let (block, chain_ref) = (h.log.len() as u64, format!("0xturn:{turn}"));
    let drawn = RegistryTurn {
        turn,
        set: 1,
        position: 0,
        reason: TurnReason::Draw,
        candidate: candidate(THIRD_SEED),
        admin_key: key(THIRD_SEED),
        block,
        chain_ref,
    };
    assert!(matches!(h.cmd(Command::AssignTurn(drawn)).last(), Some(Event::TurnAssigned { .. })));
    let now = h.ts;
    finalised(&mut h, now);

    h.advance_to(boundary(turn).expect("in range") + 60_000);
    assert_eq!(signed_by(&mut h, THIRD_SEED), Some(Event::AdminApplied));
    let rejected = Some(Event::AdminRejected { reason: "invalid signature".into() });
    assert_eq!(signed_by(&mut h, SECOND_SEED), rejected);
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(signed_by(&mut h, THIRD_SEED), Some(Event::AdminApplied), "no election's key follows");
}

/// With rotation off, the operator's own rotation is refused while elections wait in turn, though the first
/// names the key in effect and so leaves no handover pending: a catch-up mirrors three elections, the first
/// under the key in effect, and the registry turns rotation off, cancelling the third.
#[test]
fn a_signed_rotation_is_refused_while_elections_wait_in_turn() {
    let mut h = rotating(ProtocolVersion::V88);
    caught_up_third_cancelled(&mut h, common::OPERATOR_SEED);
    assert_eq!(h.engine.operator_bounds().rotation, None, "the first names the key in effect");
    assert_eq!(h.engine.operator_election().waiting.len(), 1);
    let events = h.operate(AdminCommand::RotateOperator { address: key(THIRD_SEED) });
    let reason = "while an elected handover waits, only it moves the operator key";
    assert_eq!(events.last(), Some(&Event::AdminRejected { reason: reason.into() }), "{events:?}");
}

#[test]
fn before_v88_a_successor_mirrored_before_the_frontier_supersedes_its_predecessor() {
    let mut h = rotating(ProtocolVersion::V87);
    let first_at = first_election_waits(&mut h);
    h.advance_to(first_at + 60_000);
    elect(&mut h, 2, SECOND_SEED, key(SECOND_SEED), first_at + REGISTRY_NOTICE_MS);
    finalised(&mut h, first_at + 60_000);
    h.advance_to(first_at + DAY_MS);
    assert_eq!(signed_by(&mut h, FIRST_SEED), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED), Some(Event::AdminApplied));
    assert!(h.engine.operator_election().waiting.is_empty());
}

// ── Helpers ──

/// A catch-up at the current time mirrors three elections, the first of `first_seed`'s key and the second of
/// `SECOND_SEED`'s, both seated on chain long ago, and a third the registry has yet to seat; the registry then
/// turns rotation off, cancelling the third, so the second waits behind the first.
fn caught_up_third_cancelled(h: &mut Harness, first_seed: u8) {
    let logged = h.ts;
    elect(h, 1, first_seed, key(first_seed), logged - 2 * DAY_MS);
    elect(h, 2, SECOND_SEED, key(SECOND_SEED), logged - DAY_MS);
    elect(h, 3, FOURTH_SEED, key(FOURTH_SEED), logged + REGISTRY_NOTICE_MS);
    finalised(h, logged);
    let block = h.log.len() as u64;
    let events =
        h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref: "0xoff:0".into() });
    assert_eq!(events, vec![Event::RotationSwitched { enabled: false, election_cancelled: true }]);
    let last = h.engine.operator_election().last.expect("an election is recorded");
    assert_eq!((last.epoch, last.cancelled), (2, false), "the second is the last again");
}

/// A staking harness at `version`, with rotation on.
fn rotating(version: ProtocolVersion) -> Harness {
    let h = support::harness(version);
    assert_eq!(h.engine.protocol_version(), version);
    assert!(h.engine.operator_election().rotation_enabled);
    h
}

/// The first election is mirrored on time, and the mirror follows the chain to a second before its
/// effective time, which it returns.
fn first_election_waits(h: &mut Harness) -> Millis {
    let first_at = h.ts + REGISTRY_NOTICE_MS;
    elect(h, 1, FIRST_SEED, key(FIRST_SEED), first_at);
    finalised(h, h.ts);
    h.advance_to(first_at - 1_000);
    finalised(h, first_at - 1_000);
    first_at
}

/// Mirrors the registry's election, at `epoch`, of the candidate `seed` names under `admin_key`, taking
/// effect on chain at `effective_at`.
fn elect(h: &mut Harness, epoch: u64, seed: u8, admin_key: AccountId, effective_at: Millis) -> Vec<Event> {
    let block = h.log.len() as u64;
    let events = h.cmd(Command::ElectOperator(RegistryElection {
        epoch,
        candidate: candidate(seed),
        admin_key,
        effective_at,
        block,
        chain_ref: format!("0xelect:{epoch}"),
    }));
    assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
    events
}

/// The staking mirror completes a chunk whose last block is stamped `time` on chain.
fn finalised(h: &mut Harness, time: Millis) {
    let block = h.log.len() as u64;
    h.cmd(Command::StakingProgress {
        block,
        block_hash: alloy_primitives::B256::repeat_byte(9),
        block_time: Some(time),
        named_turn: None,
    });
}

/// What an admin command signed by `seed`'s key comes to.
fn signed_by(h: &mut Harness, seed: u8) -> Option<Event> {
    h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true }).pop()
}

fn key(seed: u8) -> AccountId {
    Client::new(seed).account
}

fn candidate(seed: u8) -> AccountId {
    AccountId([seed; 20])
}
