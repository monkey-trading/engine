//! The session's `V90` part: a carried genesis. A vote activates the version, which sets the staking state's total
//! staked from the balances; a mirrored stake and unstake keep it, and a carry part outside a genesis is refused.
//! The session's log then freezes as a carry's source must: the registry mirror switches rotation off, the staking
//! mirror's frontier passes every block it mirrored, and the open emission period closes.
//!
//! `carried` then writes the log a carried genesis opens from the session's last command. A new log's genesis
//! activates `V90`, sets its parameters and markets, initialises staking and
//! carries every entitlement, stake and award the session's root proves, in parts; the next command closes the
//! carried period with no weight and cancels the periods since. A carried staker unstakes and a new stake mirrors
//! past the carried frontier; the first epoch close pays the carried award from its whole amount before the
//! stakers; and a carried trader's loss adds to the entitlement it carried.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::carry::{CarryPart, StateCarried};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::StakingConfig;
use engine_core::types::{AccountId, Side};

use crate::common::close_spread::{attest, ceilings, fund, source_key, trade};
use crate::common::events::only_fill;
use crate::common::genesis::node_genesis;
use crate::common::{BTC, Client, HOUSE_CAPITAL, Harness};
use crate::v81::{applied, scheduled};

/// The seed of the key the `V89` part traded through, whose loss earned it an entitlement the carry takes.
const CLIENT_SEED: u8 = 0x89;
/// This part's proposal id.
const PROPOSAL: u64 = 300;
/// The staker this part mirrors.
const STAKER: AccountId = AccountId([0x90; 20]);
/// A day, in milliseconds.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Activates `V90` by vote; then the total staked described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V90 };
    scheduled(h, PROPOSAL, &activation);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V90);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);

    let sum: u128 = h.engine.staked_balances().values().map(|staked| staked.0).sum();
    assert!(sum > 0, "the session staked");
    assert_eq!(total(h), sum);
    let block = 90_000_000 + h.log.len() as u64;
    let stake = Command::Stake { account: STAKER, amount: TokenUnits(9_000), block, chain_ref: "0xv90:stake".into() };
    assert_eq!(h.cmd(stake), [Event::Staked { account: STAKER, amount: TokenUnits(9_000) }]);
    let unstake = Command::Unstake {
        account: STAKER,
        amount: TokenUnits(4_000),
        block: block + 1,
        chain_ref: "0xv90:unstake".into(),
    };
    assert_eq!(h.cmd(unstake), [Event::Unstaked { account: STAKER, amount: TokenUnits(4_000) }]);
    assert_eq!(total(h), sum + 5_000);

    let events = h.cmd(Command::Admin(AdminCommand::CarryState(CarryPart::End)));
    let refused =
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("no carried genesis"));
    assert!(refused, "{events:?}");
    freeze(h);
}

/// The staking state's total staked.
fn total(h: &Harness) -> u128 {
    h.engine.staking().and_then(|staking| staking.staked_total).expect("V90 keeps a total").0
}

/// The log whose genesis carries `alpha`'s state at its last command, as described above.
pub(crate) fn carried(alpha: &mut Harness) -> Vec<Sequenced> {
    alpha.engine.state_root();
    let (parts, carried) = alpha.engine.carry_parts().expect("the frozen session carries");
    let source = alpha.engine.emissions().expect("the session emits").clone();

    let mut h = node_genesis(ProtocolVersion::V90, alpha.ts + DAY_MS);
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 30 * DAY_MS }));
    let floor = Fixed::from_int(HOUSE_CAPITAL - 2_000_000);
    h.admin(AdminCommand::InitStaking(StakingConfig {
        liquidity_floor: floor,
        epoch_ms: DAY_MS,
        start_ms: h.ts,
        cooldown_ms: 3 * DAY_MS,
    }));
    let mut events = Vec::new();
    for part in parts {
        events = h.cmd(Command::Admin(AdminCommand::CarryState(part)));
        assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    }
    assert_eq!(events, [Event::StateCarried(carried), Event::AdminApplied]);
    check_carried(&h, alpha, carried);

    // The first command past the carry closes the carried period with no weight and cancels the periods since.
    let events = h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() }));
    let current = source.schedule.period_at(h.ts).expect("the schedule has started");
    let closed = Event::EmissionPeriodClosed {
        period: source.period,
        budget: TokenUnits(source.schedule.budget(source.period)),
        allocated: TokenUnits(0),
        cancelled: TokenUnits(source.schedule.budget(source.period)),
    };
    assert_eq!(events.first(), Some(&closed), "{events:?}");
    assert!(matches!(events[1], Event::EmissionPeriodsCancelled { last, .. } if last == current - 1), "{events:?}");
    h.cmd(Command::HouseCapital { amount: Fixed::from_int(HOUSE_CAPITAL), chain_ref: "genesis".into() });

    after_genesis(&mut h, alpha);
    h.log
}

/// Switches rotation off in the registry mirror, completes the staking mirror's frontier through every block it
/// mirrored, and closes the open emission period.
fn freeze(alpha: &mut Harness) {
    let staking = alpha.engine.staking().expect("the session staked");
    let completed = staking.completed_through.map_or(0, |completed| completed.block);
    let block = staking.mirrored_through.unwrap_or(0).max(completed) + 1;
    let switch = Command::SwitchRotation {
        enabled: false,
        election_cancelled: false,
        block,
        chain_ref: "0xv90:rotation".into(),
    };
    let events = alpha.cmd(switch);
    assert!(events.iter().any(|event| matches!(event, Event::RotationSwitched { enabled: false, .. })), "{events:?}");
    let progress =
        Command::StakingProgress { block, block_hash: B256::repeat_byte(0x90), block_time: None, named_turn: None };
    let events = alpha.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    let emissions = alpha.engine.emissions().expect("the session emits");
    alpha.advance_to(emissions.schedule.period_end(emissions.period));
    alpha.cmd(Command::Tick);
}

/// The carried log holds every value the source's root proves, each award unpaid, and the source's emissions and
/// staking frontier.
fn check_carried(h: &Harness, alpha: &Harness, carried: StateCarried) {
    assert_eq!(h.engine.carried_from(), Some(carried.source));
    assert_eq!(h.engine.entitlements(), alpha.engine.entitlements());
    assert_eq!(h.engine.staked_balances(), alpha.engine.staked_balances());
    assert_eq!(h.engine.emissions(), alpha.engine.emissions());
    let (ours, theirs) = (h.engine.staking().expect("staked"), alpha.engine.staking().expect("staked"));
    assert_eq!((ours.mirrored_through, ours.completed_through), (theirs.mirrored_through, theirs.completed_through));
    assert_eq!(ours.staked_total, theirs.staked_total);
    assert!(h.engine.awards().values().all(|award| award.paid.0 == 0 && award.owed() == award.amount.0));
    assert_eq!(h.engine.awards().len(), alpha.engine.awards().len());
}

/// A carried staker unstakes and a new stake mirrors past the carried frontier; the first epoch close pays the
/// carried award first; and a carried trader's loss adds to its entitlement.
fn after_genesis(h: &mut Harness, alpha: &Harness) {
    let (&staker, &staked) = h.engine.staked_balances().iter().next().expect("the session staked");
    let completed = h.engine.staking().and_then(|staking| staking.completed_through).expect("carried").block;
    let unstake =
        Command::Unstake { account: staker, amount: staked, block: completed + 1, chain_ref: "0xv90:out".into() };
    assert_eq!(h.cmd(unstake), [Event::Unstaked { account: staker, amount: staked }]);
    let stake = Command::Stake {
        account: STAKER,
        amount: TokenUnits(1_000),
        block: completed + 2,
        chain_ref: "0xv90:in".into(),
    };
    assert_eq!(h.cmd(stake), [Event::Staked { account: STAKER, amount: TokenUnits(1_000) }]);

    let config = h.engine.staking().expect("staked").config;
    let end = config.epoch_end(0);
    h.advance_to(end);
    let block_hash = B256::repeat_byte(0x91);
    let progress =
        Command::StakingProgress { block: completed + 2, block_hash, block_time: Some(end), named_turn: None };
    let events = h.cmd(progress);
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    let events = h.cmd(Command::Tick);
    let paid = events.iter().find_map(|event| match event {
        Event::AwardPaid(paid) => Some(*paid),
        _ => None,
    });
    let paid = paid.unwrap_or_else(|| panic!("the close pays the carried award: {events:?}"));
    assert_eq!(paid.award, 0);
    assert_eq!(paid.amount.0 + paid.owed.0, alpha.engine.awards()[&0].amount.0, "{paid:?}");

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    let carried = h.engine.entitlement(client.account);
    assert!(carried.0 > 0, "the trader carried an entitlement");
    attest(h, &client);
    fund(h, &client, "1000000");
    for (price, side) in [("100000", Side::Buy), ("99000", Side::Sell)] {
        h.price(BTC, price);
        only_fill(&trade(h, &client, BTC, side, "1", price));
    }
    let emissions = h.engine.emissions().expect("carried").clone();
    h.advance_to(emissions.schedule.period_end(emissions.period));
    h.cmd(Command::Tick);
    assert_eq!(h.engine.entitlement(client.account).0, carried.0 + emissions.schedule.budget(emissions.period));
}
