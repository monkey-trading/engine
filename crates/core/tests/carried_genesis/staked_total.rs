//! The total staked from `V90`: the sum of the staked balances when `V90` activates or staking is initialised
//! under it, then kept by every mirrored stake and unstake, with a stake that would carry it past `u128::MAX`
//! refused; before `V90` the staking state holds none.

use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::close_spread::{ceilings, install_expect};
use crate::common::{HOUSE_CAPITAL, Harness};
use crate::fixture::{DAY_MS, stake};

const A: AccountId = AccountId([0xa1; 20]);
const B: AccountId = AccountId([0xa2; 20]);

/// The total the staking state holds.
fn total(h: &Harness) -> Option<TokenUnits> {
    h.engine.staking().expect("staking is initialised").staked_total
}

#[test]
fn the_total_starts_at_the_activation_and_follows_every_stake_and_unstake() {
    let mut h = Harness::staking(Fixed::from_int(HOUSE_CAPITAL), DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V89 });
    stake(&mut h, A, 300, 1);
    stake(&mut h, B, 100, 2);
    assert_eq!(total(&h), None, "before V90 the staking state holds no total");

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V90 });
    assert_eq!(total(&h), Some(TokenUnits(400)));
    stake(&mut h, B, 50, 3);
    assert_eq!(total(&h), Some(TokenUnits(450)));
    let unstake = Command::Unstake { account: A, amount: TokenUnits(300), block: 4, chain_ref: "0xunstake:4".into() };
    assert_eq!(h.cmd(unstake), [Event::Unstaked { account: A, amount: TokenUnits(300) }]);
    assert_eq!(total(&h), Some(TokenUnits(150)));
    // A refused unstake leaves it.
    let over = Command::Unstake { account: B, amount: TokenUnits(151), block: 5, chain_ref: "0xunstake:5".into() };
    assert!(matches!(h.cmd(over)[..], [Event::StakingRejected { reason: Reject::InsufficientStake, .. }]));
    assert_eq!(total(&h), Some(TokenUnits(150)));
}

#[test]
fn a_stake_that_would_carry_the_total_past_its_bound_is_refused() {
    let mut h = Harness::staking(Fixed::from_int(HOUSE_CAPITAL), DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V90 });
    assert_eq!(total(&h), Some(TokenUnits(0)));
    assert!(matches!(stake(&mut h, A, u128::MAX - 1, 1)[..], [Event::Staked { .. }]));
    let events = stake(&mut h, B, 2, 2);
    assert!(matches!(events[..], [Event::StakingRejected { reason: Reject::InvalidAmount, .. }]), "{events:?}");
    assert_eq!((total(&h), h.engine.staked(B)), (Some(TokenUnits(u128::MAX - 1)), TokenUnits(0)));
    assert!(matches!(stake(&mut h, B, 1, 3)[..], [Event::Staked { .. }]));
    assert_eq!(total(&h), Some(TokenUnits(u128::MAX)));
}

#[test]
fn staking_initialised_under_v90_starts_its_total_at_zero() {
    let mut h = crate::fixture::genesis(ProtocolVersion::V90);
    h.admin(AdminCommand::InitStaking(crate::fixture::staking_config(h.ts)));
    assert_eq!(total(&h), Some(TokenUnits(0)));
}
