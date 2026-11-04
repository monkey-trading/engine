import numpy as np
import pytest

from model import flow, stats


def test_a_constant_measure_totals_exactly():
    values = {0: np.full(5, 2.0), 1: np.full(3, 2.0)}
    population = {0: 6000, 1: 2760}
    est = stats.estimate(values, population, year_hours=8760)
    assert est.year == pytest.approx(2.0 * 8760)
    assert est.year_lo == pytest.approx(est.year) and est.year_hi == pytest.approx(est.year)
    assert est.day == pytest.approx(48.0)


def test_strata_are_weighted_by_their_hours_not_their_samples():
    # Ten quiet hours worth 1 stand for 8000, two busy ones worth 100 for 760.
    values = {0: np.ones(10), 1: np.full(2, 100.0)}
    est = stats.estimate(values, {0: 8000, 1: 760}, year_hours=8760)
    assert est.year == pytest.approx(8000 + 76_000)


def test_missing_hours_scale_up_to_a_year():
    est = stats.estimate({0: np.ones(4)}, {0: 8755}, year_hours=8755)
    assert est.year == pytest.approx(8760)


def test_the_interval_covers_the_point_and_widens_with_spread():
    rng = np.random.default_rng(1)
    tight = stats.estimate({0: 10 + rng.normal(0, 0.1, 12)}, {0: 8760}, 8760)
    wide = stats.estimate({0: 10 + rng.normal(0, 5, 12)}, {0: 8760}, 8760)
    assert tight.year_lo <= tight.year <= tight.year_hi
    assert (wide.year_hi - wide.year_lo) > 10 * (tight.year_hi - tight.year_lo)


def test_banded_margin_charges_each_slice_its_rate():
    bands = flow.BANDS["BTC-USD"]
    assert flow.margin_for(np.array([1_000_000.0]), bands)[0] == pytest.approx(10_000)
    # $2M at 1% then $3M at 2%.
    assert flow.margin_for(np.array([5_000_000.0]), bands)[0] == pytest.approx(20_000 + 60_000)


def test_launch_flow_reads_the_cap_study():
    flows = flow.market_flows()
    btc = flows["BTC-USD"]
    # The steady daily margin is the mean of the flow model's last seven full days.
    assert flow.steady_daily_margin() == pytest.approx(181_667_278, rel=1e-6)
    assert btc.fill_notional_per_day == pytest.approx(2 * btc.opening_notional_per_day)
    assert btc.fill_weights.sum() == pytest.approx(1.0)
    # Ten times the flow is ten times the notional.
    assert flow.market_flows(10.0)["BTC-USD"].fill_notional_per_day == pytest.approx(10 * btc.fill_notional_per_day)
