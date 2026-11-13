use super::*;
use crate::market::{Tier, staircase};

fn f(n: i64) -> Fixed {
    Fixed::from_int(n)
}

/// Bands of 0, 2 and 5bps at 1m and 5m, written in whole bps.
fn whole() -> Vec<SpreadTier> {
    vec![SpreadTier::bps(Some(f(1_000_000)), 0), SpreadTier::bps(Some(f(5_000_000)), 2), SpreadTier::bps(None, 5)]
}

#[test]
fn a_band_keeps_the_encoding_it_was_written_in() {
    // (written, read back) as the log and a signed listing carry them.
    for json in [
        r#"{"up_to":"1000000","bps":2}"#,
        r#"{"up_to":null,"bps":5}"#,
        r#"{"up_to":"2000000","tenth_bps":4}"#,
        r#"{"up_to":null,"tenth_bps":40}"#,
    ] {
        let tier: SpreadTier = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&tier).unwrap(), json);
    }
    let legacy: SpreadTier = serde_json::from_str(r#"{"up_to":"1000000","bps":2}"#).unwrap();
    assert!(legacy.is_whole_bps());
    assert_eq!(legacy.tenth_bps(), 20);
    let finer: SpreadTier = serde_json::from_str(r#"{"tenth_bps":4}"#).unwrap();
    assert_eq!((finer.up_to, finer.tenth_bps(), finer.is_whole_bps()), (None, 4, false));
}

#[test]
fn a_band_names_its_rate_exactly_once() {
    for json in [r#"{"up_to":"1"}"#, r#"{"bps":1,"tenth_bps":10}"#, r#"{"bps":1,"bsp":2}"#] {
        assert!(serde_json::from_str::<SpreadTier>(json).is_err(), "{json}");
    }
}

#[test]
fn a_whole_bps_band_charges_what_the_margin_staircase_charges_at_the_same_rates() {
    let tiers = whole();
    let margin: Vec<Tier> = tiers.iter().map(|t| Tier { up_to: t.up_to, bps: t.tenth_bps() / 10 }).collect();
    let odd = f(4_999_999) + "0.999999999".parse().unwrap();
    for (from, to) in [(f(0), f(500_000)), (f(0), f(6_000_000)), (f(4_000_000), f(6_000_000)), (odd, f(5_000_001))] {
        for rounding in [Rounding::Up, Rounding::Down, Rounding::TowardZero] {
            assert_eq!(spread_staircase(&tiers, from, to, rounding), staircase(&margin, from, to, rounding));
        }
    }
}

#[test]
fn a_band_in_tenths_charges_its_fraction_of_a_bp() {
    let tiers = [SpreadTier::tenths(Some(f(2_000_000)), 4), SpreadTier::tenths(None, 8)];
    // 0.4bp of 1m is 40; 0.4bp of 2m and 0.8bp of 1m more is 160.
    assert_eq!(spread_staircase(&tiers, f(0), f(1_000_000), Rounding::Up), f(40));
    assert_eq!(spread_staircase(&tiers, f(0), f(3_000_000), Rounding::Up), f(160));
    assert_eq!(spread_staircase(&tiers, f(3_000_000), f(1_000_000), Rounding::Up), Fixed::ZERO);
    // A slice of one unit rounds up to a unit and truncates to nothing.
    let unit = (f(1), f(1) + Fixed::from_raw(1));
    assert_eq!(spread_staircase(&tiers, unit.0, unit.1, Rounding::Up), Fixed::from_raw(1));
    assert_eq!(spread_staircase(&tiers, unit.0, unit.1, Rounding::TowardZero), Fixed::ZERO);
}

#[test]
fn rates_are_checked_in_the_unit_they_are_written_in() {
    assert!(validate_spread_tiers("t", &whole()).is_ok());
    let over_whole = [SpreadTier::bps(None, 10_001)];
    assert_eq!(validate_spread_tiers("t", &over_whole).unwrap_err(), "t: rates must be in 0..=10000 bps");
    assert!(validate_spread_tiers("t", &[SpreadTier::tenths(None, 100_000)]).is_ok());
    let over_tenths = [SpreadTier::tenths(None, 100_001)];
    assert_eq!(validate_spread_tiers("t", &over_tenths).unwrap_err(), "t: rates must be in 0..=100000 tenths of a bp");
    assert!(validate_spread_tiers("t", &[SpreadTier::tenths(None, -1)]).is_err());
    assert!(validate_spread_tiers("t", &[SpreadTier::bps(None, i64::MAX)]).is_err());
    assert!(validate_spread_tiers("t", &[]).is_err());
    let backwards = [SpreadTier::tenths(Some(f(5)), 1), SpreadTier::tenths(Some(f(2)), 1), SpreadTier::tenths(None, 1)];
    assert!(validate_spread_tiers("t", &backwards).is_err());
}

#[test]
fn the_version_decides_the_unit_and_from_v52_the_class_floor_binds() {
    let tenths = |rate| [SpreadTier::tenths(Some(f(1_000_000)), rate), SpreadTier::tenths(None, 40)];
    let (before, at) = (ProtocolVersion::V51, ProtocolVersion::V52);
    // Before `V52` either unit lists, below any floor.
    assert!(check_spread_tiers(&whole(), AssetClass::Crypto, before).is_ok());
    assert!(check_spread_tiers(&tenths(0), AssetClass::Crypto, before).is_ok());
    assert!(check_spread_tiers(&whole(), AssetClass::Crypto, at).is_err());
    // (class, floor in tenths): a band at the floor lists and one a tenth below it does not.
    for (class, floor) in [
        (AssetClass::Fx, 1),
        (AssetClass::Index, 1),
        (AssetClass::Crypto, 10),
        (AssetClass::Equity, 10),
        (AssetClass::Commodity, 10),
        (AssetClass::Etf, 10),
        (AssetClass::Bond, 10),
        (AssetClass::Rates, 10),
    ] {
        assert_eq!(spread_floor_tenth_bps(class), floor);
        assert!(check_spread_tiers(&tenths(floor), class, at).is_ok(), "{class:?}");
        let err = check_spread_tiers(&tenths(floor - 1), class, at).unwrap_err();
        assert!(err.contains("below the"), "{class:?}: {err}");
    }
}

#[test]
fn conversion_writes_tenths_and_raises_a_band_to_its_floor() {
    let converted: Vec<SpreadTier> = whole().into_iter().map(|tier| tier.converted(10)).collect();
    let rates: Vec<(i64, bool)> = converted.iter().map(|t| (t.tenth_bps(), t.is_whole_bps())).collect();
    assert_eq!(rates, [(10, false), (20, false), (50, false)]);
    assert_eq!(converted[1].up_to, Some(f(5_000_000)));
}
