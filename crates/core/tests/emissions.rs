//! MONKEY emissions from `V19`, end to end through `Engine::apply`: fixed ten-minute budgets shared
//! pro rata by net realised losses, rounding, cancellation, and finality at period boundaries.
//! Eligibility details are in `emissions_eligibility.rs`; the schedule's arithmetic in
//! `emissions_schedule.rs`.

mod common;

use common::emissions::{allocations, cancelled_ranges, closed, funded, next_period, round_trip, schedule};
use common::events::fills;
use common::{BTC, Harness, acct, f};
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::types::Side;

const BUDGET: u128 = 1_000_000;

#[test]
fn a_period_budget_is_shared_pro_rata_by_realised_losses_rounding_down() {
    let mut h = Harness::emitting(BUDGET + 1);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", "99900");
    assert_eq!(h.engine.period_results()[&a].realised, f("-100"));

    let events = next_period(&mut h);
    // 250,000.25 and 750,000.75 round down, and the unit left over is cancelled.
    assert_eq!(allocations(&events), [(a, 0, f("100"), 250_000), (b, 0, f("300"), 750_000)]);
    assert_eq!(closed(&events), [(0, BUDGET + 1, BUDGET, 1)]);
    assert_eq!((h.engine.entitlement(a), h.engine.entitlement(b)), (TokenUnits(250_000), TokenUnits(750_000)));
    assert!(h.engine.period_results().is_empty(), "a finalised period's results are cleared");
}

#[test]
fn larger_losses_change_shares_but_never_the_budget() {
    // Losses of 100 and 300, then of 1,000 and 3,000: the same budget, split 25% and 75% each time.
    for to in ["99900", "99000"] {
        let mut h = Harness::emitting(BUDGET);
        let [a, b] = funded(&mut h);
        round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", to);
        let events = next_period(&mut h);
        let amounts: Vec<u128> = allocations(&events).into_iter().map(|(.., amount)| amount).collect();
        assert_eq!(amounts, [250_000, 750_000], "{to}");
        assert_eq!(closed(&events), [(0, BUDGET, BUDGET, 0)], "{to}");
    }
}

#[test]
fn a_one_unit_budget_rounds_every_share_down_and_cancels_the_unit() {
    let mut h = Harness::emitting(1);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", "99900");
    let events = next_period(&mut h);
    assert_eq!(allocations(&events), [(a, 0, f("100"), 0), (b, 0, f("300"), 0)]);
    assert_eq!(closed(&events), [(0, 1, 0, 1)]);

    // A single account with weight takes the whole unit.
    round_trip(&mut h, &[(a, "1")], "100000", "99999");
    assert_eq!(allocations(&next_period(&mut h)), [(a, 1, f("1"), 1)]);
    assert_eq!(h.engine.entitlement(a), TokenUnits(1));
}

#[test]
fn profits_and_flat_closes_add_no_weight_and_an_empty_period_cancels_its_budget_for_good() {
    let mut h = Harness::emitting(BUDGET);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1")], "100000", "100100");
    round_trip(&mut h, &[(b, "1")], "100000", "100000");
    assert!(!h.engine.period_results().contains_key(&b), "a flat close records nothing");
    let events = next_period(&mut h);
    assert!(allocations(&events).is_empty(), "{events:?}");
    assert_eq!(closed(&events), [(0, BUDGET, 0, BUDGET)]);

    // Period 1 holds only the tick that finalised period 0. Periods 2 to 4 pass with no command, so
    // the first command in period 5 finalises period 1 and cancels them in one step.
    let schedule = schedule(&h);
    h.advance_to(schedule.period_end(4) + 1);
    let events = h.cmd(Command::Tick);
    assert_eq!(closed(&events), [(1, BUDGET, 0, BUDGET)]);
    assert_eq!(cancelled_ranges(&events), [(2, 4, 3 * BUDGET)]);

    // A loss now earns exactly one period's budget: nothing cancelled carries forward.
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    assert_eq!(allocations(&next_period(&mut h)), [(a, 5, f("100"), BUDGET)]);
    let emissions = h.engine.emissions().unwrap();
    assert_eq!((emissions.entitled, emissions.cancelled), (TokenUnits(BUDGET), TokenUnits(5 * BUDGET)));
    // Every finalised period's budget is either an entitlement or cancelled.
    assert_eq!(emissions.entitled.0 + emissions.cancelled.0, schedule.budgets(0, emissions.period));
}

#[test]
fn the_first_command_at_a_period_end_finalises_it_before_any_of_its_own_effects() {
    let mut h = Harness::emitting(BUDGET);
    let [a, b] = funded(&mut h);
    let leveraged = acct(3);
    h.deposit(leveraged, "10001");
    let end = schedule(&h).period_end(0);
    h.price(BTC, "100000");
    for (account, qty) in [(a, "1"), (b, "1"), (leveraged, "1")] {
        assert_eq!(fills(&h.market_order(account, BTC, Side::Buy, qty, "100000")).len(), 1);
    }
    h.advance_to(end - 1);
    h.price(BTC, "99900");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Sell, "1", "99900")).len(), 1);

    // The price at the boundary closes out the leveraged account, and b sells there too: both
    // belong to period 1, after period 0 is final.
    h.advance_to(end);
    let events = h.price(BTC, "85000");
    assert!(matches!(events[..2], [Event::EmissionAllocated { .. }, Event::EmissionPeriodClosed { .. }]), "{events:?}");
    assert_eq!(allocations(&events), [(a, 0, f("100"), BUDGET)]);
    assert!(fills(&events).iter().any(|fill| fill.liquidation && fill.account == leveraged), "{events:?}");
    assert_eq!(fills(&h.market_order(b, BTC, Side::Sell, "1", "85000")).len(), 1);
    assert_eq!(h.engine.emissions().unwrap().period, 1);
    let mut accrued: Vec<_> = h.engine.period_results().keys().copied().collect();
    accrued.sort();
    assert_eq!(accrued, [b, leveraged]);
}

#[test]
fn replaying_the_log_reproduces_every_allocation_and_the_root() {
    let mut h = Harness::emitting(BUDGET);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", "99900");
    next_period(&mut h);
    round_trip(&mut h, &[(b, "2")], "100000", "99950");
    let schedule = schedule(&h);
    h.advance_to(schedule.period_end(7));
    h.cmd(Command::Tick);

    let mut replayed = engine_core::Engine::new();
    for sequenced in &h.log {
        replayed.apply(sequenced).expect("in sequence");
    }
    assert_eq!(replayed.emissions(), h.engine.emissions());
    assert_eq!((replayed.entitlement(a), replayed.entitlement(b)), (h.engine.entitlement(a), h.engine.entitlement(b)));
    assert_eq!(replayed.state_root(), h.engine.state_root());
}

#[test]
fn an_entitlement_proves_against_the_root_it_was_taken_at_and_grows_only() {
    let mut h = Harness::emitting(BUDGET);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", "99900");
    h.engine.state_root();
    assert_eq!(h.engine.entitlement_proof(a), None, "nothing is entitled before finalisation");
    next_period(&mut h);
    let root = h.engine.state_root().into();
    let proof = h.engine.entitlement_proof(a).unwrap();
    assert_eq!((proof.root, proof.entitled, proof.seq), (root, TokenUnits(250_000), h.log.len() as u64 - 1));
    assert!(proof.verify(root));
    assert!(!engine_core::merkle::EntitlementProof { entitled: TokenUnits(250_001), ..proof.clone() }.verify(root));
    assert!(!engine_core::merkle::EntitlementProof { account: b, ..proof.clone() }.verify(root));

    // A later allocation adds to the entitlement; the earlier proof stays tied to its own root.
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    next_period(&mut h);
    let later = h.engine.state_root().into();
    let grown = h.engine.entitlement_proof(a).unwrap();
    assert_eq!(grown.entitled, TokenUnits(250_000 + BUDGET));
    assert!(grown.verify(later) && !proof.verify(later) && !grown.verify(root));
}
