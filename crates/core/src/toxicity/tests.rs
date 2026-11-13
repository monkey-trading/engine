use super::*;
use proptest::prelude::*;

#[test]
fn a_tier_applies_from_its_score_exactly() {
    let tiers = launch_score_tiers();
    // E = 4, W2 = 1: a score of exactly 4 reaches the first tier, a raw unit less does not.
    let at = ToxicityScore { edge: f("4"), weight_squares: f("1"), fills: f("20"), ..ToxicityScore::default() };
    assert_eq!(at.tier(&tiers), Some(0));
    let under = ToxicityScore { edge: f("3.999999999"), ..at.clone() };
    assert_eq!(under.tier(&tiers), None);
    // E = 8000, W2 = 1e6: exactly 8 reaches the second.
    let top = ToxicityScore { edge: f("8000"), weight_squares: f("1000000"), ..at.clone() };
    assert_eq!(top.tier(&tiers), Some(1));
    assert_eq!(top.score(), f("8"));
}

#[test]
fn no_tier_before_the_fill_minimum_with_no_edge_or_with_no_weight() {
    let tiers = launch_score_tiers();
    let toxic = ToxicityScore { edge: f("100"), weight_squares: f("1"), fills: f("20"), ..ToxicityScore::default() };
    assert_eq!(toxic.tier(&tiers), Some(1));
    let short = ToxicityScore { fills: f("19.999999999"), ..toxic.clone() };
    assert_eq!(short.tier(&tiers), None);
    let losing = ToxicityScore { edge: f("-100"), ..toxic.clone() };
    assert_eq!(losing.tier(&tiers), None);
    let weightless = ToxicityScore { weight_squares: Fixed::ZERO, ..toxic.clone() };
    assert_eq!(weightless.tier(&tiers), None);
    assert_eq!(weightless.score(), Fixed::ZERO);
    assert_eq!(toxic.tier(&[]), None);
}

#[test]
fn a_weight_is_capped_at_four_times_the_decayed_mean_fill() {
    let mut score = ToxicityScore::default();
    // The first fill weighs its whole notional.
    score.fold(f("100"), Fixed::ONE, f("1"));
    assert_eq!((score.edge, score.weight_squares), (f("100"), f("10000")));
    // A fill ten times the mean weighs four times it, but its notional joins the mean in full.
    score.fold(f("1000"), Fixed::ONE, f("1"));
    assert_eq!((score.edge, score.weight_squares), (f("500"), f("170000")));
    assert_eq!((score.fills, score.notional), (f("2"), f("1100")));
    // The mean is now 550, so a fill of 1000 weighs all of it.
    score.fold(f("1000"), Fixed::ONE, f("-0.5"));
    assert_eq!((score.edge, score.weight_squares), (f("0"), f("1170000")));
}

#[test]
fn a_partly_matched_fill_counts_its_unmatched_share() {
    let mut score = ToxicityScore::default();
    score.fold(f("50"), f("0.5"), f("2"));
    assert_eq!(
        (score.fills, score.notional, score.edge, score.weight_squares),
        (f("0.5"), f("50"), f("100"), f("2500"))
    );
}

#[test]
fn the_sums_decay_together_by_the_hours_crossed() {
    let score = ToxicityScore { edge: f("-8"), weight_squares: f("64"), fills: f("20"), notional: f("2000"), step: 10 };
    let week = score.decayed_to((10 + 168) * decay::STEP_MS + 1);
    assert_eq!(
        week,
        ToxicityScore { edge: f("-4"), weight_squares: f("32"), fills: f("10"), notional: f("1000"), step: 178 }
    );
    // Within the same hour nothing decays.
    assert_eq!(score.decayed_to(10 * decay::STEP_MS + 3_599_999), score);
    // A time before the step reached, as a corporate catch-up stamps, decays nothing either.
    assert_eq!(score.decayed_to(0), score);
}

#[test]
fn score_tiers_validate_their_order_scores_and_add_ons() {
    assert!(validate_score_tiers(&launch_score_tiers()).is_ok());
    assert!(validate_score_tiers(&[]).is_ok());
    let tier = |score: &str, bps| ScoreTier { min_score: f(score), extra_spread_bps: bps };
    for bad in [
        vec![tier("8", 5), tier("4", 2)],
        vec![tier("4", 2), tier("4", 5)],
        vec![tier("0", 2)],
        vec![tier("-1", 2)],
        vec![tier("1000.000000001", 2)],
        vec![tier("4", 0)],
        vec![tier("4", 10_001)],
    ] {
        assert!(validate_score_tiers(&bad).is_err(), "{bad:?}");
    }
    assert!(validate_score_tiers(&[tier("1000", 10_000)]).is_ok());
}

#[test]
fn a_listing_with_no_floor_takes_its_classs_at_activation() {
    use crate::types::AssetClass::*;
    for (classes, floor) in
        [(vec![Fx, Bond, Rates], "0.1"), (vec![Index], "0.5"), (vec![Commodity, Equity, Etf, Crypto], "1")]
    {
        for class in classes {
            assert_eq!(default_markout_floor_bps(class), f(floor), "{class:?}");
            assert!(MARKOUT_FLOOR_BPS.contains(&default_markout_floor_bps(class)));
        }
    }
}

#[test]
fn the_integer_root_rounds_down() {
    for (n, root) in [(0u128, 0u128), (1, 1), (3, 1), (4, 2), (99, 9), (100, 10), (u128::MAX, u64::MAX as u128)] {
        assert_eq!(isqrt(U256::from(n)), U256::from(root), "{n}");
    }
    assert_eq!(isqrt(U256::MAX), U256::from(u128::MAX));
}

proptest! {
    /// The root is the largest whole number whose square is at most the value.
    #[test]
    fn the_integer_root_is_the_floor(raw in any::<u128>(), shift in 0usize..128) {
        let n = U256::from(raw) << shift;
        let root = isqrt(n);
        prop_assert!(root * root <= n);
        let next = root + U256::ONE;
        prop_assert!(next * next > n);
    }
}

// ── Helpers ──

fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}
