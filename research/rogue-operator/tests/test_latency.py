import itertools

import numpy as np
import pytest

from model import latency


def brute_force(r, c):
    """The best total over every path of positions, knowing every return."""
    best = -np.inf
    for path in itertools.product((-1.0, 0.0, 1.0), repeat=len(r)):
        pos = np.array(path)
        traded = np.abs(np.diff(np.concatenate([[0.0], pos])))
        best = max(best, float((pos * r).sum() - (c * traded).sum()))
    return best


def path_value(pos, r, c):
    traded = np.abs(np.diff(np.concatenate([[0.0], pos])))
    return float((pos * r).sum() - (c * traded).sum())


def test_without_spread_the_policy_takes_every_move():
    r = np.array([0.001, -0.002, 0.0005, -0.0001, 0.003])
    pos = latency.policy(r, np.zeros(len(r)), horizon=1)
    assert list(pos) == [1.0, -1.0, 1.0, -1.0, 1.0]
    out = latency.settle(pos, r, np.zeros(len(r)))
    assert out["gross"] == pytest.approx(np.abs(r).sum())
    # Every position wins, so the fee is a tenth of the whole.
    assert out["pnl"] == pytest.approx(0.9 * np.abs(r).sum())


def test_full_lookahead_matches_the_brute_force_optimum():
    rng = np.random.default_rng(3)
    for _ in range(20):
        r = rng.normal(0, 1e-4, 7)
        c = np.full(7, 0.6e-4)
        # Knowing the whole remaining path, the first decision is optimal, and so is the rest.
        pos = latency.policy(r, c, horizon=7)
        assert path_value(pos, r, c) == pytest.approx(brute_force(r, c), abs=1e-12)


def test_a_spread_wider_than_every_known_move_keeps_the_operator_flat():
    r = np.array([1e-4, -1e-4, 1e-4, -1e-4])
    pos = latency.policy(r, np.full(4, 2e-4), horizon=2)
    assert not pos.any()


def test_no_lookahead_means_no_trading():
    r = np.array([1e-3, 1e-3])
    assert not latency.policy(r, np.zeros(2), horizon=0).any()


def test_settle_charges_spread_on_both_fills_and_fee_only_on_winners():
    r = np.array([10e-4, 0.0, -5e-4, 0.0])
    pos = np.array([1.0, 0.0, 1.0, 0.0])
    c = np.full(4, 1e-4)
    out = latency.settle(pos, r, c)
    # First position: +10 bps less 2 bps of spread = +8, fee 0.8. Second: -5 less 2 = -7, no fee.
    assert out["spread"] == pytest.approx(4e-4)
    assert out["fee"] == pytest.approx(0.8e-4)
    assert out["pnl"] == pytest.approx(8e-4 - 0.8e-4 - 7e-4)
    assert out["positions"] == 2
