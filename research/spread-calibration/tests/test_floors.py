import numpy as np
import pytest

from calib import floors


def test_a_measured_market_scales_back_to_its_measured_spread():
    sigma = {"BTC-USD": np.array([0.004, 0.006, 0.012])}
    ratio = floors.rho({"BTC-USD": 1.67}, sigma)["BTC-USD"]
    assert floors.year_spread_bps(sigma["BTC-USD"], ratio) == pytest.approx(1.67)


def test_the_year_spread_is_the_rms_of_the_hours_brownian_moves():
    # One hour of 1% Parkinson volatility moves 1% * sqrt(5 / 3600) in root mean square over 5 s.
    assert floors.year_spread_bps(np.array([0.01]), 1.0) == pytest.approx(1e4 * 0.01 * np.sqrt(5 / 3600))
    assert floors.year_spread_bps(np.array([0.01, 0.03]), 1.0) == pytest.approx(1e4 * np.sqrt(0.0005) * np.sqrt(5 / 3600))


def test_the_floor_is_the_year_rounded_up_to_the_hundredth():
    assert floors.floor_bps(0.271) == 0.28
    assert floors.floor_bps(1.68) == 1.68
    assert floors.floor_bps(1.68, 0.5) == 0.84


def test_trailing_scores_never_exceed_the_hours_own_units_and_the_floor_case_divides_by_the_floor():
    sigma = np.array([0.002, 0.01, 0.05])
    s = 1.0 * sigma * np.sqrt(5 / 3600) * 1e4
    out = floors.scales(sigma, 1.0, 1.0)
    assert out["trailing"] == pytest.approx(s / np.maximum(s, 1.0))
    assert (out["trailing"] <= 1.0).all()
    assert out["at_floor"] == pytest.approx(s)


def test_honest_reach_rises_when_scores_are_inflated():
    rng = np.random.default_rng(1)
    z0 = rng.standard_normal(100_000)
    plain = floors.honest_reach(z0, np.ones(10), rng, accounts=2000, windows=(200,))["200"]["4.0"]
    doubled = floors.honest_reach(z0, np.full(10, 2.0), rng, accounts=2000, windows=(200,))["200"]["4.0"]
    assert doubled > plain


def test_a_higher_floor_lowers_the_latency_edge_only_in_hours_quieter_than_it():
    hours = [(1.0, 1e-4, np.full(10, 2e-4)), (1.0, 4e-4, np.full(10, 8e-4))]
    assert floors.latency_edge(hours, 0.0)["edge_z"] == pytest.approx(2.0)
    assert floors.latency_edge(hours, 2e-4)["edge_z"] == pytest.approx((1.0 + 2.0) / 2)


def test_the_genesis_lines_write_each_floor_as_a_decimal_of_bps():
    text = floors.toml_lines([{"symbol": "EUR-USD", "source": "scaled", "year_bps": 0.271, "floor_bps": 0.14}])
    assert 'markout_floor_bps = "0.14"' in text and "# EUR-USD" in text
