//! The score's decay in exact integer steps. Time is cut into whole hours of sequenced time from the
//! epoch, and a value decays by the hours its step crosses: each whole half-life (168 hours) halves
//! it, and the hours left over multiply it by the table entry for that many hours. Entry `h` is the
//! hourly factor `2^(-1/168)`, rounded to nine decimals, applied `h` times with each product
//! truncated, so the table falls strictly from 1 and its last entry stays above one half. A decayed
//! value therefore never grows with the hours crossed, and its magnitude never grows: every product
//! truncates toward zero.

use crate::Fixed;
use crate::fixed::Rounding;
use crate::toxicity::HALF_LIFE_MS;
use crate::types::Millis;

/// One decay step: an hour of sequenced time.
pub(crate) const STEP_MS: Millis = 60 * 60 * 1000;
/// The steps in one half-life.
const STEPS: usize = (HALF_LIFE_MS / STEP_MS) as usize;
/// `2^(-1/168)` in `Fixed`'s raw units, rounded to the nearest.
const STEP_FACTOR: i128 = 995_882_624;
/// The factor for each count of steps short of a half-life.
const FACTORS: [i128; STEPS] = factors();
/// Halvings past which every value representable as `Fixed` decays to zero.
const MAX_HALVINGS: i64 = 127;

/// `FACTORS`: `STEP_FACTOR` raised to each power below `STEPS`, truncating after each product.
const fn factors() -> [i128; STEPS] {
    let one = Fixed::ONE.raw();
    let mut table = [one; STEPS];
    let mut step = 1;
    while step < STEPS {
        table[step] = table[step - 1] * STEP_FACTOR / one;
        step += 1;
    }
    table
}

/// The decay step `ts` falls in.
pub(crate) fn step_of(ts: Millis) -> i64 {
    ts.div_euclid(STEP_MS)
}

/// `value` decayed by `steps` steps, truncated toward zero; `value` itself for none.
pub(crate) fn decay(value: Fixed, steps: i64) -> Fixed {
    if steps <= 0 || value.is_zero() {
        return value;
    }
    let halvings = steps / STEPS as i64;
    if halvings >= MAX_HALVINGS {
        return Fixed::ZERO;
    }
    // Dividing rather than shifting truncates a negative value toward zero too.
    let halved = Fixed::from_raw(value.raw() / (1i128 << halvings));
    let factor = FACTORS[(steps % STEPS as i64) as usize];
    halved.mul_div(Fixed::from_raw(factor), Fixed::ONE, Rounding::TowardZero)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn the_table_falls_strictly_from_one_and_ends_above_a_half() {
        assert_eq!(FACTORS[0], Fixed::ONE.raw());
        assert!(FACTORS.windows(2).all(|pair| pair[1] < pair[0]), "{FACTORS:?}");
        assert!(FACTORS[STEPS - 1] > Fixed::ONE.raw() / 2, "{}", FACTORS[STEPS - 1]);
        // A whole half-life halves exactly; a step short of it is within a ten-millionth of the
        // curve's 0.502067199...
        assert_eq!(decay(Fixed::from_int(10), STEPS as i64), Fixed::from_int(5));
        let short = decay(Fixed::ONE, STEPS as i64 - 1).raw();
        assert!((short - 502_067_199).abs() < 100, "{short}");
    }

    #[test]
    fn a_value_decays_to_zero_past_the_halvings_its_range_holds() {
        let max = Fixed::from_raw(i128::MAX);
        assert_eq!(decay(max, MAX_HALVINGS * STEPS as i64), Fixed::ZERO);
        assert!(decay(max, (MAX_HALVINGS - 1) * STEPS as i64).is_positive());
        assert_eq!(decay(Fixed::ONE, i64::MAX), Fixed::ZERO);
    }

    #[test]
    fn no_step_leaves_a_value_as_it_is() {
        assert_eq!(decay(Fixed::from_int(-7), 0), Fixed::from_int(-7));
        assert_eq!(step_of(STEP_MS - 1), 0);
        assert_eq!(step_of(STEP_MS), 1);
    }

    proptest! {
        /// More steps never leave more of a value, of either sign, and never flip its sign.
        #[test]
        fn decay_is_monotone_in_the_steps(raw in -(1i128 << 100)..(1i128 << 100), steps in 0i64..10_000, more in 0i64..400) {
            let value = Fixed::from_raw(raw);
            let (fewer, later) = (decay(value, steps), decay(value, steps + more));
            prop_assert!(later.abs() <= fewer.abs(), "{value} after {steps} then {more} steps: {fewer} -> {later}");
            prop_assert!(later.signum() * value.signum() >= 0);
            prop_assert!(fewer.abs() <= value.abs());
        }
    }
}
