"""The toxicity calibration: fill-size and markout distributions, memory and prior from the
drift and climb bounds, and the tier thresholds and spreads from the benign false-positive
bound, all on the engine's exact notional-weighted recurrence."""

import numpy as np
import pytest

from model import prices, toxicity


def test_averaged_folds_one_fill_toward_the_markout():
    score, history = toxicity.averaged(0.0, 100.0, 10.0, 100.0, 1_000.0)
    assert score == pytest.approx(5.0)
    assert history == 200.0
    # a markout below the score still moves the score toward it, and history caps at memory
    score, history = toxicity.averaged(5.0, 1_000.0, 1.0, 100.0, 1_000.0)
    assert 1.0 < score < 5.0
    assert history == 1_000.0


def test_sigma_markout_scales_the_median_hour_to_the_horizon():
    bars = np.full((prices.WEEK, 1, 4), prices.CLOSED, dtype=np.int32)
    log_range = np.log(1.001)  # every open hour has a 10 bp high-low range
    for hour in range(prices.WEEK):
        bars[hour, 0] = [0, round(log_range * 1e6), 0, 0]
    sigma = toxicity.sigma_markout_bps(bars, horizon_ms=2000)
    stored_range = round(log_range * 1e6) / 1e6  # the bars carry micro-log units
    parkinson_hour = stored_range / np.sqrt(4 * np.log(2))
    assert sigma[0] == pytest.approx(parkinson_hour / np.sqrt(3600) * np.sqrt(2) * 1e4, rel=1e-9)


def test_size_quantiles_invert_the_margin_weighted_cdf():
    genesis = {"markets": [{"symbol": "M", "margin_tiers": [{"up_to": 1_000_000, "bps": 1000}, {"bps": 2000}]}]}
    sizes = {("directional", "capped", "1000@1000000,2000"): (np.array([100.0, 1e6]), np.array([0.5, 1.0]))}
    out = toxicity.size_quantiles(sizes, genesis, quantiles=(0.5, 0.9))
    assert out["M"]["n50"] == 100.0
    assert out["M"]["n90"] == 1e6  # the last bound carries the tail


def test_choose_memory_sizes_to_the_worst_market_at_p99():
    symbols = ["A", "B"]
    sigma = np.array([1.0, 2.0])  # bps
    sizes = {"A": {"n99": 1e6}, "B": {"n99": 1e6}}
    choice = toxicity.choose_memory(sigma, symbols, sizes, drift_bps=1.0)
    assert choice["memory_usd"] == pytest.approx(4e6)  # B: 1e6 * (2/1)^2
    assert choice["worst_market"] == "B"


def test_drift_bound_is_sigma_over_the_effective_sample():
    assert toxicity.drift_bound_sigma(2.0, 4e6, 4e6) == pytest.approx(2.0)


def test_climb_score_is_monotone_and_bounded_by_the_markout():
    score_10 = toxicity.climb_score(2.0, 100_000.0, 500_000.0, 5_000_000.0, 10)
    score_50 = toxicity.climb_score(2.0, 100_000.0, 500_000.0, 5_000_000.0, 50)
    assert 0.0 < score_10 < score_50 < 2.0


def test_benign_cross_probability_is_zero_without_noise():
    p = toxicity.benign_cross_probability(0.0, 100_000.0, 500_000.0, 5_000_000.0, 1.0, 200, 1, 2_000)
    assert p == 0.0


def test_choose_tiers_reaches_the_highest_passing_threshold():
    symbols = ["A"]
    sigma = np.array([0.1])  # a quiet market: the benign bound passes everywhere
    sizes = {"A": {"n50": 50_000.0, "n75": 50_000.0, "n99": 50_000.0}}
    memory, prior = 5_000_000.0, 2_500_000.0
    # enough trials that zero crossings bound the probability under BENIGN_FP at 99% confidence
    choice = toxicity.choose_tiers(sigma, symbols, sizes, memory, prior, seed=3, trials=8_000)
    reach = toxicity.climb_score(toxicity.M_TOX_BPS, 50_000.0, prior, memory, choice["climb_fills"])
    assert choice["tier1_bps"] == int(reach)
    assert choice["tier2_bps"] == choice["tier1_bps"] * 2
    # the spread is the markout that reaches the tier in the stated fills
    assert toxicity.climb_score(choice["spread1_bps"], 50_000.0, prior, memory, choice["climb_fills"]) >= choice["tier1_bps"]


def test_markout_to_reach_lands_on_the_target():
    m = toxicity.markout_to_reach(1.0, 100_000.0, 500_000.0, 5_000_000.0, 100)
    assert toxicity.climb_score(m, 100_000.0, 500_000.0, 5_000_000.0, 100) >= 1.0
    assert toxicity.climb_score(m - 0.2, 100_000.0, 500_000.0, 5_000_000.0, 100) < 1.0


def test_benign_cross_probability_is_positive_with_noise_and_a_low_threshold():
    p = toxicity.benign_cross_probability(5.0, 100_000.0, 500_000.0, 5_000_000.0, 0.5, 200, 7, 4_000)
    assert 0.0 < p < 1.0


def test_correlated_fills_cross_at_least_as_often_as_spaced_ones():
    # Overlapping horizons move markouts together: fills half a second apart cross no less often
    # than fills a horizon apart, on the same seeded paths.
    args = (3.0, 100_000.0, 500_000.0, 5_000_000.0, 1.0, 100)
    tight = toxicity.correlated_cross_probability(*args, seed=5, trials=4_000, cadence_s=0.5)
    spaced = toxicity.correlated_cross_probability(*args, seed=5, trials=4_000, cadence_s=2.0)
    assert tight >= spaced - 0.02


def test_choose_tiers_gates_on_the_benign_bound(monkeypatch):
    # On this noisy market no threshold in the toxic reach passes the independent-trials bound
    # (the stated bound is for fills at least a horizon apart): the choice must raise, not stand.
    monkeypatch.setattr(toxicity, "M_TOX_BPS", 3.0)
    monkeypatch.setattr(toxicity, "CLIMB_CANDIDATES", (75,))  # fix the count: no escaping upward
    symbols = ["NOISY"]
    sigma = np.array([5.0])  # crosses 2 bps often enough to violate the bound at the reach
    sizes = {"NOISY": {"n50": 100_000.0, "n75": 100_000.0, "n99": 100_000.0}}
    with pytest.raises(ValueError):
        toxicity.choose_tiers(sigma, symbols, sizes, 5_000_000.0, 2_500_000.0, seed=11, trials=40_000)


def test_choose_tiers_reports_the_burst_sensitivity():
    symbols = ["MKT"]
    sigma = np.array([1.0])
    sizes = {"MKT": {"n50": 100_000.0, "n75": 100_000.0, "n99": 100_000.0}}
    choice = toxicity.choose_tiers(sigma, symbols, sizes, 5_000_000.0, 2_500_000.0, seed=17, trials=8_000)
    assert set(choice["burst_sensitivity"]) == {"1.0", "0.5"}
    assert all(0.0 <= p <= 1.0 for p in choice["burst_sensitivity"].values())


def test_fills_a_horizon_apart_do_not_overlap():
    # At cadence equal to the horizon, markouts share no path increments: the crossing
    # probability matches the independent-trials bound within Monte Carlo tolerance.
    args = (2.0, 100_000.0, 500_000.0, 5_000_000.0, 1.0, 100)
    iid = toxicity.benign_cross_probability(*args, seed=23, trials=8_000)
    spaced = toxicity.correlated_cross_probability(*args, seed=23, trials=8_000, cadence_s=2.0)
    assert abs(iid - spaced) <= 0.02


def test_choose_tiers_raises_when_no_threshold_passes():
    symbols = ["NOISY"]
    sigma = np.array([20.0])  # crossings are near-certain at any threshold in the reach
    sizes = {"NOISY": {"n50": 100_000.0, "n75": 100_000.0, "n99": 100_000.0}}
    with pytest.raises(ValueError):
        toxicity.choose_tiers(sigma, symbols, sizes, 5_000_000.0, 4_500_000.0, seed=13, trials=2_000)


def test_the_calibration_refuses_a_market_without_sizes():
    genesis = {"markets": [{"symbol": "A", "margin_tiers": [{"bps": 1000}]}, {"symbol": "B", "margin_tiers": [{"bps": 1000}]}]}
    with pytest.raises(ValueError) as err:
        toxicity.require_coverage(["A", "B"], {"A": {}}, genesis)
    assert "B" in str(err.value)


def test_averaged_rounds_up_at_the_engine_scale():
    # a step of 1.5e-9 rounds up to 2e-9 at the engine's nine decimals, not 1.5e-9
    score, _ = toxicity.averaged(0.0, 1.0, 3e-9, 1.0, 1_000.0)
    assert score == pytest.approx(2e-9)


def test_overlapping_markouts_share_path_increments():
    # deterministic path: unit increments. At half-second cadence the horizon is 8 ticks and
    # fills start 2 ticks apart, so consecutive markouts share 6 of 8 increments; at a
    # horizon's cadence they share none.
    trials, fills = 4_000, 20
    fine = toxicity.correlated_markouts(np.random.default_rng(0), 1.0, fills, trials, cadence_s=0.5)
    spaced = toxicity.correlated_markouts(np.random.default_rng(0), 1.0, fills, trials, cadence_s=2.0)
    fine_corr = np.corrcoef(fine[:, 1:], fine[:, :-1], rowvar=False)[fills - 1, 0]
    spaced_corr = np.corrcoef(spaced[:, 1:], spaced[:, :-1], rowvar=False)[fills - 1, 0]
    assert fine_corr > 0.5  # consecutive markouts share 6 of 8 increments
    assert abs(spaced_corr) < 0.1  # a horizon apart shares none
