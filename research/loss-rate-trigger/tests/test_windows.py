import numpy as np

from trigger import windows


def test_trailing_sums_the_window_ending_at_each_bin():
    s = np.array([1.0, 2.0, 3.0, 4.0, 5.0])
    assert windows.trailing(s, 2).tolist() == [1.0, 3.0, 5.0, 7.0, 9.0]
    assert windows.trailing(s, 10).tolist() == [1.0, 3.0, 6.0, 10.0, 15.0]


def test_hourly_max_takes_each_whole_hour():
    r = np.array([1.0, 5.0, 2.0, 0.0, 7.0, 3.0, 9.0])
    assert windows.hourly_max(r, 3).tolist() == [5.0, 7.0]


def test_the_level_rises_at_once_and_falls_only_after_the_cooldown_once_below():
    reading = np.array([0, 0, 5, 0, 0, 0, 0, 0, 0, 0], dtype=float)
    assert windows.raised(reading, 5.0, 3).astype(int).tolist() == [0, 0, 1, 1, 1, 0, 0, 0, 0, 0]
    # Still over the threshold when the cooldown ends: it stays raised until the reading falls.
    reading = np.array([0, 5, 5, 5, 5, 5, 0, 0, 5, 0], dtype=float)
    assert windows.raised(reading, 5.0, 2).astype(int).tolist() == [0, 1, 1, 1, 1, 1, 0, 0, 1, 1]
    # A dip under the threshold inside the cooldown does not lower it.
    reading = np.array([5, 0, 5, 0, 0, 0], dtype=float)
    assert windows.raised(reading, 5.0, 3).astype(int).tolist() == [1, 1, 1, 0, 0, 0]


def test_the_attackers_room_per_window_is_the_threshold_less_honest_loss():
    honest = np.array([1.0, -2.0, 3.0, 4.0, -10.0, 0.0])
    assert windows.disjoint_room(honest, 2, 5.0).tolist() == [6.0, 0.0, 15.0]


def test_strata_split_the_open_hours_by_volatility_rank():
    vol = np.arange(200, dtype=float)[::-1]
    is_open = np.ones(200, dtype=bool)
    is_open[0] = False
    s = windows.strata(vol, is_open)
    assert s[0] == -1
    counts = np.bincount(s[s >= 0], minlength=5)
    assert counts.tolist() == [100, 59, 30, 8, 2]
    assert s[1] == 4 and s[-1] == 0
