//! The MONKEY period close, with every account having lost in the period, and the staking epoch close at
//! 10k stakers, each at the latest rules.

use alloy_primitives::B256;
use engine_core::command::Command;
use engine_core::event::Event;

use crate::common::launch_book::{Mix, NamedCounts, Origin, build};
use crate::report::timed;
use crate::support::{accounts, count, finish, report};

const NAMED: NamedCounts = NamedCounts { adders: 1, openers: 0, closeouts: 0 };
const STAKERS: usize = 10_000;
/// Both closes come well after the book's own clock, which its assembly moves a few minutes on.
const START_AFTER_MS: i64 = 15 * 60_000;
/// Emission periods are ten minutes, so an epoch of 25 never ends with one.
const EPOCH_MS: i64 = 25 * 60_000;

/// The close of emission period 0, where every account lost and each of its tiers' boost is read.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_emission_period_close() {
    let mix = Mix {
        origin: Some(Origin { start_after: START_AFTER_MS, epoch_ms: 24 * 3_600_000 }),
        period_losses: true,
        ..Mix::proposal(accounts())
    };
    let mut book = build(&mix, NAMED);
    let end = book.h.engine.emissions().expect("emissions are initialised").schedule.period_end(0);
    assert_eq!(book.h.engine.period_results().len(), accounts() - book.named_total(), "every account lost");
    book.h.advance_to(end - 1);
    book.apply(Command::Tick);
    book.h.advance_to(end);
    let (events, us) = timed(|| book.apply(Command::Tick));
    let allocated = count(&events, |e| matches!(e, Event::EmissionAllocated { .. }));
    assert_eq!(allocated, accounts() - book.named_total(), "every account that lost is allocated");
    assert!(book.h.engine.period_results().is_empty());
    report("mass_emission_period_close", us, &events, &format!("accounts={}", accounts()));
    finish();
}

/// The close of staking epoch 0 with `STAKERS` stakers: the walk of every account's equity for the
/// excess, then each staker's distribution.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_staking_epoch_close() {
    let stakers = STAKERS.min(accounts() / 10);
    let mix = Mix {
        origin: Some(Origin { start_after: START_AFTER_MS, epoch_ms: EPOCH_MS }),
        stakers,
        house_capital: 200_000_000,
        ..Mix::proposal(accounts())
    };
    let mut book = build(&mix, NAMED);
    assert_eq!(book.h.engine.staked_balances().len(), stakers);
    let excess = book.h.engine.distributable_excess().expect("staking is initialised");
    let engine = &book.h.engine;
    assert!(
        excess.is_positive(),
        "the house holds more than the floor: excess {excess} cash {} nav {}",
        engine.house_cash(),
        engine.house_nav()
    );
    let end = book.h.engine.staking().expect("staking is initialised").config.epoch_end(0);
    // Periods end every ten minutes, the epoch between two of them, and the first command after each
    // finalises it. From `V75` an epoch also waits for the staking mirror's finalised frontier to pass its
    // end, which the node's watcher reports.
    book.h.advance_to(end - 1);
    book.apply(Command::Tick);
    let frontier = Command::StakingProgress {
        block: 1,
        block_hash: B256::repeat_byte(1),
        block_time: Some(end),
        named_turn: None,
    };
    let events = book.apply(frontier);
    assert!(events.is_empty(), "{events:?}");
    book.h.advance_to(end);
    let (events, us) = timed(|| book.apply(Command::Tick));
    let paid = count(&events, |e| matches!(e, Event::DistributionAllocated { .. }));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { .. })), "{}", events.len());
    assert_eq!(paid, stakers, "the close distributes to every staker");
    report("mass_staking_epoch_close", us, &events, &format!("stakers={stakers} paid={paid} excess={excess}"));
    finish();
}
