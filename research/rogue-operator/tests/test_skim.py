import itertools

import numpy as np
import pytest

from model import skim


def brute_schedule(x, at, side, size, lag, delay):
    best = -np.inf
    windows = [range(a - lag, a + delay + 1) for a in at]
    for moments in itertools.product(*windows):
        if any(b < a for a, b in zip(moments, moments[1:])):
            continue
        best = max(best, sum(s * q * (x[u] - x[a]) for u, a, s, q in zip(moments, at, side, size)))
    return best


def test_bound_takes_each_orders_worst_index_in_its_window():
    x = np.array([0.0, 3.0, -2.0, 1.0, 0.0, 5.0, 0.0])
    at = np.array([3])
    # A buy at step 3 with a lag of 2 and no delay: the highest of steps 1..3 is 3.
    assert skim.bound(x, at, np.array([1.0]), lag=2, delay=0)[0] == pytest.approx(2.0)
    # A sell: the lowest of 1..3 is -2.
    assert skim.bound(x, at, np.array([-1.0]), lag=2, delay=0)[0] == pytest.approx(3.0)
    # With two steps of delay a buy reaches step 5.
    assert skim.bound(x, at, np.array([1.0]), lag=2, delay=2)[0] == pytest.approx(4.0)


def test_schedule_is_the_best_monotone_assignment():
    rng = np.random.default_rng(7)
    for _ in range(30):
        x = np.cumsum(rng.normal(0, 1, 30))
        at = np.sort(rng.integers(4, 24, size=4))
        side = rng.choice([-1.0, 1.0], size=4)
        size = rng.uniform(1, 3, size=4)
        gains = skim.schedule(x, at, side, size, lag=3, delay=1)
        assert (size * gains).sum() == pytest.approx(brute_schedule(x, at, side, size, 3, 1))


def test_schedule_never_beats_the_bound_and_meets_it_for_one_order():
    rng = np.random.default_rng(11)
    x = np.cumsum(rng.normal(0, 1, 200))
    at = np.sort(rng.integers(10, 180, size=40))
    side = rng.choice([-1.0, 1.0], size=40)
    size = np.ones(40)
    assert skim.schedule(x, at, side, size, 5, 0).sum() <= skim.bound(x, at, side, 5, 0).sum() + 1e-9
    one = at[:1]
    assert skim.schedule(x, one, side[:1], size[:1], 5, 0)[0] == pytest.approx(skim.bound(x, one, side[:1], 5, 0)[0])


def test_causal_waits_the_delay_and_looks_back_the_lag():
    x = np.array([0.0, 0.0, 9.0, 0.0, 0.0, 4.0, 2.0])
    # A buy at step 1, delay 4, lag 2: executes at step 5 with the best of 3..5, which is 4,
    # missing the 9 at step 2 that only hindsight could take.
    assert skim.causal(x, np.array([1]), np.array([1.0]), lag=2, delay=4)[0] == pytest.approx(4.0)
    assert skim.bound(x, np.array([2]), np.array([1.0]), lag=1, delay=4)[0] == pytest.approx(0.0)


def test_arrivals_are_poisson_and_inside_the_window():
    rng = np.random.default_rng(0)
    at, side = skim.arrivals(rng, rate_per_s=2.0, start=100, length=72_000, step_s=0.05)
    assert at.min() >= 100 and at.max() < 72_100
    assert len(at) == pytest.approx(7200, rel=0.05)
    assert (np.diff(at) >= 0).all()
    assert abs((side > 0).mean() - 0.5) < 0.03
