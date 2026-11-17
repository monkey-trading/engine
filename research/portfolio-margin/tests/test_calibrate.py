import numpy as np

from pm import calibrate, stats


def test_fit_recovers_loadings_that_average_one():
    rng = np.random.default_rng(1)
    f = rng.standard_t(4, 20000) * 0.01
    beta = np.array([0.5, 1.0, 1.5])
    r = np.outer(f, beta) + rng.normal(0, 0.002, (20000, 3))
    _, b, e = calibrate.fit(r)
    assert abs(b.mean() - 1) < 0.02
    assert np.allclose(b / b.mean(), beta, atol=0.05)
    assert np.allclose(e.mean(axis=1), 0, atol=1e-12)  # the factor is the members' mean, so the residuals average to zero


def test_shocks_round_up_to_a_tenth_of_a_basis_point():
    assert calibrate.up(0.00011) == 0.00011
    assert calibrate.up(0.000101) > 0.000101
    assert abs(calibrate.up(0.000101) - 0.00011) < 1e-12


def test_coexceed_is_the_tail_share_when_independent_and_one_when_equal():
    rng = np.random.default_rng(2)
    a, b = rng.normal(size=50000), rng.normal(size=50000)
    assert abs(stats.coexceed(a, b, 0.05) - 0.05) < 0.02
    assert stats.coexceed(a, a, 0.05) == 1.0


def test_weighted_quantile():
    x, w = np.array([1.0, 2.0, 3.0]), np.array([1.0, 1.0, 8.0])
    assert stats.wquantile(x, w, 0.5) == 3.0
    assert stats.wquantile(x, np.ones(3), 0.5) == 2.0
