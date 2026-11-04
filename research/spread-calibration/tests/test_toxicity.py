import numpy as np
import pytest

from calib import toxicity


def test_fills_to_tier_has_a_floor_of_min_fills():
    assert toxicity.fills_to_tier(3.0, 4.0) == toxicity.MIN_FILLS
    assert toxicity.fills_to_tier(0.5, 4.0) == pytest.approx(64.0)
    assert toxicity.fills_to_tier(0.0, 4.0) == float("inf")


def test_an_edgeless_normal_account_rarely_reaches_the_first_tier():
    rng = np.random.default_rng(1)
    reach = toxicity.honest_reach(rng.standard_normal(100_000), 200, rng, accounts=2000)
    assert reach["4.0"] < 0.01
    assert reach["8.0"] == 0.0


def test_an_account_with_edge_reaches_the_tier():
    rng = np.random.default_rng(2)
    reach = toxicity.honest_reach(rng.standard_normal(100_000) + 1.0, 200, rng, accounts=500)
    assert reach["4.0"] == 1.0


def test_dilution_size_is_bounded_by_the_weight_cap_and_its_cost_grows_with_the_edge():
    low = toxicity.dilution_cost_share(1.0, 100.0, 2.0, 2.0, 4.0)
    high = toxicity.dilution_cost_share(2.0, 100.0, 2.0, 2.0, 4.0)
    assert high["share"] > low["share"]
    cap = toxicity.WEIGHT_CAP
    k = np.log(2) / toxicity.HALF_LIFE_DAYS * (1.0 * 100.0 / (np.log(2) / toxicity.HALF_LIFE_DAYS) / 4.0) ** 2
    assert low["size_ratio"] == pytest.approx(cap / 2 + np.sqrt(cap * cap / 4 + (cap - 1) * k / 100.0))


def test_hour_sigma_is_the_rms_of_horizon_changes():
    step = 0.05
    k = int(round(toxicity.HORIZON_S / step))
    x = np.arange(1000) * 1e-5
    assert toxicity.hour_sigma(x, step, 0, 1000) == pytest.approx(k * 1e-5)
