//! The new log after its carried genesis: its emissions continue from the source's period with none before its
//! own first command allocating, carried stakes unstake and the mirror resumes past the carried frontier, and its
//! first distributing close pays the carried award from its whole amount.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::awards::{AwardPaid, Severity};
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::staking::UsdcUnits;
use engine_core::types::AccountId;

use crate::common::close_spread::{attest, fund};
use crate::common::{HOUSE_CAPITAL, Harness};
use crate::fixture::{
    RESEARCHER, STAKERS, TRADER_SEEDS, UNSTAKED, carry, destination, lose, next_period, source, stake, trader,
};

/// A new log carried from `source`, its genesis credited with house capital, and the credit's events.
fn carried() -> (Harness, Harness, Vec<Event>) {
    let alpha = source();
    let (parts, _) = alpha.engine.carry_parts().expect("the source carries");
    let mut h = destination(&alpha);
    carry(&mut h, &parts);
    let credit = h.cmd(Command::HouseCapital { amount: Fixed::from_int(HOUSE_CAPITAL), chain_ref: "genesis".into() });
    (alpha, h, credit)
}

#[test]
fn the_first_command_past_the_carried_period_closes_it_with_no_weight_and_cancels_every_skipped_one() {
    let (alpha, mut h, tick) = carried();
    let before = alpha.engine.emissions().expect("the source emits").clone();
    let schedule = before.schedule;
    let current = schedule.period_at(h.ts).expect("the schedule has started");
    assert!(current > before.period + 1, "a day passed between the logs");
    let budget = schedule.budget(before.period);
    let closes: Vec<&Event> = tick
        .iter()
        .filter(|event| matches!(event, Event::EmissionPeriodClosed { .. } | Event::EmissionPeriodsCancelled { .. }))
        .collect();
    let skipped = schedule.budgets(before.period + 1, current);
    assert_eq!(
        closes,
        [
            &Event::EmissionPeriodClosed {
                period: before.period,
                budget: TokenUnits(budget),
                allocated: TokenUnits(0),
                cancelled: TokenUnits(budget),
            },
            &Event::EmissionPeriodsCancelled {
                first: before.period + 1,
                last: current - 1,
                cancelled: TokenUnits(skipped)
            },
        ],
        "{tick:?}"
    );
    let after = h.engine.emissions().expect("carried");
    assert_eq!((after.period, after.entitled), (current, before.entitled));
    assert_eq!(after.cancelled.0, before.cancelled.0 + budget + skipped);

    // A carried account's loss in the new log adds to the entitlement it carried.
    let seed = TRADER_SEEDS[0];
    let client = trader(&h, seed);
    let carried = h.engine.entitlement(client.account);
    assert!(carried.0 > 0);
    attest(&mut h, &client);
    fund(&mut h, &client, "1000000");
    lose(&mut h, seed);
    let close = next_period(&mut h);
    let allocated: u128 = close
        .iter()
        .filter_map(|event| match event {
            Event::EmissionAllocated { account, amount, .. } if *account == client.account => Some(amount.0),
            _ => None,
        })
        .sum();
    assert_eq!(allocated, schedule.budget(current), "the only loss takes the whole budget");
    assert_eq!(h.engine.entitlement(client.account).0, carried.0 + allocated);
}

#[test]
fn a_carried_stake_unstakes_and_the_mirror_resumes_only_past_the_carried_frontier() {
    let (_, mut h, _) = carried();
    let (staker, amount) = STAKERS[0];
    let total = h.engine.staking().and_then(|staking| staking.staked_total).expect("a total").0;
    let unstake = Command::Unstake {
        account: staker,
        amount: TokenUnits(amount - UNSTAKED),
        block: 8,
        chain_ref: "0xunstake:8".into(),
    };
    assert_eq!(h.cmd(unstake), [Event::Unstaked { account: staker, amount: TokenUnits(amount - UNSTAKED) }]);
    assert_eq!(h.engine.staked(staker), TokenUnits(0));
    let completed = h.engine.staking().and_then(|staking| staking.completed_through).expect("a frontier");
    // The carried frontier's block, under another hash, and a block below it are refused.
    for block in [completed.block, completed.block - 1] {
        let progress =
            Command::StakingProgress { block, block_hash: B256::repeat_byte(0xee), block_time: None, named_turn: None };
        let events = h.cmd(progress);
        assert!(matches!(events.last(), Some(Event::AdminRejected { .. })), "{events:?}");
    }
    assert_eq!(stake(&mut h, RESEARCHER, 5, 9), [Event::Staked { account: RESEARCHER, amount: TokenUnits(5) }]);
    let staking = h.engine.staking().expect("staked");
    assert_eq!(staking.staked_total, Some(TokenUnits(total - (amount - UNSTAKED) + 5)));
    assert_eq!(staking.mirrored_through, Some(9));
}

#[test]
fn the_first_distributing_close_pays_the_carried_award_from_its_whole_amount() {
    let (alpha, mut h, _) = carried();
    let award = &h.engine.awards()[&0];
    assert_eq!((award.owed(), award.amount), (Severity::Critical.amount().0, Severity::Critical.amount()));
    assert!(alpha.engine.awards()[&0].owed() < award.owed());
    let config = h.engine.staking().expect("staked").config;
    let end = config.epoch_end(0);
    h.advance_to(end);
    let block = h.engine.staking().and_then(|staking| staking.completed_through).expect("a frontier").block + 1;
    let progress =
        Command::StakingProgress { block, block_hash: B256::repeat_byte(1), block_time: Some(end), named_turn: None };
    assert!(!h.cmd(progress).iter().any(|event| matches!(event, Event::AdminRejected { .. })));
    let events = h.cmd(Command::Tick);
    let paid: Vec<&AwardPaid> = events
        .iter()
        .filter_map(|event| match event {
            Event::AwardPaid(paid) => Some(paid),
            _ => None,
        })
        .collect();
    let cap = h.engine.award_cap().max.0;
    let expected = AwardPaid {
        award: 0,
        account: RESEARCHER,
        epoch: 0,
        amount: UsdcUnits(cap),
        owed: UsdcUnits(Severity::Critical.amount().0 - cap),
    };
    assert_eq!(paid, [&expected], "{events:?}");
    assert_eq!(h.engine.distribution(RESEARCHER), UsdcUnits(cap));
    // The rest of the excess goes to the carried stakers.
    let staker = STAKERS[1].0;
    let allocated =
        events.iter().any(|event| matches!(event, Event::DistributionAllocated { account, .. } if *account == staker));
    assert!(allocated, "{events:?}");
}

#[test]
fn the_launch_operators_installation_past_the_carried_frontier_keeps_its_key_and_the_carried_state() {
    let (_, mut h, _) = carried();
    let operator = crate::common::Client::operator().account;
    let before = (h.engine.entitlements().clone(), h.engine.staked_balances().clone(), h.engine.awards().clone());
    let block = h.engine.staking().and_then(|staking| staking.completed_through).expect("a frontier").block + 1;
    let installation = engine_core::election::RegistryInstallation {
        candidate: AccountId([0xc1; 20]),
        admin_key: operator,
        block,
        chain_ref: "0xinstall:launch".into(),
    };
    let events = h.cmd(Command::InstallOperator(installation));
    assert!(events.iter().any(|event| matches!(event, Event::OperatorInstalled { .. })), "{events:?}");
    // Seven days on, the key in effect is still the operator's: its admin commands apply.
    h.advance(engine_core::operator_bounds::NOTICE_MS);
    h.operator_admin(engine_core::command::AdminCommand::SetReduceOnly {
        market: Some(crate::common::BTC),
        reduce_only: false,
    });
    let after = (h.engine.entitlements().clone(), h.engine.staked_balances().clone(), h.engine.awards().clone());
    assert_eq!(after, before);
}
