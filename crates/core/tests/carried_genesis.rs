//! A carried genesis from `V90`, through `Engine::apply`: a new log's genesis takes each entitlement and stake,
//! the staking mirror's frontier, the emission schedule and every award, unpaid, from a source root that proves
//! them, checks the sets complete, and continues the source's emissions with no period before the new log's
//! first command allocating (`carried_genesis/continuation.rs`); every part that does not prove, comes out of
//! order or leaves a value out is refused (`carried_genesis/refusals.rs`); and the staking state keeps its total
//! staked from `V90` (`carried_genesis/staked_total.rs`).

mod common;

#[path = "carried_genesis/continuation.rs"]
mod continuation;
#[path = "carried_genesis/fixture.rs"]
mod fixture;
#[path = "carried_genesis/refusals.rs"]
mod refusals;
#[path = "carried_genesis/staked_total.rs"]
mod staked_total;

use alloy_primitives::B256;
use common::{CHAIN_ID, VAULT};
use engine_core::awards::Award;
use engine_core::carry::{CarryPart, CarrySource, MAX_CARRY_ENTRIES, StateCarried};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::staking::UsdcUnits;
use fixture::{STAKERS, UNSTAKED, carry, destination, source};

#[test]
fn a_carried_genesis_takes_every_value_the_source_root_proves() {
    let mut alpha = source();
    let root = B256::from(alpha.engine.state_root());
    let (parts, totals) = alpha.engine.carry_parts().expect("the source carries");
    let seq = alpha.engine.last_seq().expect("the source has commands");
    let expected = CarrySource { chain_id: CHAIN_ID, vault: VAULT, seq, root };
    assert!(matches!(&parts[0], CarryPart::Begin { source, .. } if *source == expected), "{:?}", parts[0]);
    assert_eq!(parts.last(), Some(&CarryPart::End));

    let mut h = destination(&alpha);
    let events = carry(&mut h, &parts);
    let alpha_emissions = alpha.engine.emissions().expect("the source emits");
    let staked: u128 = STAKERS.iter().map(|(_, amount)| amount).sum::<u128>() - UNSTAKED;
    let carried = StateCarried {
        source: expected,
        accounts: 2 + STAKERS.len() as u64,
        entitled: alpha_emissions.entitled,
        staked: TokenUnits(staked),
        awards: 1,
    };
    assert_eq!((events, totals), (vec![Event::StateCarried(carried), Event::AdminApplied], carried));

    assert_eq!(h.engine.carried_from(), Some(expected));
    assert!(h.engine.carry_progress().is_none());
    assert_eq!(h.engine.entitlements(), alpha.engine.entitlements());
    assert_eq!(h.engine.entitlements().len(), 2, "both traders earned an entitlement");
    assert_eq!(h.engine.staked_balances(), alpha.engine.staked_balances());
    assert_eq!(h.engine.emissions(), Some(alpha_emissions));
    let (ours, theirs) = (h.engine.staking().expect("staked"), alpha.engine.staking().expect("staked"));
    assert_eq!(ours.staked_total, Some(TokenUnits(staked)));
    assert_eq!((ours.mirrored_through, ours.completed_through), (theirs.mirrored_through, theirs.completed_through));
    assert_eq!((ours.config, ours.epoch, ours.distributed), (fixture::staking_config(h.ts), 0, UsdcUnits(0)));
    let unpaid: Vec<Award> =
        alpha.engine.awards().values().map(|award| Award { paid: UsdcUnits(0), ..award.clone() }).collect();
    assert!(alpha.engine.awards()[&0].paid.0 > 0, "the source paid its award in part");
    assert_eq!(h.engine.awards().values().cloned().collect::<Vec<_>>(), unpaid);
    assert!(h.engine.distribution(fixture::RESEARCHER).is_zero(), "distributions are not carried");
}

#[test]
fn the_parts_carry_at_most_the_batch_bound_each_in_increasing_order() {
    let mut alpha = source();
    // Enough stakers that the stakes take two parts.
    for (block, n) in (8..).zip(0..MAX_CARRY_ENTRIES as u8) {
        let mut account = [0xb0; 20];
        account[19] = n;
        let events = fixture::stake(&mut alpha, engine_core::types::AccountId(account), 1, block);
        assert!(matches!(events[..], [Event::Staked { .. }]), "{events:?}");
    }
    let progress = engine_core::command::Command::StakingProgress {
        block: 8 + MAX_CARRY_ENTRIES as u64,
        block_hash: B256::repeat_byte(8),
        block_time: None,
        named_turn: None,
    };
    alpha.cmd(progress);
    alpha.engine.state_root();
    let (parts, totals) = alpha.engine.carry_parts().expect("the source carries");
    let stakes: Vec<usize> = parts
        .iter()
        .filter_map(|part| match part {
            CarryPart::Stakes { entries } => Some(entries.len()),
            _ => None,
        })
        .collect();
    assert_eq!(stakes, [MAX_CARRY_ENTRIES, STAKERS.len()]);
    let mut h = destination(&alpha);
    let events = carry(&mut h, &parts);
    assert_eq!(events[0], Event::StateCarried(totals));
    assert_eq!(h.engine.staked_balances(), alpha.engine.staked_balances());
}
