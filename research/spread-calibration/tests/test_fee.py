import math

import numpy as np
import pytest

from calib import fee


def test_family_holds_its_peak_then_falls_log_linearly_to_the_standard_fee():
    s = fee.family(1.0, 1800.0, 300.0)
    assert float(fee.fee_at(s, 10.0)) == 1.0
    assert float(fee.fee_at(s, 300.0)) == 1.0
    assert float(fee.fee_at(s, 1800.0)) == pytest.approx(0.10)
    assert float(fee.fee_at(s, 7200.0)) == pytest.approx(0.10)
    mid = math.sqrt(300.0 * 1800.0)
    assert float(fee.fee_at(s, mid)) == pytest.approx(0.55)


def test_family_at_the_standard_fee_is_flat():
    s = fee.family(fee.BASE, 1800.0)
    assert np.allclose(fee.fee_at(s, [0.5, 60.0, 1e5]), fee.BASE)


def test_required_is_the_fee_that_brings_the_best_theta_to_the_budget():
    # positive 10, negative 4 per unit a year; with no budget the fee must take 60% of the gains.
    year = {(5.0, 1.0): (10.0, 4.0), (5.0, 2.0): (2.0, 1.9)}
    assert fee.required(year, 1.0, 0.0)[5.0] == pytest.approx(0.6)
    # A budget a day relaxes it: 365 * budget / size of slack.
    assert fee.required(year, 1.0, 1.0 / 365)[5.0] == pytest.approx(0.5)


def test_envelope_falls_with_the_hold_and_rounds_up_to_five_percent():
    out = fee.envelope({1.0: 0.31, 10.0: 0.42, 60.0: 0.12}, {1.0: 0.2, 10.0: 0.1, 60.0: 0.11})
    assert out == pytest.approx({1.0: 0.45, 10.0: 0.45, 60.0: 0.15})


def test_honest_cost_counts_only_the_holds_asked_for():
    rows = [{"hold_from_s": "0", "hold_to_s": "900", "notional_share": "0.5"},
            {"hold_from_s": "900", "hold_to_s": "3600", "notional_share": "0.5"}]
    s = fee.family(1.0, 900.0, 60.0)
    assert fee.honest_cost(rows, s, from_s=900.0)["extra_fee_share_of_profit"] == pytest.approx(0.0, abs=1e-12)
    short = fee.honest_cost(rows, s, to_s=900.0)
    assert short["share_of_profit_over_base"] > 0.9
    assert 0 < short["extra_fee_share_of_profit"] < 0.9


def test_an_add_moves_the_open_time_to_the_size_weighted_average():
    c = fee.HoldClock()
    c.add(1.0, 0.0)
    c.add(1.0, 100.0)
    assert c.opened == pytest.approx(50.0)
    assert c.close(-1.0, 150.0) == pytest.approx(100.0)
    assert c.opened == pytest.approx(50.0)  # a partial close keeps the open time


def test_a_flip_closes_the_position_at_its_hold_and_opens_the_rest_now():
    c = fee.HoldClock()
    c.add(2.0, 10.0)
    assert c.trade(-5.0, 70.0) == pytest.approx(60.0)
    assert (c.qty, c.opened) == (-3.0, 70.0)
    assert c.trade(-1.0, 80.0) is None
    with pytest.raises(ValueError):
        c.close(4.0 + 1.0, 90.0)


def test_slicing_an_aged_position_pays_at_the_slices_steady_age():
    c = fee.HoldClock()
    c.add(10.0, 0.0)
    holds = []
    for k in range(1, 400):
        holds.append(c.close(-1.0, 10.0 * k))
        c.add(1.0, 10.0 * k)
    assert holds[-1] == pytest.approx(fee.gamed_age(10.0, 10.0), rel=1e-6)
