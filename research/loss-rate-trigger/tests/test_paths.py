import numpy as np

from trigger import paths
from trigger.paths import BINS_PER_HOUR, CLOSED


def bars(rows):
    return np.array(rows, dtype=np.int32)


def test_bridge_starts_and_ends_at_zero_with_the_hour_variance():
    rng = np.random.default_rng(1)
    b = paths.bridge(BINS_PER_HOUR, np.full(4000, 0.01), rng)
    assert np.all(b[:, 0] == 0)
    # The last bin's value is one step from the end, where the bridge is pinned at zero.
    assert np.abs(b[:, -1]).mean() < 0.002
    # A Brownian bridge's variance at its midpoint is a quarter of the walk's over the span.
    assert abs(b[:, BINS_PER_HOUR // 2].var() / 0.01**2 - 0.25) < 0.03


def test_hours_join_at_their_open_and_close_and_a_closed_hour_holds():
    # Hour 0 opens at 0 and closes at +1%; hour 1 is closed; hour 2 gaps +0.5% and closes -0.2%.
    m = bars([[0, 12000, -3000, 10000], [CLOSED, CLOSED, CLOSED, CLOSED], [5000, 4000, -6000, -2000]])
    p = paths.from_bars(m, np.random.default_rng(2), demean=False)
    x = p.x.reshape(3, BINS_PER_HOUR)
    assert x[0, 0] == 0.0
    assert np.all(x[1] == x[1, 0]) and abs(x[1, 0] - 0.01) < 1e-12
    assert abs(x[2, 0] - 0.015) < 1e-12
    assert not p.is_open[BINS_PER_HOUR : 2 * BINS_PER_HOUR].any() and p.is_open[:BINS_PER_HOUR].all()
    assert np.allclose(p.hour_min, x.min(axis=1)) and np.allclose(p.hour_max, x.max(axis=1))


def test_demeaning_takes_the_mean_open_hour_return_off_each_gap():
    m = bars([[0, 2000, 0, 1000], [0, 2000, 0, 1000], [CLOSED] * 4, [0, 2000, 0, 1000]])
    p = paths.from_bars(m, np.random.default_rng(3))
    x = p.x.reshape(4, BINS_PER_HOUR)
    # Each open hour returns 0.1%, so each demeaned gap is -0.1% and every hour opens where the first did.
    assert abs(x[1, 0] - x[0, 0]) < 1e-12 and abs(x[3, 0] - x[0, 0]) < 1e-12 and abs(x[0, 0] + 0.001) < 1e-12


def test_next_open_skips_closed_bins():
    is_open = np.array([True, False, False, True, False])
    assert paths.next_open(is_open).tolist() == [0, 3, 3, 3, 5]


def brute_first(x, start, stop, level, below):
    for b in range(start + 1, stop + 1):
        if (x[b] <= level) if below else (x[b] >= level):
            return b
    return -1


def test_first_crossing_matches_a_scan_of_every_bin():
    rng = np.random.default_rng(4)
    x = np.cumsum(rng.standard_normal(BINS_PER_HOUR * 40)) * 1e-4
    p = paths.from_array(x)
    cross = paths.Crossings(p)
    n = 3000
    start = rng.integers(0, len(x) - 2, n)
    stop = np.minimum(start + rng.integers(1, BINS_PER_HOUR * 12, n), len(x) - 1)
    below = rng.random(n) < 0.5
    level = x[start] + np.where(below, -1, 1) * rng.uniform(0, 0.03, n)
    got = cross.first(start, stop, level, below, batch=257)
    want = np.array([brute_first(x, s, e, lv, b) for s, e, lv, b in zip(start, stop, level, below)])
    assert np.array_equal(got, want)
    assert (got >= 0).any() and (got < 0).any()
