//! The session's `V92` part: the carried frontier. A vote activates the version, which records no carried frontier
//! in a log whose genesis carried nothing, so a stake mirrored from a block below its completed frontier still
//! applies; the staking mirror's frontier then passes it again, so the session stays a carry's source.
//!
//! `carried`, which wrote `v92-carried.jsonl` and now runs as a test, opens the log a carried genesis opens under
//! `V92` from the session's last command (`v90`'s genesis): the genesis records the carried staking frontier's
//! block, and the session's stake mirrored again, an unstake, a registry mirror and a completed scan at or below it
//! are refused; then `v90`'s continuation mirrors above it.

use alloy_primitives::B256;
#[cfg(test)]
use engine_core::command::Sequenced;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
#[cfg(test)]
use engine_core::event::Reject;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::Harness;
use crate::v81::{applied, scheduled};

/// This part's proposal id.
const PROPOSAL: u64 = 302;
/// The staker this part mirrors.
const STAKER: AccountId = AccountId([0x92; 20]);
/// The reference of the stake this part mirrors.
const STAKE_REF: &str = "0xv92:stake";

/// Activates `V92` by vote; then the stake described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V92 };
    scheduled(h, PROPOSAL, &activation);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V92);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let staking = h.engine.staking().expect("the session staked");
    assert_eq!(staking.carried_through, None, "the session's genesis carried nothing");
    let completed = staking.completed_through.expect("the session completed a scan").block;
    let mirrored = staking.mirrored_through.expect("the session mirrored");
    let stake = Command::Stake {
        account: STAKER,
        amount: TokenUnits(9_200),
        block: completed - 1,
        chain_ref: STAKE_REF.into(),
    };
    assert_eq!(h.cmd(stake), [Event::Staked { account: STAKER, amount: TokenUnits(9_200) }]);
    let block = mirrored.max(completed) + 1;
    let progress =
        Command::StakingProgress { block, block_hash: B256::repeat_byte(0x92), block_time: None, named_turn: None };
    let events = h.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
}

/// The log whose genesis carries `alpha`'s state at its last command under `V92`, as described above.
#[cfg(test)]
pub(crate) fn carried(alpha: &mut Harness) -> Vec<Sequenced> {
    let mut h = crate::v90::carried_genesis(alpha, ProtocolVersion::V92);
    let staking = h.engine.staking().expect("carried");
    let frontier = staking.completed_through.expect("carried").block;
    assert_eq!(staking.carried_through, Some(frontier));
    let refused = Reject::CarriedFrontier { through: frontier };

    // The session's stake, mirrored again as the source mirrored it, under a reference this log does not hold.
    let again = alpha.log.iter().find_map(|sequenced| match &sequenced.cmd {
        Command::Stake { chain_ref, .. } if chain_ref == STAKE_REF => Some(sequenced.cmd.clone()),
        _ => None,
    });
    let events = h.cmd(again.expect("the session mirrored its stake"));
    assert!(matches!(&events[..], [Event::StakingRejected { reason, .. }] if *reason == refused), "{events:?}");
    assert_eq!(h.engine.staked(STAKER), TokenUnits(9_200), "counted once");
    let (&staker, &staked) = h.engine.staked_balances().iter().next().expect("carried stakes");
    let unstake =
        Command::Unstake { account: staker, amount: staked, block: frontier - 1, chain_ref: "0xv92:out".into() };
    let events = h.cmd(unstake);
    assert!(matches!(&events[..], [Event::StakingRejected { reason, .. }] if *reason == refused), "{events:?}");
    let switch = Command::SwitchRotation {
        enabled: true,
        election_cancelled: false,
        block: frontier,
        chain_ref: "0xv92:on".into(),
    };
    let events = h.cmd(switch);
    assert!(
        matches!(&events[..], [Event::ElectionRejected { reason, .. }] if reason.contains("carried")),
        "{events:?}"
    );
    // The carried scan itself, which `V90` took again.
    let block_hash = staking_hash(&h);
    let progress = Command::StakingProgress { block: frontier, block_hash, block_time: None, named_turn: None };
    let events = h.cmd(progress);
    assert!(matches!(events[..], [Event::AdminRejected { .. }]), "{events:?}");
    assert_eq!(h.engine.staked(staker), staked);

    crate::v90::after_genesis(&mut h, alpha);
    h.log
}

/// The block hash of the staking mirror's completed frontier.
#[cfg(test)]
fn staking_hash(h: &Harness) -> B256 {
    h.engine.staking().and_then(|staking| staking.completed_through).expect("a completed frontier").block_hash
}
