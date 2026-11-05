"""The calibration's probability estimates and floor choice."""

import numpy as np
import pytest

from model import experiments


def test_the_upper_bound_is_the_exact_one_sided_95_percent_bound():
    # No failure in 2,000 paths: 1 - 0.05 ** (1 / 2,000), about the rule of three's 3 / 2,000.
    assert experiments.upper_bound(0, 2000) == pytest.approx(1 - 0.05 ** (1 / 2000), rel=1e-6)
    # One failure: the rate at which one or none happens 5% of the time, about 4.74 / 2,000.
    bound = experiments.upper_bound(1, 2000)
    assert (1 - bound) ** 2000 + 2000 * bound * (1 - bound) ** 1999 == pytest.approx(0.05, rel=1e-6)
    assert experiments.upper_bound(20, 2000) == pytest.approx(0.0144, abs=2e-4)
    assert experiments.upper_bound(5, 5) == 1.0


def sweep(points):
    return [{"floor": f, "p": p, "refused_share_directional": r} for f, p, r in points]


def test_a_level_takes_the_largest_floor_before_the_first_miss():
    rows = sweep([(0, 0.0, 0.3), (1, 0.001, 0.2), (2, 0.02, 0.1), (3, 0.0, 0.0)])
    assert experiments.floor_at_level(rows, "p", 0.01) == 1
    assert experiments.floor_at_level(sweep([(0, 0.5, 0.3), (1, 0.6, 0.0)]), "p", 0.01) == 0


def test_a_level_never_sets_a_floor_past_the_binding_point():
    # Refusals end at a floor of 2, so a level every floor meets stops there.
    rows = sweep([(0, 0.0, 0.3), (1, 0.0, 0.2), (2, 0.0, 0.005), (3, 0.0, 0.0)])
    assert experiments.floor_at_level(rows, "p", 0.01) == 2


def test_a_target_takes_the_candidate_meeting_it_that_refuses_least():
    rows = [{"total_floor": f, "p": p, "refused_share_directional": r} for f, p, r in ((0, 0.0, 1.0), (10, 0.0005, 0.4), (20, 0.003, 0.3), (30, 0.0, 0.2), (40, 0.0, 0.2))]
    assert experiments.chosen(rows, "p", 0.001)["total_floor"] == 30
    assert experiments.chosen(rows, "p", 0.01)["total_floor"] == 30
    assert experiments.chosen([{"total_floor": 0, "p": 0.5, "refused_share_directional": 0.0}], "p", 0.01) is None


def test_a_lane_where_nothing_opened_cannot_be_insolvent():
    n = 2000
    out = {k: np.zeros((n, 1)) for k in ("nav_end", "refused", "refused_hedged", "desired", "desired_directional", "refused_week1", "desired_week1", "fee", "financing", "bad_debt", "mint_basis", "max_queue", "max_gross_exposure")}
    out |= {"min_nav": np.full((n, 1), np.inf), "min_cash": np.full((n, 1), np.inf), "ever_negative": np.zeros((n, 1), bool), "queued": np.zeros((n, 1), bool), "first_negative_h": np.full((n, 1), -1)}
    stats = experiments.lane_stats(out, 0)
    assert stats["p_ever"] == stats["p_short"] == stats["p_ever_upper"] == stats["p_short_upper"] == 0.0


def test_a_lane_reports_what_its_paths_count():
    n = 2000
    out = {k: np.zeros((n, 1)) for k in ("nav_end", "refused", "refused_hedged", "desired", "desired_directional", "refused_week1", "desired_week1", "fee", "financing", "bad_debt", "mint_basis", "max_queue", "max_gross_exposure")}
    out |= {"min_nav": np.ones((n, 1)), "min_cash": np.ones((n, 1)), "ever_negative": np.zeros((n, 1), bool), "queued": np.zeros((n, 1), bool), "first_negative_h": np.full((n, 1), -1)}
    out["min_cash"][:3] = -1.0
    out["ever_negative"][:7] = True
    stats = experiments.lane_stats(out, 0)
    assert (stats["ever_paths"], stats["p_ever"], stats["short_paths"], stats["p_short"]) == (7, 7 / n, 3, 3 / n)
    assert stats["p_ever_upper"] == pytest.approx(experiments.upper_bound(7, n))


def test_the_study_inputs_give_btc_and_eth_the_genesis_100x_bands_and_a_flow_profile():
    from run import inputs

    _, _, _, flow, markets = inputs()
    for symbol in ("BTC-USD", "ETH-USD"):
        i = markets.symbols.index(symbol)
        assert markets.margin_rate[i] == pytest.approx(0.01)
        assert all((kind, "capped", markets.schedule[i]) in flow.sizes for kind in ("directional", "hedged"))
