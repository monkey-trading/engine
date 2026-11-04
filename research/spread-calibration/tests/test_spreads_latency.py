import numpy as np

from calib import latency, spreads


def test_the_best_tier_rounds_up_to_whole_bps_and_is_at_least_one():
    assert spreads.best_tier_bps(0.2, 0.18) == 1
    assert spreads.best_tier_bps(1.18, 0.18) == 2
    assert spreads.best_tier_bps(0.0, 0.0) == 1


def test_positions_do_not_overlap():
    t = np.arange(0, 100, 5)
    eligible = np.ones(len(t), dtype=bool)
    taken = latency.trades(t, eligible, 12)
    starts = t[taken]
    assert list(starts) == [0, 15, 30, 45, 60, 75, 90]
