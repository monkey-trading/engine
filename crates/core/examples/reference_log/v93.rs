//! The session's `V93` part: a carried genesis carries no stake. A vote activates the version, a stake part is refused
//! whatever its place, and the session freezes again as a carry's source must.
//!
//! `carried` then writes the log a carried genesis opens from the session's last command under `V93`. Its genesis
//! carries every entitlement and award the session's root proves and the source's frontier and total staked, but no
//! stake. The source's staking contract events then mirror again, in the source's order, while a registry event in
//! those blocks is refused; the source's own frontier, reached with its total, ends the re-mirror with the source's
//! stake set, and records it as the carried frontier, at which a later stake and the registry event are refused;
//! and the log goes on as `V90`'s carried log does.

use alloy_primitives::B256;
use engine_core::Engine;
use engine_core::carry::CarryPart;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::emissions::TokenUnits;
use engine_core::event::{Event, Reject};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::{Remirror, StakingCheckpoint};
use engine_core::types::AccountId;

use crate::common::Harness;
use crate::v81::{applied, scheduled};
use crate::v90::{after_genesis, carried_genesis};

/// This part's proposal id.
const PROPOSAL: u64 = 303;
/// An account that stakes, too late, at the carried frontier.
const LATE: AccountId = AccountId([0x93; 20]);

/// Activates `V93` by vote, refuses a stake part, and freezes the session again.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V93 };
    scheduled(h, PROPOSAL, &activation);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V93);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let events = h.cmd(Command::Admin(AdminCommand::CarryState(CarryPart::Stakes { entries: Vec::new() })));
    let refused =
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("carries no stakes"));
    assert!(refused, "{events:?}");
    freeze(h);
}

/// Completes the staking mirror's frontier past the vote this part mirrored, and closes the open emission period.
fn freeze(alpha: &mut Harness) {
    let staking = alpha.engine.staking().expect("the session staked");
    let completed = staking.completed_through.map_or(0, |completed| completed.block);
    let block = staking.mirrored_through.unwrap_or(0).max(completed) + 1;
    let progress =
        Command::StakingProgress { block, block_hash: B256::repeat_byte(0x93), block_time: None, named_turn: None };
    let events = alpha.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    let emissions = alpha.engine.emissions().expect("the session emits");
    alpha.advance_to(emissions.schedule.period_end(emissions.period));
    alpha.cmd(Command::Tick);
}

/// The log whose genesis carries `alpha`'s state at its last command under `V93`, as described above.
pub(crate) fn carried(alpha: &mut Harness) -> Vec<Sequenced> {
    // Its genesis applies every part, so none carries a stake, which `V93` refuses.
    let mut h = carried_genesis(alpha, ProtocolVersion::V93);
    let theirs = alpha.engine.staking().expect("the session staked");
    let through = theirs.completed_through.expect("the frozen session completed its scan");
    let staked_total = theirs.staked_total.expect("V90 keeps a total");
    assert!(h.engine.staked_balances().is_empty(), "no stake is carried");
    assert_eq!(h.engine.staking().and_then(|staking| staking.remirror), Some(Remirror { through, staked_total }));

    remirror(&mut h, alpha, through);
    after_genesis(&mut h, alpha);
    h.log
}

/// Mirrors the source's staking contract events again, in the source's order, refuses a registry event in their
/// blocks, and reaches the source's frontier with its stake set.
fn remirror(h: &mut Harness, alpha: &Harness, through: StakingCheckpoint) {
    for cmd in mirrored_stakes(&alpha.log) {
        let events = h.cmd(cmd);
        assert!(matches!(events[..], [Event::Staked { .. }] | [Event::Unstaked { .. }]), "{events:?}");
    }
    let switch = Command::SwitchRotation {
        enabled: true,
        election_cancelled: false,
        block: through.block,
        chain_ref: "0xv93".into(),
    };
    let events = h.cmd(switch);
    let refused = matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("again"));
    assert!(refused, "{events:?}");
    assert!(h.engine.remirroring(), "the frontier is not yet the source's");
    let progress = Command::StakingProgress {
        block: through.block,
        block_hash: through.block_hash,
        block_time: through.time,
        named_turn: None,
    };
    let events = h.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    assert!(!h.engine.remirroring());
    assert_eq!(h.engine.staked_balances(), alpha.engine.staked_balances());
    let staking = h.engine.staking().expect("the genesis stakes");
    assert_eq!(staking.staked_total, alpha.engine.staking().and_then(|staking| staking.staked_total));
    assert_eq!(staking.completed_through, Some(through));
    assert_eq!(staking.carried_through, Some(through.block), "the source's frontier is the carried one");
    assert_eq!(h.engine.staked_balances().values().map(|staked| staked.0).sum::<u128>(), sum(h));

    // From here `V92`'s rule holds: a stake under a reference the re-mirror did not hold, and the registry event
    // refused above, are refused at the carried frontier.
    let late =
        Command::Stake { account: LATE, amount: TokenUnits(1), block: through.block, chain_ref: "0xv93:late".into() };
    let events = h.cmd(late);
    let refused = Reject::CarriedFrontier { through: through.block };
    assert!(matches!(&events[..], [Event::StakingRejected { reason, .. }] if *reason == refused), "{events:?}");
    let switch = Command::SwitchRotation {
        enabled: true,
        election_cancelled: false,
        block: through.block,
        chain_ref: "0xv93:late".into(),
    };
    let events = h.cmd(switch);
    let refused = matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("carried"));
    assert!(refused, "{events:?}");
}

/// The stake and unstake mirrors that applied in `log`, in its order: the staking contract's events, as the chain
/// holds them.
fn mirrored_stakes(log: &[Sequenced]) -> Vec<Command> {
    let mut engine = Engine::new();
    log.iter()
        .filter_map(|sequenced| {
            let events = engine.apply(sequenced).expect("the session replays");
            let applied = events.iter().any(|event| matches!(event, Event::Staked { .. } | Event::Unstaked { .. }));
            let stake = matches!(sequenced.cmd, Command::Stake { .. } | Command::Unstake { .. });
            (stake && applied).then(|| sequenced.cmd.clone())
        })
        .collect()
}

/// The staking state's total staked.
fn sum(h: &Harness) -> u128 {
    h.engine.staking().and_then(|staking| staking.staked_total).unwrap_or(TokenUnits(0)).0
}
