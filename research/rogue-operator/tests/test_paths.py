import numpy as np

from model.paths import divergence_clamped, window_max, window_min


def test_window_extremes_match_the_direct_computation():
    rng = np.random.default_rng(5)
    for n, width in ((1, 0), (10, 3), (97, 10), (500, 499), (1000, 37)):
        x = rng.normal(size=n)
        want_max = np.array([x[i : i + width + 1].max() for i in range(n - width)])
        want_min = np.array([x[i : i + width + 1].min() for i in range(n - width)])
        assert np.array_equal(window_max(x, width), want_max)
        assert np.array_equal(window_min(x, width), want_min)


def test_a_venue_specific_print_is_held_out_of_the_index():
    x = np.log(np.array([100.0, 100.0, 98.5, 98.4, 100.1, 100.0]))  # one venue sweeps 1.5% and back
    other = np.log(np.full(6, 100.0))
    held = divergence_clamped(x, other, 25)
    assert np.allclose(np.exp(held), [100.0, 100.0, 100.0, 100.0, 100.1, 100.0])
    # Within the bound nothing changes.
    assert np.array_equal(divergence_clamped(x, x, 25), x)
