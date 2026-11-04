//! The session's `V48` part: the timelock's `OperatorInstalled` event mirrors while rotation is
//! off, and the installed key signs from seven days after the installation is logged, waiting no
//! finalised frontier.

use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryInstallation;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::{BTC, Client, Harness};
use crate::v37::ELECTED_SEED;

/// The seed of the installed operator's admin key.
pub(crate) const INSTALLED_SEED: u8 = 0x57;
const CANDIDATE: AccountId = AccountId([0x58; 20]);

/// `V48` activates over rotation left on by `V47`'s part. An installation is refused while
/// rotation is on; once it is off, the timelock installs a new operator, whose key signs seven
/// days later without waiting for the staking mirror's finalised frontier, and a rotation the key
/// in effect signs is refused while the installed handover waits.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V48 });
    let installed = Client::new(INSTALLED_SEED).account;
    let refused = h.cmd(Command::InstallOperator(RegistryInstallation {
        candidate: CANDIDATE,
        admin_key: installed,
        block: 48_000_010,
        chain_ref: "0xinstall:48:0".into(),
    }));
    assert!(matches!(refused.last(), Some(Event::ElectionRejected { .. })), "{refused:?}");
    let off = Command::SwitchRotation {
        enabled: false,
        election_cancelled: false,
        block: 48_000_020,
        chain_ref: "0xrotation:48:1".into(),
    };
    h.cmd(off);

    let effective_at = h.ts + NOTICE_MS;
    let events = h.cmd(Command::InstallOperator(RegistryInstallation {
        candidate: CANDIDATE,
        admin_key: installed,
        block: 48_000_030,
        chain_ref: "0xinstall:48:1".into(),
    }));
    assert!(
        matches!(events.last(), Some(Event::OperatorInstalled { admin_key, effective_at: at, .. }) if *admin_key == installed && *at == effective_at),
        "{events:?}"
    );
    // Until its time the key in effect signs; a rotation it signs cannot replace the installed
    // handover; the installed key waits no finalised frontier.
    let rotation = AdminCommand::RotateOperator { address: Client::new(0x59).account };
    let rejected = h.operate_as(ELECTED_SEED, rotation);
    assert!(
        matches!(rejected.last(), Some(Event::AdminRejected { reason }) if reason == "while an installed handover waits, only it moves the operator key"),
        "{rejected:?}"
    );
    h.advance_to(effective_at);
    let reduce_only = AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true };
    assert_eq!(
        h.operate_as(ELECTED_SEED, reduce_only.clone()).last(),
        Some(&Event::AdminRejected { reason: "invalid signature".into() })
    );
    assert_eq!(h.operate_as(INSTALLED_SEED, reduce_only).last(), Some(&Event::AdminApplied));
}
