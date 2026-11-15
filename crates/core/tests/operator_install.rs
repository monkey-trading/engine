//! The timelock-only operator installation from `V48`, through `Engine::apply`: the registry's
//! `OperatorInstalled` event mirrored once under its chain reference, the installed key taking
//! effect seven days after it is logged and waiting no finalised frontier, a pending rotation
//! replaced, and `RotateOperator` refused while an installed handover waits.

mod common;

use common::close_spread::{ceilings, install_expect};
use common::{BTC, Client, Harness, operator};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::election::{InstalledOperator, RegistryInstallation};
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, PendingRotation};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};

/// The seed of the installed operator's admin key.
const INSTALLED_SEED: u8 = 0x77;
const CANDIDATE: AccountId = AccountId([0xc8; 20]);
const DAY_MS: Millis = 24 * 60 * 60 * 1000;

fn installed() -> AccountId {
    Client::new(INSTALLED_SEED).account
}

/// A harness at `V48` with rotation off, as the registry requires for an installation.
fn v48() -> Harness {
    let mut h = Harness::staking(Fixed::from_int(1_000_000), DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V48 });
    h
}

/// Mirrors the registry's installation of `admin_key` under `chain_ref`, returning its events.
fn install(h: &mut Harness, admin_key: AccountId, chain_ref: &str) -> Vec<Event> {
    let block = h.log.len() as u64;
    h.cmd(Command::InstallOperator(RegistryInstallation {
        candidate: CANDIDATE,
        admin_key,
        block,
        chain_ref: chain_ref.into(),
    }))
}

fn refused(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::ElectionRejected { reason: r, .. }) if r == reason), "{events:?}");
}

/// The outcome of a reduce-only change in BTC signed by the key from `seed`.
fn signed_by(h: &mut Harness, seed: u8, reduce_only: bool) -> Option<Event> {
    h.operate_as(seed, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only }).pop()
}

#[test]
fn installations_need_v48() {
    let mut h = Harness::v37();
    refused(&install(&mut h, installed(), "0xinstall:0"), "an operator installation needs protocol V48");
    assert_eq!(h.engine.operator_election().installed, None);
    assert!(!h.engine.is_staking_credited("0xinstall:0"));
}

#[test]
fn the_timelock_installs_an_operator_while_rotation_is_off() {
    let mut h = v48();
    let effective_at = h.ts + NOTICE_MS;
    let events = install(&mut h, installed(), "0xinstall:0");
    assert_eq!(
        events.last(),
        Some(&Event::OperatorInstalled { candidate: CANDIDATE, admin_key: installed(), effective_at })
    );
    assert_eq!(
        h.engine.operator_election().installed,
        Some(InstalledOperator { candidate: CANDIDATE, admin_key: installed(), effective_at })
    );
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: installed(), effective_at }));
    assert!(h.engine.is_staking_credited("0xinstall:0"));
    assert_eq!(h.engine.staking().unwrap().mirrored_through, Some(h.log.len() as u64 - 1));
    // The mirror credits its reference once.
    refused(&install(&mut h, installed(), "0xinstall:0"), "chain reference 0xinstall:0 is already mirrored");
    // Until its time the old key signs; the installed one does not, and no rotation the old key
    // signs can replace the installed handover.
    h.advance_to(effective_at - 1);
    assert_eq!(
        signed_by(&mut h, INSTALLED_SEED, true),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_eq!(
        h.operate(AdminCommand::RotateOperator { address: Client::new(0x78).account }).pop(),
        Some(Event::AdminRejected {
            reason: "while an installed handover waits, only it moves the operator key".into()
        })
    );
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: installed(), effective_at }));
    // The installed key takes effect at its time with no finalised frontier to wait for.
    h.advance_to(effective_at);
    assert_eq!(
        signed_by(&mut h, common::OPERATOR_SEED, false),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(signed_by(&mut h, INSTALLED_SEED, false), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator().map(|o| o.address), Some(installed()));
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator_bounds().rotations, 1);
}

#[test]
fn an_installation_while_rotation_is_on_or_naming_nothing_is_refused() {
    let mut h = v48();
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref: "0xon:0".into() });
    refused(&install(&mut h, installed(), "0xinstall:0"), "an installation while rotation is on");
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: false, block, chain_ref: "0xoff:0".into() });
    refused(&install(&mut h, AccountId([0; 20]), "0xinstall:0"), "an installation names a nonzero candidate and key");
    assert_eq!(h.engine.operator_election().installed, None);
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert!(!h.engine.is_staking_credited("0xinstall:0"));
}

#[test]
fn an_installation_naming_the_key_in_effect_records_no_rotation() {
    let mut h = v48();
    let events = install(&mut h, operator().address, "0xinstall:0");
    let effective_at = h.ts + NOTICE_MS;
    assert_eq!(
        events.last(),
        Some(&Event::OperatorInstalled { candidate: CANDIDATE, admin_key: operator().address, effective_at })
    );
    assert_eq!(h.engine.operator_bounds().rotation, None);
    h.advance_to(effective_at);
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator_bounds().rotations, 0);
}

#[test]
fn a_later_installation_replaces_the_pending_handover() {
    let mut h = v48();
    install(&mut h, installed(), "0xinstall:0");
    let other = Client::new(0x78).account;
    let effective_at = h.ts + NOTICE_MS;
    let events = install(&mut h, other, "0xinstall:1");
    assert_eq!(events.last(), Some(&Event::OperatorInstalled { candidate: CANDIDATE, admin_key: other, effective_at }));
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: other, effective_at }));
    // The first installation no longer gates a signed rotation: only the last one does.
    assert_eq!(
        h.operate(AdminCommand::RotateOperator { address: installed() }).pop(),
        Some(Event::AdminRejected {
            reason: "while an installed handover waits, only it moves the operator key".into()
        })
    );
}

#[test]
fn an_installation_supersedes_a_signed_rotation() {
    let mut h = v48();
    let signed = Client::new(0x78).account;
    h.operator_admin(AdminCommand::RotateOperator { address: signed });
    let effective_at = h.ts + NOTICE_MS;
    install(&mut h, installed(), "0xinstall:0");
    assert_eq!(h.engine.operator_bounds().rotation, Some(PendingRotation { address: installed(), effective_at }));
}

#[test]
fn turning_rotation_on_drops_an_installed_handover_still_pending() {
    let mut h = v48();
    install(&mut h, installed(), "0xinstall:0");
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref: "0xon:0".into() });
    assert_eq!(h.engine.operator_bounds().rotation, None);
    // The dropped handover never applies: while rotation is on, only an election moves the key.
    h.advance(2 * NOTICE_MS);
    assert_eq!(
        signed_by(&mut h, INSTALLED_SEED, true),
        Some(Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
}

#[test]
fn an_election_after_an_installation_applies_normally() {
    let mut h = v48();
    install(&mut h, installed(), "0xinstall:0");
    // The registry installed at once; the mirror delivers the installation, then rotation turns
    // on and an election names another key before the installed handover's time.
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref: "0xon:0".into() });
    let elected = Client::new(0x79).account;
    let election_at = h.ts + NOTICE_MS;
    h.cmd(Command::ElectOperator(engine_core::election::RegistryElection {
        epoch: 1,
        candidate: AccountId([0xc9; 20]),
        admin_key: elected,
        effective_at: election_at,
        block: h.log.len() as u64,
        chain_ref: "0xelect:0".into(),
    }));
    // The election's handover replaced the dropped one and moves the key at its time.
    h.advance_to(election_at);
    let block = h.log.len() as u64;
    h.cmd(Command::StakingProgress {
        block,
        block_hash: alloy_primitives::B256::repeat_byte(9),
        block_time: Some(election_at),
        named_turn: None,
    });
    assert_eq!(signed_by(&mut h, 0x79, false), Some(Event::AdminApplied));
    assert_eq!(h.engine.operator().map(|o| o.address), Some(elected));
}

/// `v48` with rotation on, an election waiting out its engine notice though already in effect on
/// chain: the registry time is a day out, so the engine hands over seven days after the mirror.
fn deferred_election() -> (Harness, AccountId, Millis) {
    let mut h = v48();
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref: "0xon:0".into() });
    let elected = Client::new(0x79).account;
    let registry_at = h.ts + DAY_MS;
    h.cmd(Command::ElectOperator(engine_core::election::RegistryElection {
        epoch: 1,
        candidate: AccountId([0xc9; 20]),
        admin_key: elected,
        effective_at: registry_at,
        block: h.log.len() as u64,
        chain_ref: "0xelect:0".into(),
    }));
    // On chain the election has taken effect; the engine notice still runs.
    h.advance_to(registry_at);
    let block = h.log.len() as u64;
    h.cmd(Command::StakingProgress {
        block,
        block_hash: alloy_primitives::B256::repeat_byte(9),
        block_time: Some(registry_at),
        named_turn: None,
    });
    (h, elected, registry_at)
}

#[test]
fn an_installation_supersedes_the_elections_a_cancellation_could_restore() {
    let (mut h, elected, _) = deferred_election();
    // The catch-up the registry ran: rotation off cancels nothing (the election took effect on
    // chain), the timelock installs a fallback, rotation turns on, a promotion is made and then
    // cancelled before its time.
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: false, block, chain_ref: "0xoff:0".into() });
    install(&mut h, installed(), "0xinstall:0");
    assert_eq!(h.engine.operator_election().last.map(|last| last.cancelled), Some(true));
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref: "0xon:1".into() });
    let promoted = Client::new(0x7a).account;
    let promoted_at = h.ts + DAY_MS;
    h.cmd(Command::ElectOperator(engine_core::election::RegistryElection {
        epoch: 2,
        candidate: AccountId([0xca; 20]),
        admin_key: promoted,
        effective_at: promoted_at,
        block: h.log.len() as u64,
        chain_ref: "0xelect:1".into(),
    }));
    let block = h.log.len() as u64;
    let events =
        h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: true, block, chain_ref: "0xoff:1".into() });
    assert_eq!(events.last(), Some(&Event::RotationSwitched { enabled: false, election_cancelled: true }));
    // The cancelled promotion restores nothing: the superseded election's handover is gone, and
    // the registry names the installed operator, not the election it replaced.
    assert_eq!(h.engine.operator_bounds().rotation, None);
    h.advance(2 * NOTICE_MS);
    assert_eq!(signed_by(&mut h, 0x79, true), Some(Event::AdminRejected { reason: "invalid signature".into() }));
    assert_eq!(signed_by(&mut h, common::OPERATOR_SEED, true), Some(Event::AdminApplied));
    assert_ne!(h.engine.operator().map(|operator| operator.address), Some(elected));
}

#[test]
fn an_installation_seats_no_candidate_even_under_the_key_in_effect() {
    let (mut h, elected, registry_at) = deferred_election();
    // The deferred election seats its candidate under the key in effect once its time passes.
    h.advance_to(h.ts + NOTICE_MS);
    let block = h.log.len() as u64;
    h.cmd(Command::StakingProgress {
        block,
        block_hash: alloy_primitives::B256::repeat_byte(9),
        block_time: Some(registry_at),
        named_turn: None,
    });
    assert_eq!(h.engine.operator().map(|o| o.address), Some(elected));
    assert_eq!(h.engine.operator_candidate(), Some(AccountId([0xc9; 20])));
    // Rotation off, then the timelock installs a new candidate under the very key in effect: no
    // handover is recorded, but the superseded candidate keeps no claim on the operator's share.
    let block = h.log.len() as u64;
    h.cmd(Command::SwitchRotation { enabled: false, election_cancelled: false, block, chain_ref: "0xoff:0".into() });
    install(&mut h, elected, "0xinstall:0");
    assert_eq!(h.engine.operator_bounds().rotation, None);
    assert_eq!(h.engine.operator_candidate(), None);
    h.advance(2 * NOTICE_MS);
    assert_eq!(h.engine.operator_candidate(), None);
    assert_eq!(signed_by(&mut h, 0x79, true), Some(Event::AdminApplied));
}
