//! The session's `V50` part: the operator's share is split by time in office, so a handover halfway
//! through an epoch pays the outgoing and incoming operators half the fifth each.

use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryElection;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::UsdcUnits;
use engine_core::types::AccountId;

use crate::common::{Client, Harness};
use crate::v47::close;
use crate::v48::INSTALLED_SEED;

/// The seed of the candidate's admin key this part elects.
pub(crate) const ELECTED_SEED: u8 = 0x5a;
const CANDIDATE: AccountId = AccountId([0x5b; 20]);

/// `V50` activates with the installed operator of `V48`'s part in effect, and rotation turns on
/// again. An epoch the installed key serves whole pays its address the whole fifth. An election takes
/// effect halfway through a later epoch, whose close splits the fifth between the installed key's
/// address and the elected candidate by the time each held the office.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V50 });
    let (enabled, block, chain_ref) = (true, 50_000_000, "0xrotation:50:0".to_owned());
    h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    let installed = Client::new(INSTALLED_SEED).account;
    let (shares, excess, _) = close(h);
    assert_eq!(shares, vec![(installed, UsdcUnits(excess.0 / 5))]);

    let staking = h.engine.staking().expect("the session staked");
    let start = staking.config.epoch_end(staking.config.epochs_elapsed(h.ts + NOTICE_MS));
    let effective_at = start + staking.config.epoch_ms / 2;
    let epoch = h.engine.operator_election().last.expect("earlier parts elected").epoch + 1;
    let admin_key = Client::new(ELECTED_SEED).account;
    let events = h.cmd(Command::ElectOperator(RegistryElection {
        epoch,
        candidate: CANDIDATE,
        admin_key,
        effective_at,
        block: 50_000_010,
        chain_ref: "0xelect:50:0".into(),
    }));
    assert!(matches!(events.last(), Some(Event::OperatorElected { .. })), "{events:?}");
    // The close that opens the handover's epoch pays the installed key, in office throughout.
    h.advance_to(start - 1);
    let (shares, _, _) = close(h);
    assert_eq!(shares.iter().map(|share| share.0).collect::<Vec<_>>(), [installed]);
    assert_eq!(h.ts, start);

    h.advance_to(effective_at);
    let frontier = Command::StakingProgress {
        block: 50_000_020,
        block_hash: alloy_primitives::B256::repeat_byte(50),
        block_time: Some(effective_at),
    };
    h.cmd(frontier);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.operator().map(|operator| operator.address), Some(admin_key));
    let (shares, excess, allocated) = close(h);
    let half = excess.0 / 5 / 2;
    let mut expected = vec![(installed, UsdcUnits(half)), (CANDIDATE, UsdcUnits(half))];
    expected.sort();
    assert_eq!(shares, expected);
    assert!(allocated.0 <= excess.0, "{allocated:?} {excess:?}");
}
