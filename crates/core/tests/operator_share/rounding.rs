//! The operator's share rounds down, what it rounds off goes to stakers, and only their dust stays
//! with the house: as a table of the cases that matter and a property over the rest.

use proptest::collection::vec;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

use super::*;

#[test]
fn the_share_rounds_down_and_only_the_stakers_dust_stays_with_the_house() {
    // Excess, stakes, the operator's share, each staker's part, and the dust.
    type Case<'a> = (u128, &'a [u128], u128, &'a [u128], u128);
    let cases: [Case; 8] = [
        // Less than five units: the share rounds to nothing and stakers take it all.
        (1, &[1], 0, &[1], 0),
        (4, &[1], 0, &[4], 0),
        // The fifth exactly.
        (5, &[1], 1, &[4], 0),
        // The fraction the share rounds off goes to stakers, not the house.
        (9, &[1], 1, &[8], 0),
        // Stakers' own rounding is the dust.
        (8, &[1, 1], 1, &[3, 3], 1),
        (10, &[1, 1, 1], 2, &[2, 2, 2], 2),
        // A staker whose part rounds to nothing leaves it as dust.
        (5, &[1, 9], 1, &[0, 3], 1),
        // Stakers too many to earn a unit: the operator is the only credit.
        (6, &[1, 1, 1, 1, 1, 1, 1], 1, &[0, 0, 0, 0, 0, 0, 0], 5),
    ];
    for (excess, stakes, expected_share, parts, dust) in cases {
        let mut h = rotating(excess);
        seat(&mut h, 1, CANDIDATE, key());
        let stakers: Vec<AccountId> = (0..stakes.len()).map(|i| AccountId([0x10 + i as u8; 20])).collect();
        for (staker, amount) in stakers.iter().zip(stakes) {
            stake(&mut h, *staker, *amount);
        }
        let cash = h.engine.house_cash();
        let events = close(&mut h);
        let label = format!("excess {excess}, stakes {stakes:?}");
        assert_eq!(leaf(&h, CANDIDATE), expected_share, "{label}");
        let actual: Vec<u128> = stakers.iter().map(|staker| leaf(&h, *staker)).collect();
        assert_eq!(actual, parts, "{label}");
        let total = excess - dust;
        assert_eq!(allocated(&events), UsdcUnits(total), "{label}");
        assert_eq!(h.engine.house_cash(), cash - Fixed::from_raw(total as i128 * USDC), "{label}");
        // A share of nothing emits no event.
        assert_eq!(share(&events).is_some(), expected_share > 0, "{label}");
    }
}

#[test]
fn rounding_never_credits_more_than_the_epochs_distribution() {
    let config =
        Config { cases: 64, rng_seed: RngSeed::Fixed(0x5eed_0046), failure_persistence: None, ..Config::default() };
    let strategy = (1u128..=1_000_000_000_000u128, vec(1u128..=1_000_000_000_000_000_000_000u128, 1..6));
    let result = TestRunner::new(config).run(&strategy, |(excess, stakes)| {
        let mut h = rotating(excess);
        seat(&mut h, 1, CANDIDATE, key());
        let stakers: Vec<AccountId> = (0..stakes.len()).map(|i| AccountId([0x10 + i as u8; 20])).collect();
        for (staker, amount) in stakers.iter().zip(&stakes) {
            stake(&mut h, *staker, *amount);
        }
        let events = close(&mut h);
        let credited: u128 = leaf(&h, CANDIDATE) + stakers.iter().map(|staker| leaf(&h, *staker)).sum::<u128>();
        let check = |holds: bool, what: &str| {
            if holds { Ok(()) } else { Err(TestCaseError::fail(format!("{what}: {excess} {stakes:?} {events:?}"))) }
        };
        check(leaf(&h, CANDIDATE) == excess / 5, "the operator's share is the excess's fifth, rounded down")?;
        check(credited == allocated(&events).0, "the leaves sum to what the close allocated")?;
        check(credited <= excess, "nothing past the excess")?;
        // Each staker rounds off less than one unit of what the share leaves them.
        check(excess - credited < stakers.len() as u128, "the dust is under a unit a staker")?;
        check(h.engine.staking().unwrap().distributed.0 == credited, "cumulative distributed matches")?;
        Ok(())
    });
    if let Err(error) = result {
        panic!("{error}");
    }
}
