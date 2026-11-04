import numpy as np

from model.paths import window_max, window_min


def test_window_extremes_match_the_direct_computation():
    rng = np.random.default_rng(5)
    for n, width in ((1, 0), (10, 3), (97, 10), (500, 499), (1000, 37)):
        x = rng.normal(size=n)
        want_max = np.array([x[i : i + width + 1].max() for i in range(n - width)])
        want_min = np.array([x[i : i + width + 1].min() for i in range(n - width)])
        assert np.array_equal(window_max(x, width), want_max)
        assert np.array_equal(window_min(x, width), want_min)
