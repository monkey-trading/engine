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
    choice = toxicity.choose_tiers(sigma, symbols, sizes, memory, prior, seed=3, trials=4_000)
    reach = toxicity.climb_score(toxicity.M_TOX_BPS, 50_000.0, prior, memory, choice["climb_fills"])
    assert choice["tier1_bps"] == int(reach)
    assert choice["tier2_bps"] == choice["tier1_bps"] * 2
    # the spread is the markout that reaches the tier in the stated fills
    assert toxicity.climb_score(choice["spread1_bps"], 50_000.0, prior, memory, choice["climb_fills"]) >= choice["tier1_bps"]


def test_markout_to_reach_lands_on_the_target():
    m = toxicity.markout_to_reach(1.0, 100_000.0, 500_000.0, 5_000_000.0, 100)
    assert toxicity.climb_score(m, 100_000.0, 500_000.0, 5_000_000.0, 100) >= 1.0
    assert toxicity.climb_score(m - 0.2, 100_000.0, 500_000.0, 5_000_000.0, 100) < 1.0
