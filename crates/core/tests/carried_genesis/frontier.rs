//! The carried frontier from `V92`: a carried genesis records the block of the staking frontier it carried, and a
//! staking, registry, schedule, vote or fault proof event mirrored from a block at or below it is refused, as is a
//! completed scan there, while one above it applies. Under `V91` the source's own stake, mirrored again below the
//! frontier, counted twice. A log carried before `V92` takes its completed frontier at the activation, a log that
//! carried nothing records none, and a replay of a log in which a mirror below the frontier took effect fails there.

use alloy_primitives::B256;
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::election::{RegistryElection, RegistryInstallation};
use engine_core::emissions::TokenUnits;
use engine_core::event::{Event, Reject};
use engine_core::governance::{GovernanceConfig, PassedProposal, command_hash};
use engine_core::operator_faults::ProvenFault;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{RegistryRotationSet, RegistryTurn, SetMember, TurnReason};
use engine_core::types::AccountId;

use crate::common::close_spread::{ceilings, install_expect};
use crate::common::{BTC, Client, HOUSE_CAPITAL, Harness};
use crate::fixture::{DAY_MS, STAKERS, carry, destination_at, source, stake};

/// The block of the source's completed staking frontier, past every block it mirrored.
const FRONTIER: u64 = 7;
/// The source's `Staked` event for its second staker, in block 2, and its reference.
const SECOND_STAKE: (u64, &str) = (2, "0xstake:2");

/// A new log whose genesis, under `version`, carried `source`'s state, and the tick that closes the carried
/// emission period.
fn carried_at(version: ProtocolVersion) -> Harness {
    let alpha = source();
    let (parts, _) = alpha.engine.carry_parts(version).expect("the source carries");
    let mut h = destination_at(&alpha, version);
    carry(&mut h, &parts);
    h.cmd(Command::Tick);
    let staking = h.engine.staking().expect("carried");
    assert_eq!(staking.completed_through.map(|completed| completed.block), Some(FRONTIER));
    h
}

/// The carried frontier the staking state records.
fn carried_through(h: &Harness) -> Option<u64> {
    h.engine.staking().expect("staking is initialised").carried_through
}

/// Mirrors the source's `Staked` event for its second staker again, under the reference the source credited it
/// under, which this log does not hold.
fn second_stake_again(h: &mut Harness) -> Vec<Event> {
    let (account, amount) = STAKERS[1];
    let (block, chain_ref) = SECOND_STAKE;
    h.cmd(Command::Stake { account, amount: TokenUnits(amount), block, chain_ref: chain_ref.into() })
}

/// A completed scan through `block` under `hash`, and whether the engine refused it.
fn scan_refused(h: &mut Harness, block: u64, hash: u8) -> bool {
    let block_hash = B256::repeat_byte(hash);
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: None, named_turn: None });
    events.iter().any(|event| matches!(event, Event::AdminRejected { .. }))
}

/// Each registry, schedule, vote and fault proof mirror from `block`, each under a reference naming it and the
/// block, in an order in which each is credited above the frontier: an installation while rotation is off, rotation
/// on, an election, a rotation set, a turn, a passed proposal and a proven fault.
fn registry_mirrors(h: &Harness, block: u64) -> Vec<(String, Command)> {
    let chain_ref = |kind: &str| format!("0x{kind}:{block}");
    let (candidate, operator) = (AccountId([0xc1; 20]), Client::operator().account);
    let installation =
        RegistryInstallation { candidate, admin_key: operator, block, chain_ref: chain_ref("installation") };
    let switch =
        Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref: chain_ref("rotation") };
    let election = RegistryElection {
        epoch: 1,
        candidate,
        admin_key: AccountId([0xe1; 20]),
        effective_at: h.ts,
        block,
        chain_ref: chain_ref("election"),
    };
    let member = SetMember { candidate, admin_key: operator, weight: TokenUnits(1) };
    let set = RegistryRotationSet {
        set: 1,
        epoch: 1,
        effective_at: h.ts,
        quorum: true,
        members: vec![member],
        block,
        chain_ref: chain_ref("set"),
    };
    let turn = RegistryTurn {
        turn: 1,
        set: 1,
        position: 0,
        reason: TurnReason::Draw,
        candidate,
        admin_key: operator,
        block,
        chain_ref: chain_ref("turn"),
    };
    let reduce_only = AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true };
    let proposal = PassedProposal {
        proposal: 1,
        fast: false,
        command_hash: command_hash(&reduce_only),
        command: Some(reduce_only),
        block,
        chain_ref: chain_ref("vote"),
    };
    let fault = ProvenFault {
        member: candidate,
        turn: 1,
        position: 0,
        signer_epoch: 1,
        seq: 1,
        block,
        chain_ref: chain_ref("fault"),
    };
    vec![
        (installation.chain_ref.clone(), Command::InstallOperator(installation)),
        (chain_ref("rotation"), switch),
        (election.chain_ref.clone(), Command::ElectOperator(election)),
        (set.chain_ref.clone(), Command::AdoptRotationSet(set)),
        (turn.chain_ref.clone(), Command::AssignTurn(turn)),
        (proposal.chain_ref.clone(), Command::PassProposal(Box::new(proposal))),
        (fault.chain_ref.clone(), Command::ProveFault(fault)),
    ]
}

#[test]
fn the_sources_stake_mirrored_again_below_the_frontier_counts_twice_under_v91_and_is_refused_from_v92() {
    let (account, amount) = STAKERS[1];
    let mut before = carried_at(ProtocolVersion::V91);
    assert_eq!(carried_through(&before), None, "V91 records no carried frontier");
    assert_eq!(second_stake_again(&mut before), [Event::Staked { account, amount: TokenUnits(amount) }]);
    assert_eq!(before.engine.staked(account), TokenUnits(2 * amount), "the carried stake counted twice");
    assert!(!scan_refused(&mut before, FRONTIER, 7), "V91 takes the carried scan again");

    let mut h = carried_at(ProtocolVersion::V92);
    assert_eq!(carried_through(&h), Some(FRONTIER));
    let total = h.engine.staking().and_then(|staking| staking.staked_total);
    let events = second_stake_again(&mut h);
    let (_, chain_ref) = SECOND_STAKE;
    let refused = Event::StakingRejected {
        account,
        amount: TokenUnits(amount),
        chain_ref: chain_ref.into(),
        reason: Reject::CarriedFrontier { through: FRONTIER },
    };
    assert_eq!(events, [refused]);
    assert_eq!(h.engine.staked(account), TokenUnits(amount));
    let staking = h.engine.staking().expect("carried");
    assert_eq!((staking.staked_total, staking.mirrored_through), (total, Some(6)));
    assert!(!h.engine.is_staking_credited(chain_ref), "a refused mirror credits nothing");
}

#[test]
fn every_mirror_at_or_below_the_carried_frontier_is_refused_and_one_above_it_applies() {
    let mut h = carried_at(ProtocolVersion::V92);
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: AccountId([0x70; 20]), restriction_ttl_ms: DAY_MS }));
    let (staker, staked) = (STAKERS[2].0, h.engine.staked(STAKERS[2].0));
    let newcomer = AccountId([0xb7; 20]);
    for block in [0, FRONTIER - 1, FRONTIER] {
        let stake = stake(&mut h, newcomer, 5, block);
        let reason = Reject::CarriedFrontier { through: FRONTIER };
        assert!(matches!(&stake[..], [Event::StakingRejected { reason: r, .. }] if *r == reason), "{stake:?}");
        let unstake = Command::Unstake { account: staker, amount: staked, block, chain_ref: format!("0xout:{block}") };
        let unstake = h.cmd(unstake);
        assert!(matches!(&unstake[..], [Event::StakingRejected { reason: r, .. }] if *r == reason), "{unstake:?}");
        for (chain_ref, mirror) in registry_mirrors(&h, block) {
            let events = h.cmd(mirror);
            let frontier = format!("block {block} is at or below the carried staking frontier at block {FRONTIER}");
            let named = matches!(&events[..], [Event::ElectionRejected { reason, .. }] if *reason == frontier);
            assert!(named, "{chain_ref}: {events:?}");
            assert!(!h.engine.is_staking_credited(&chain_ref), "{chain_ref}");
        }
    }
    // The carried scan's own block and hash, which `V91` takes again.
    assert!(scan_refused(&mut h, FRONTIER, 7));
    assert_eq!((h.engine.staked(newcomer), h.engine.staked(staker)), (TokenUnits(0), staked));

    let above = FRONTIER + 1;
    assert_eq!(stake(&mut h, newcomer, 5, above), [Event::Staked { account: newcomer, amount: TokenUnits(5) }]);
    let unstake = Command::Unstake { account: staker, amount: staked, block: above, chain_ref: "0xout:8".into() };
    assert_eq!(h.cmd(unstake), [Event::Unstaked { account: staker, amount: staked }]);
    for (chain_ref, mirror) in registry_mirrors(&h, above) {
        let events = h.cmd(mirror);
        assert!(!events.iter().any(|event| matches!(event, Event::ElectionRejected { .. })), "{chain_ref}: {events:?}");
        assert!(h.engine.is_staking_credited(&chain_ref), "{chain_ref}");
    }
    assert!(!scan_refused(&mut h, above, 8));
    assert_eq!(h.engine.staking().and_then(|staking| staking.mirrored_through), Some(above));
}

#[test]
fn a_log_carried_before_v92_takes_its_completed_frontier_at_the_activation() {
    let mut h = carried_at(ProtocolVersion::V91);
    let newcomer = AccountId([0xb8; 20]);
    assert!(matches!(stake(&mut h, newcomer, 5, 9)[..], [Event::Staked { .. }]));
    assert!(!scan_refused(&mut h, 10, 10));
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V92 });
    assert_eq!(carried_through(&h), Some(10), "the completed frontier at the activation");
    for block in [FRONTIER + 1, 10] {
        let events = stake(&mut h, newcomer, 5, block);
        let reason = Reject::CarriedFrontier { through: 10 };
        assert!(matches!(&events[..], [Event::StakingRejected { reason: r, .. }] if *r == reason), "{events:?}");
    }
    assert!(matches!(stake(&mut h, newcomer, 5, 11)[..], [Event::Staked { .. }]));
    assert_eq!(h.engine.staked(newcomer), TokenUnits(10));
}

#[test]
fn a_log_that_carried_nothing_records_no_frontier_and_mirrors_as_before() {
    let mut h = Harness::staking(engine_core::Fixed::from_int(HOUSE_CAPITAL), DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V92 });
    let account = AccountId([0xa9; 20]);
    assert!(matches!(stake(&mut h, account, 5, 5)[..], [Event::Staked { .. }]));
    assert!(!scan_refused(&mut h, 6, 6));
    assert_eq!(carried_through(&h), None);
    // A mirror below the completed frontier applies under the rules this log has always had.
    assert!(matches!(stake(&mut h, account, 5, 3)[..], [Event::Staked { .. }]));
    assert_eq!(h.engine.staked(account), TokenUnits(10));
}

#[test]
fn a_replay_of_a_log_whose_mirror_below_the_frontier_took_effect_fails_at_that_command() {
    let mut h = carried_at(ProtocolVersion::V92);
    assert!(matches!(stake(&mut h, AccountId([0xb9; 20]), 5, 12)[..], [Event::Staked { .. }]));
    // Above the frontier and below the mirror's reach, the source's stake applies again as a node that took it
    // below the frontier would have applied it: the same credit, the same reference, and the frontier unmoved.
    let (account, amount) = STAKERS[1];
    let (_, chain_ref) = SECOND_STAKE;
    let honest = Command::Stake { account, amount: TokenUnits(amount), block: 9, chain_ref: chain_ref.into() };
    assert!(matches!(h.cmd(honest)[..], [Event::Staked { .. }]));

    let mut engine = Engine::new();
    let roots: Vec<[u8; 32]> = h
        .log
        .iter()
        .map(|sequenced| {
            engine.apply(sequenced).expect("the log replays");
            engine.state_root()
        })
        .collect();
    let at = h.log.len() - 1;
    let mut forged = h.log.clone();
    let Command::Stake { block, .. } = &mut forged[at].cmd else { unreachable!("the stake is last") };
    *block = SECOND_STAKE.0;

    let mut replay = Engine::new();
    let diverged = forged.iter().zip(&roots).position(|(sequenced, root)| {
        replay.apply(sequenced).expect("the forged log applies");
        replay.state_root() != *root
    });
    assert_eq!(diverged, Some(at), "the replay fails at the forged mirror and no earlier");
}
