//! Deterministic fixed-point arithmetic.
//!
//! Every price, quantity and cash amount in the engine is a [`Fixed`]: an `i128` scaled by 10^9.
//! Floats never touch engine state, so replaying the log on any machine gives identical results.
//! Every product and quotient is the exact result rounded once: `mul`, `div`, `mul_bps`, and
//! `div_int` truncate toward zero, and `mul_div` and the `_rounded` methods round in the direction
//! the caller names. No intermediate overflows where the result fits, so only a result outside
//! `Fixed`'s range overflows; release builds keep overflow checks on, so that halts the engine
//! rather than corrupting state. External input is bounded by [`Fixed::MAX_INPUT`] first.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::str::FromStr;

use alloy_primitives::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DECIMALS: u32 = 9;
const SCALE: i128 = 10i128.pow(DECIMALS);
const BPS: i128 = 10_000;

/// The internal representation of [`Fixed`]'s `Deserialize` input: a decimal string or an integer.
#[derive(Deserialize)]
#[serde(untagged)]
enum FixedRepr {
    Str(String),
    Int(i64),
}

/// Which way [`Fixed::mul_div`] and the `_rounded` methods round a result that falls between two
/// representable values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    /// Toward negative infinity.
    Down,
    /// Toward positive infinity.
    Up,
    /// Toward zero, as `mul`, `div`, and `mul_bps` round.
    TowardZero,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fixed(i128);

#[derive(Debug, thiserror::Error)]
#[error("invalid fixed-point number {0:?}")]
pub struct ParseFixedError(String);

impl Add for Fixed {
    type Output = Fixed;

    /// Exact addition.
    fn add(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 + rhs.0)
    }
}

impl Sub for Fixed {
    type Output = Fixed;

    /// Exact subtraction.
    fn sub(self, rhs: Fixed) -> Fixed {
        Fixed(self.0 - rhs.0)
    }
}

impl Neg for Fixed {
    type Output = Fixed;

    /// Exact negation.
    fn neg(self) -> Fixed {
        Fixed(-self.0)
    }
}

impl AddAssign for Fixed {
    /// Exact in-place addition.
    fn add_assign(&mut self, rhs: Fixed) {
        self.0 += rhs.0;
    }
}

impl SubAssign for Fixed {
    /// Exact in-place subtraction.
    fn sub_assign(&mut self, rhs: Fixed) {
        self.0 -= rhs.0;
    }
}

impl Sum for Fixed {
    /// Exact sum of an iterator of values.
    fn sum<I: Iterator<Item = Fixed>>(iter: I) -> Fixed {
        iter.fold(Fixed::ZERO, Add::add)
    }
}

impl fmt::Display for Fixed {
    /// Shortest exact decimal form: no trailing zeros, no exponent.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let int = abs / SCALE as u128;
        let frac = abs % SCALE as u128;
        if frac == 0 {
            return write!(f, "{sign}{int}");
        }
        let frac = format!("{frac:0width$}", width = DECIMALS as usize);
        write!(f, "{sign}{int}.{}", frac.trim_end_matches('0'))
    }
}

impl fmt::Debug for Fixed {
    /// Same as `Display`, so test failures read as numbers.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Fixed {
    type Err = ParseFixedError;

    /// Parses an optionally negative decimal with at most nine fractional digits.
    /// Error cases:
    /// - empty input, or no digits around the point
    /// - a character other than an ASCII digit, one leading `-`, or one `.`
    /// - more than nine fractional digits
    /// - a value outside the `i128` range at this scale
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseFixedError(s.to_owned());
        let (neg, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int, frac) = body.split_once('.').unwrap_or((body, ""));
        if int.is_empty() && frac.is_empty() {
            return Err(err());
        }
        if frac.len() > DECIMALS as usize {
            return Err(err());
        }
        let digits = |p: &str| p.is_empty() || p.bytes().all(|b| b.is_ascii_digit());
        if !digits(int) || !digits(frac) {
            return Err(err());
        }
        let int: i128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| err())? };
        let frac_raw: i128 = if frac.is_empty() {
            0
        } else {
            frac.parse::<i128>().map_err(|_| err())? * 10i128.pow(DECIMALS - frac.len() as u32)
        };
        let raw = int.checked_mul(SCALE).and_then(|v| v.checked_add(frac_raw)).ok_or_else(err)?;
        Ok(Fixed(if neg { -raw } else { raw }))
    }
}

impl Serialize for Fixed {
    /// Serializes as a decimal string so no JSON client loses precision.
    /// Error cases:
    /// - the serializer rejects a string
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fixed {
    /// Accepts a decimal string or a whole integer.
    /// Error cases:
    /// - neither a string nor an integer
    /// - a string that does not parse as a `Fixed`
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match FixedRepr::deserialize(d)? {
            FixedRepr::Str(s) => s.parse().map_err(serde::de::Error::custom),
            FixedRepr::Int(n) => Ok(Fixed::from_int(n)),
        }
    }
}

impl Fixed {
    pub const ZERO: Fixed = Fixed(0);
    pub const ONE: Fixed = Fixed(SCALE);
    /// Largest magnitude accepted for any externally supplied number (qty, price, amount), so each
    /// stays far inside the `i128` range at this scale. Positions built from many such numbers can
    /// grow past it, and products and quotients on them stay in range wherever their result fits.
    pub const MAX_INPUT: Fixed = Fixed(1_000_000_000 * SCALE);

    /// A value from its raw scaled representation.
    pub const fn from_raw(raw: i128) -> Self {
        Fixed(raw)
    }

    /// The raw scaled representation.
    pub const fn raw(self) -> i128 {
        self.0
    }

    /// A whole number.
    pub const fn from_int(n: i64) -> Self {
        Fixed(n as i128 * SCALE)
    }

    /// `bps` basis points as a fraction, e.g. `from_bps(50)` is 0.005.
    pub const fn from_bps(bps: i64) -> Self {
        Fixed(bps as i128 * SCALE / BPS)
    }

    // Named methods rather than `Mul`/`Div` impls, so truncation is visible at every call site.
    /// Product, truncated toward zero. The larger factor is split at the scale, so the full product
    /// never forms and only a result outside `Fixed`'s range overflows.
    #[allow(clippy::should_implement_trait)]
    pub fn mul(self, rhs: Fixed) -> Fixed {
        self.mul_rounded(rhs, Rounding::TowardZero)
    }

    /// Quotient, truncated toward zero. The dividend is split at the divisor, so only a result outside
    /// `Fixed`'s range overflows. Panics on a zero divisor, which callers rule out first.
    #[allow(clippy::should_implement_trait)]
    pub fn div(self, rhs: Fixed) -> Fixed {
        self.div_rounded(rhs, Rounding::TowardZero)
    }

    /// Product, rounded as `rounding` says. The larger factor is split at the scale, so the full
    /// product never forms and only a result outside `Fixed`'s range overflows.
    pub fn mul_rounded(self, rhs: Fixed, rounding: Rounding) -> Fixed {
        let (big, small) =
            if self.0.unsigned_abs() >= rhs.0.unsigned_abs() { (self.0, rhs.0) } else { (rhs.0, self.0) };
        Fixed(in_range(mul_div_raw(big, small, SCALE, rounding)))
    }

    /// Quotient, rounded as `rounding` says. The dividend is split at the divisor, so only a result
    /// outside `Fixed`'s range overflows. Panics on a zero divisor, which callers rule out first.
    pub fn div_rounded(self, rhs: Fixed, rounding: Rounding) -> Fixed {
        Fixed(in_range(mul_div_raw(self.0, SCALE, rhs.0, rounding)))
    }

    /// `self * num / den` from the exact product, rounded once as `rounding` says. Panics on a zero
    /// `den`, which callers rule out first.
    pub fn mul_div(self, num: Fixed, den: Fixed, rounding: Rounding) -> Fixed {
        Fixed(in_range(mul_div_raw(self.0, num.0, den.0, rounding)))
    }

    /// A checked exact-product quotient; `None` for a zero divisor or an unrepresentable result.
    pub fn checked_mul_div(self, num: Fixed, den: Fixed, rounding: Rounding) -> Option<Fixed> {
        if den.is_zero() {
            return None;
        }
        mul_div_raw(self.0, num.0, den.0, rounding).map(Fixed)
    }

    /// `bps` basis points of this value, truncated toward zero.
    pub fn mul_bps(self, bps: i64) -> Fixed {
        self.mul_bps_rounded(bps, Rounding::TowardZero)
    }

    /// `bps` basis points of this value, rounded as `rounding` says.
    pub fn mul_bps_rounded(self, bps: i64, rounding: Rounding) -> Fixed {
        Fixed(in_range(mul_div_raw(self.0, bps as i128, BPS, rounding)))
    }

    /// Product with a whole number.
    pub fn mul_int(self, n: i64) -> Fixed {
        Fixed(self.0 * n as i128)
    }

    /// Quotient by a whole number, truncated toward zero. Panics on zero, which callers rule out.
    pub fn div_int(self, n: i64) -> Fixed {
        assert!(n != 0, "fixed-point division by zero");
        Fixed(self.0 / n as i128)
    }

    /// Quotient by a whole number, rounded as `rounding` says. Panics on zero, which callers rule out.
    pub fn div_int_rounded(self, n: i64, rounding: Rounding) -> Fixed {
        Fixed(round_div(self.0, n as i128, rounding))
    }

    /// `self` relative to `base` in whole basis points, truncated, saturating at the `i64` range.
    /// Panics on a zero base, which callers rule out first.
    pub fn bps_of(self, base: Fixed) -> i64 {
        assert!(base.0 != 0, "bps_of zero base");
        match mul_div_raw(self.0, BPS, base.0, Rounding::TowardZero) {
            Some(bps) => bps.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
            // Beyond `i128`, so beyond `i64` on the side the signs give.
            None if (self.0 < 0) == (base.0 < 0) => i64::MAX,
            None => i64::MIN,
        }
    }

    /// Absolute value.
    pub fn abs(self) -> Fixed {
        Fixed(self.0.abs())
    }

    /// -1, 0 or 1.
    pub fn signum(self) -> i64 {
        self.0.signum() as i64
    }

    /// True for zero. Takes `&self` so serde's `skip_serializing_if` can call it.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// True above zero.
    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// True below zero.
    pub fn is_negative(self) -> bool {
        self.0 < 0
    }

    /// True for a value an external party may supply as a qty, price, or amount: above zero and
    /// at most [`Fixed::MAX_INPUT`].
    pub fn is_valid_input(self) -> bool {
        self.is_positive() && self <= Fixed::MAX_INPUT
    }

    /// Big-endian bytes of the raw representation, for signed digests.
    pub fn to_be_bytes(self) -> [u8; 16] {
        self.0.to_be_bytes()
    }
}

/// `a * b / d`, rounded as `rounding` says, or `None` when the result is outside the `i128` range.
/// `a` splits at `d` into `whole * d + part`, so the result is `whole * b` plus `part`'s share,
/// which is below `|b|`: only that share's product can need more than 128 bits, and it is formed
/// in 256 then. Panics on a zero `d`, which callers rule out first.
fn mul_div_raw(a: i128, b: i128, d: i128, rounding: Rounding) -> Option<i128> {
    assert!(d != 0, "fixed-point division by zero");
    let (Some(whole), part) = (a.checked_div(d), a.wrapping_rem(d)) else {
        // Only `i128::MIN / -1` overflows the split, and `b` then splits at `d` exactly.
        return if b == i128::MIN { None } else { mul_div_raw(b, a, d, rounding) };
    };
    // `whole * b` and the share have the same sign, so neither overflows unless the result does.
    let whole = whole.checked_mul(b)?;
    let share = match part.checked_mul(b) {
        Some(product) => round_div(product, d, rounding),
        None => wide_share(part, b, d, rounding)?,
    };
    whole.checked_add(share)
}

/// `part * b / d` for `|part| < |d|`, rounded as `rounding` says, from the exact 256-bit product,
/// or `None` when rounding takes it past the `i128` range.
fn wide_share(part: i128, b: i128, d: i128, rounding: Rounding) -> Option<i128> {
    let product = U256::from(part.unsigned_abs()) * U256::from(b.unsigned_abs());
    let divisor = U256::from(d.unsigned_abs());
    let negative = (part < 0) ^ (b < 0) ^ (d < 0);
    // An inexact share moves one unit away from zero when the rounding points that way.
    let away = !(product % divisor).is_zero()
        && matches!((rounding, negative), (Rounding::Down, true) | (Rounding::Up, false));
    // At most `|b|`, so at most 2^127, since `|part| < |d|`.
    let magnitude = u128::try_from(product / divisor + U256::from(u8::from(away))).expect("a share fits u128");
    if negative { 0i128.checked_sub_unsigned(magnitude) } else { 0i128.checked_add_unsigned(magnitude) }
}

/// A raw result `mul_div_raw` produced, which halts the engine when it is outside `Fixed`'s range.
fn in_range(raw: Option<i128>) -> i128 {
    raw.expect("fixed-point result out of range")
}

/// `n / d`, rounded as `rounding` says. Panics on a zero `d`, which callers rule out first.
fn round_div(n: i128, d: i128, rounding: Rounding) -> i128 {
    assert!(d != 0, "fixed-point division by zero");
    let (quotient, remainder) = (n / d, n % d);
    // Truncation leaves the exact quotient above `quotient` when the remainder has the divisor's
    // sign, and below it when the signs differ.
    let above = remainder != 0 && (remainder < 0) == (d < 0);
    match rounding {
        Rounding::Down if remainder != 0 && !above => quotient - 1,
        Rounding::Up if above => quotient + 1,
        _ => quotient,
    }
}

#[cfg(test)]
mod tests;
