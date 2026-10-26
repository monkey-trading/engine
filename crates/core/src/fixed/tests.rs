use super::*;
use alloy_primitives::I256;
use proptest::arbitrary::any;
use proptest::prop_assert_eq;
use proptest::prop_oneof;
use proptest::sample::select;
use proptest::strategy::Strategy;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}

#[test]
fn checked_product_quotients_reject_zero_and_overflow_and_preserve_signed_rounding() {
    let maximum = Fixed::from_raw(i128::MAX);
    assert_eq!(maximum.checked_mul_div(maximum, maximum, Rounding::Down), Some(maximum));
    assert_eq!(maximum.checked_mul_div(f("2"), Fixed::ONE, Rounding::Down), None);
    assert_eq!(Fixed::ONE.checked_mul_div(Fixed::ONE, Fixed::ZERO, Rounding::Down), None);
    assert_eq!(f("-0.5").checked_mul_div(f("0.000000001"), Fixed::ONE, Rounding::Down), Some(f("-0.000000001")));
    assert_eq!(f("-0.5").checked_mul_div(f("0.000000001"), Fixed::ONE, Rounding::Up), Some(Fixed::ZERO));
}

#[test]
fn directed_rounding_brackets_the_exact_result_for_either_sign() {
    // 0.000000003 * 1.5 = 0.0000000045, and 1 / 3 = 0.333..., each of either sign.
    let cases = [
        (f("0.000000003").mul_rounded(f("1.5"), Rounding::Down), f("0.000000004")),
        (f("0.000000003").mul_rounded(f("1.5"), Rounding::Up), f("0.000000005")),
        (f("-0.000000003").mul_rounded(f("1.5"), Rounding::Down), f("-0.000000005")),
        (f("-0.000000003").mul_rounded(f("1.5"), Rounding::Up), f("-0.000000004")),
        (f("1").div_rounded(f("3"), Rounding::Down), f("0.333333333")),
        (f("1").div_rounded(f("3"), Rounding::Up), f("0.333333334")),
        (f("-1").div_rounded(f("3"), Rounding::Down), f("-0.333333334")),
        (f("-1").div_rounded(f("3"), Rounding::Up), f("-0.333333333")),
        (f("1").div_rounded(f("-3"), Rounding::Down), f("-0.333333334")),
        (f("1").div_rounded(f("-3"), Rounding::Up), f("-0.333333333")),
        // Exact results do not move.
        (f("2.5").mul_rounded(f("-4"), Rounding::Down), f("-10")),
        (f("2.5").mul_rounded(f("-4"), Rounding::Up), f("-10")),
        (f("-9").div_rounded(f("3"), Rounding::Down), f("-3")),
        (f("-9").div_rounded(f("3"), Rounding::Up), f("-3")),
    ];
    for (n, (got, want)) in cases.into_iter().enumerate() {
        assert_eq!(got, want, "case {n}");
    }
    // Products and quotients whose full intermediate exceeds `i128` but whose result fits.
    let big = Fixed::from_raw(18_000_000_000_000_000_000_000_000_000);
    assert_eq!(big.mul_rounded(f("10"), Rounding::Up), Fixed::from_raw(180_000_000_000_000_000_000_000_000_000));
    assert_eq!(f("10").mul_rounded(big, Rounding::Down), big.mul_int(10));
    let odd = big + Fixed::from_raw(1);
    assert_eq!(odd.mul_rounded(f("0.5"), Rounding::Down), Fixed::from_raw(big.raw() / 2));
    assert_eq!(odd.mul_rounded(f("0.5"), Rounding::Up), Fixed::from_raw(big.raw() / 2 + 1));
    let huge = big.mul_int(100) + Fixed::from_raw(1);
    assert_eq!(huge.div_rounded(f("0.1"), Rounding::Down), huge.mul_int(10));
    assert_eq!(huge.div_rounded(f("3"), Rounding::Down), Fixed::from_raw(huge.raw() / 3));
    assert_eq!(huge.div_rounded(f("3"), Rounding::Up), Fixed::from_raw(huge.raw() / 3 + 1));
    assert_eq!((-huge).div_rounded(f("3"), Rounding::Down), Fixed::from_raw(-huge.raw() / 3 - 1));
    // Over a grid of signs and magnitudes: down <= exact <= up, one unit apart unless exact.
    let values = [-7_000_000_001i128, -1_500_000_000, -3, -1, 1, 2, 999_999_999, 1_083_712_345, 151_370_000_000];
    for (a, b) in values.iter().flat_map(|a| values.iter().map(move |b| (*a, *b))) {
        for (num, den, down, up) in [
            (
                a * b,
                SCALE,
                Fixed(a).mul_rounded(Fixed(b), Rounding::Down),
                Fixed(a).mul_rounded(Fixed(b), Rounding::Up),
            ),
            (
                a * SCALE,
                b,
                Fixed(a).div_rounded(Fixed(b), Rounding::Down),
                Fixed(a).div_rounded(Fixed(b), Rounding::Up),
            ),
        ] {
            // Compare `down * den <= num <= up * den`, flipping the inequalities for a negative `den`.
            let (lo, hi) = (down.0 * den, up.0 * den);
            let bracketed = if den > 0 { lo <= num && num <= hi } else { lo >= num && num >= hi };
            assert!(bracketed, "{a} {b}: {down:?} {up:?}");
            assert_eq!(up.0 - down.0, i128::from(num % den != 0), "{a} {b}");
        }
    }
}

#[test]
fn parse_and_display_round_trip() {
    for s in ["0", "1", "-1", "1.5", "-0.000000001", "123456.789"] {
        assert_eq!(f(s).to_string(), s);
    }
    assert_eq!(f(".5"), f("0.5"));
    // Ten fractional digits is more precision than the type holds.
    assert!("1.0000000001".parse::<Fixed>().is_err());
    assert!("abc".parse::<Fixed>().is_err());
    assert!("".parse::<Fixed>().is_err());
    assert!("1e5".parse::<Fixed>().is_err());
}

#[test]
fn arithmetic_truncates_toward_zero() {
    assert_eq!(f("1.5").mul(f("2")), f("3"));
    assert_eq!(f("1").div(f("3")), f("0.333333333"));
    assert_eq!(f("-1").div(f("3")), f("-0.333333333"));
    assert_eq!(f("1000").mul_bps(25), f("2.5"));
    assert_eq!(Fixed::from_bps(1), f("0.0001"));
    assert_eq!((f("101") - f("100")).bps_of(f("100")), 100);
}

#[test]
fn rounded_arithmetic_rounds_the_exact_result_once_in_the_direction_asked() {
    let (third, two_thirds) = (f("0.333333333"), f("0.666666667"));
    assert_eq!(f("1").mul_div(f("1"), f("3"), Rounding::Down), third);
    assert_eq!(f("1").mul_div(f("1"), f("3"), Rounding::Up), f("0.333333334"));
    assert_eq!(f("-1").mul_div(f("1"), f("3"), Rounding::Down), f("-0.333333334"));
    assert_eq!(f("-1").mul_div(f("1"), f("3"), Rounding::Up), -third);
    // A negative divisor flips the sign of the result, not the direction.
    assert_eq!(f("1").mul_div(f("1"), f("-3"), Rounding::Down), f("-0.333333334"));
    assert_eq!(f("2").mul_div(f("1"), f("3"), Rounding::Up), two_thirds);
    // Exact results are not moved.
    assert_eq!(f("1.5").mul_div(f("4"), f("3"), Rounding::Up), f("2"));
    assert_eq!(f("1.1").mul_rounded(f("0.000000001"), Rounding::Down), Fixed::from_raw(1));
    assert_eq!(f("1.1").mul_rounded(f("0.000000001"), Rounding::Up), Fixed::from_raw(2));
    assert_eq!(f("-1.1").mul_rounded(f("0.000000001"), Rounding::Down), Fixed::from_raw(-2));
    assert_eq!(f("-1.1").mul_rounded(f("0.000000001"), Rounding::Up), Fixed::from_raw(-1));
    assert_eq!(Fixed::from_raw(9).mul_bps_rounded(1_000, Rounding::Up), Fixed::from_raw(1));
    assert_eq!(Fixed::from_raw(-9).mul_bps_rounded(1_000, Rounding::Down), Fixed::from_raw(-1));
    assert_eq!(Fixed::from_raw(9).mul_bps_rounded(1_000, Rounding::Down), Fixed::ZERO);
    assert_eq!(f("1").div_int_rounded(3, Rounding::Up), f("0.333333334"));
    assert_eq!(f("-1").div_int_rounded(3, Rounding::Up), -third);
    assert_eq!(f("-1").div_int_rounded(3, Rounding::Down), f("-0.333333334"));
}

#[test]
fn truncating_products_and_quotients_overflow_only_outside_the_range() {
    // 1.71e11 units at an index of 1e9: the scaled product, 1.71e38, is past `i128`, the result
    // is not. Each sign truncates toward zero, as the plain formula does where it fits.
    let qty = Fixed::from_int(171_000_000_000);
    let mid = Fixed::MAX_INPUT;
    assert_eq!(qty.mul(mid), Fixed::from_int(171_000_000_000).mul_int(1_000_000_000));
    assert_eq!((-qty).mul(mid), -qty.mul(mid));
    let odd = Fixed::from_raw(qty.raw() + 1);
    assert_eq!(odd.mul(f("0.5")), Fixed::from_raw(qty.raw() / 2));
    assert_eq!((-odd).mul(f("0.5")), Fixed::from_raw(-qty.raw() / 2));
    let huge = Fixed::from_raw(1_800_000_000_000_000_000_000_000_000_001);
    assert_eq!(huge.div(f("0.1")), huge.mul_int(10));
    assert_eq!(huge.div(f("3")), Fixed::from_raw(huge.raw() / 3));
    assert_eq!((-huge).div(f("3")), Fixed::from_raw(-huge.raw() / 3));
    // Where the plain formula fits, the result is the same, for every sign.
    let values = [-7_000_000_001i128, -1_500_000_000, -3, -1, 1, 2, 999_999_999, 1_083_712_345, 151_370_000_000];
    for (a, b) in values.iter().flat_map(|a| values.iter().map(move |b| (*a, *b))) {
        assert_eq!(Fixed(a).mul(Fixed(b)), Fixed(a * b / SCALE), "{a} * {b}");
        assert_eq!(Fixed(a).div(Fixed(b)), Fixed(a * SCALE / b), "{a} / {b}");
    }
}

#[test]
fn bps_of_saturates_instead_of_wrapping() {
    // The smallest base against the largest input is ~10^22 bps, beyond `i64`.
    assert_eq!(Fixed::MAX_INPUT.bps_of(Fixed::from_raw(1)), i64::MAX);
    assert_eq!((-Fixed::MAX_INPUT).bps_of(Fixed::from_raw(1)), i64::MIN);
}

#[test]
fn products_of_valid_inputs_do_not_overflow() {
    let max = Fixed::MAX_INPUT;
    assert!(max.is_valid_input());
    assert!(!(max + Fixed::from_raw(1)).is_valid_input());
    assert!(!Fixed::ZERO.is_valid_input());
    assert_eq!(max.mul(max).mul_bps(10_000), f("1000000000000000000"));
}

#[test]
fn results_inside_the_range_do_not_overflow_however_wide_the_raw_intermediate() {
    // 171bn units at the input ceiling: a raw product of 1.71e38, past `i128::MAX`.
    assert_eq!(f("171000000000").mul(Fixed::MAX_INPUT), f("171000000000000000000"));
    assert_eq!(Fixed::MAX_INPUT.mul_rounded(f("-171000000000.000000001"), Rounding::Down), f("-171000000000000000001"));
    // A third of the range, whose raw value times 10,000 or 10^9 is past `i128::MAX`.
    let (big, odd) = (Fixed::from_raw(i128::MAX / 3), Fixed::from_raw(i128::MAX / 3 + 1));
    assert_eq!(big.mul_bps(10_000), big);
    assert_eq!(odd.mul_bps_rounded(5_000, Rounding::Down), Fixed::from_raw(odd.raw() / 2));
    assert_eq!(odd.mul_bps_rounded(5_000, Rounding::Up), Fixed::from_raw(odd.raw() / 2 + 1));
    assert_eq!((-odd).mul_bps_rounded(5_000, Rounding::Down), Fixed::from_raw(-odd.raw() / 2 - 1));
    assert_eq!(big.mul_bps(3), Fixed::from_raw(17_014_118_346_046_923_173_168_730_371_588_410));
    assert_eq!(big.div(f("10")), Fixed::from_raw(big.raw() / 10));
    assert_eq!(big.div_rounded(f("10"), Rounding::Up), Fixed::from_raw(big.raw() / 10 + 1));
    assert_eq!(big.mul_div(big, big, Rounding::Down), big);
    // A base so large that its remainder times 10,000 is past `i128::MAX`.
    assert_eq!(big.bps_of(big.mul_int(2)), 5_000);
    assert_eq!(big.bps_of(-big), -10_000);
}

#[test]
#[should_panic(expected = "fixed-point result out of range")]
fn a_result_outside_the_range_halts() {
    Fixed::from_raw(i128::MAX).mul(f("1.000000001"));
}

#[test]
fn toward_zero_truncates_either_sign() {
    assert_eq!(f("0.000000003").mul_rounded(f("0.5"), Rounding::TowardZero), Fixed::from_raw(1));
    assert_eq!(f("-0.000000003").mul_rounded(f("0.5"), Rounding::TowardZero), Fixed::from_raw(-1));
    assert_eq!(f("-1").div_rounded(f("3"), Rounding::TowardZero), f("-0.333333333"));
    assert_eq!(f("-1").div_int_rounded(3, Rounding::TowardZero), f("-0.333333333"));
}

#[test]
fn the_split_product_is_the_exact_result_rounded_once_or_none_outside_the_range() {
    // `i128::MIN / -1`, the one split that overflows, which the generator rarely draws.
    for (b, result) in [(0, Some(0)), (-1, Some(i128::MIN)), (1, None), (i128::MIN, None)] {
        assert_eq!(mul_div_raw(i128::MIN, b, -1, Rounding::Down), result, "MIN * {b} / -1");
    }
    let divisor = raw().prop_filter("a divisor", |d| *d != 0);
    check((raw(), raw(), divisor, rounding()), |(a, b, d, rounding)| {
        prop_assert_eq!(
            mul_div_raw(a, b, d, rounding),
            exact(a, b, d, rounding),
            "{} * {} / {} {:?}",
            a,
            b,
            d,
            rounding
        );
        Ok(())
    });
}

#[test]
fn every_product_and_quotient_rounds_its_exact_result() {
    check((raw(), raw(), any::<i64>(), rounding()), |(a, b, bps, rounding)| {
        let (x, y) = (Fixed(a), Fixed(b));
        if let Some(product) = exact(a, b, SCALE, rounding) {
            prop_assert_eq!(x.mul_rounded(y, rounding), Fixed(product));
        }
        if let Some(share) = exact(a, bps as i128, BPS, rounding) {
            prop_assert_eq!(x.mul_bps_rounded(bps, rounding), Fixed(share));
        }
        if b != 0 {
            if let Some(quotient) = exact(a, SCALE, b, rounding) {
                prop_assert_eq!(x.div_rounded(y, rounding), Fixed(quotient));
            }
            let saturated = if (a < 0) == (b < 0) { i64::MAX } else { i64::MIN };
            let bps = exact(a, BPS, b, Rounding::TowardZero)
                .map_or(saturated, |bps| bps.clamp(i64::MIN as i128, i64::MAX as i128) as i64);
            prop_assert_eq!(x.bps_of(y), bps);
        }
        Ok(())
    });
}

#[test]
fn serde_as_string() {
    let json = serde_json::to_string(&f("1.25")).unwrap();
    assert_eq!(json, "\"1.25\"");
    assert_eq!(serde_json::from_str::<Fixed>("\"1.25\"").unwrap(), f("1.25"));
    assert_eq!(serde_json::from_str::<Fixed>("3").unwrap(), f("3"));
    assert!(serde_json::from_str::<Fixed>("1.5").is_err());
}

// ── Helpers ──

/// The exact `a * b / d` from its full 256-bit product, rounded as `rounding` says, or `None`
/// outside the `i128` range.
fn exact(a: i128, b: i128, d: i128, rounding: Rounding) -> Option<i128> {
    let wide = |x: i128| I256::try_from(x).expect("an i128 fits I256");
    let (product, divisor) = (wide(a) * wide(b), wide(d));
    let (quotient, remainder) = (product / divisor, product % divisor);
    let non_negative = product.is_negative() == divisor.is_negative();
    let rounded = match rounding {
        Rounding::Down if !remainder.is_zero() && !non_negative => quotient - I256::ONE,
        Rounding::Up if !remainder.is_zero() && non_negative => quotient + I256::ONE,
        _ => quotient,
    };
    i128::try_from(rounded).ok()
}

/// A raw value of either sign with a magnitude of any bit length up to 127, or one of the range's
/// edges and the engine's scales.
fn raw() -> impl Strategy<Value = i128> {
    prop_oneof![
        6 => (0u32..=127, any::<i128>()).prop_map(|(bits, x)| x >> (127 - bits)),
        1 => select(vec![i128::MIN, i128::MIN + 1, -SCALE, -1, 0, 1, BPS, SCALE, i128::MAX]),
    ]
}

/// Any rounding direction.
fn rounding() -> impl Strategy<Value = Rounding> {
    select(vec![Rounding::Down, Rounding::Up, Rounding::TowardZero])
}

/// Runs `test` over 16,384 cases from a fixed seed, so every run checks the same ones.
fn check<S: Strategy>(strategy: S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let config =
        Config { cases: 16_384, rng_seed: RngSeed::Fixed(0x5eed_0085), failure_persistence: None, ..Config::default() };
    if let Err(failure) = TestRunner::new(config).run(&strategy, test) {
        panic!("{failure}");
    }
}
