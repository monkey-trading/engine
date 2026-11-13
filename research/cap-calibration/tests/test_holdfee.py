"""The expected profit fee of a class of short holds, with the spread inside the gain."""

import math

import numpy as np
import pytest

from model import accounting, holdfee, simulate

D3 = ((1.0, 1.0), (2.0, 0.9083), (4.0, 0.8166), (7.5, 0.7335), (15.0, 0.6418), (30.0, 0.55), (60.0, 0.4583),
      (120.0, 0.3666), (240.0, 0.2749), (450.0, 0.1918), (900.0, 0.10))
FEE = accounting.HoldFee(points=D3, discount=0.2)
FLAT = accounting.HoldFee(points=((1.0, 0.1),))


def test_psi_is_the_expected_excess_of_a_standard_normal():
    assert float(holdfee.psi(0.0)) == pytest.approx(simulate.GAIN_PER_SD)
    z = np.linspace(-12, 12, 400_001)
    density = np.exp(-z * z / 2) / math.sqrt(2 * math.pi)
    for u in (0.1, 0.5, 2.0):
        assert float(holdfee.psi(u)) == pytest.approx(float(np.trapezoid(np.maximum(z - u, 0) * density, z)), rel=1e-6)


def test_one_hold_at_no_spread_and_a_flat_fee_is_the_gain_the_model_charged_before():
    # A class whose mean root's square is its mean holds every position at the mean.
    table = holdfee.bound_table(FLAT, 0.25, 0.5)
    sigma = np.array([0.01, 0.02])
    expected = 0.1 * simulate.GAIN_PER_SD * sigma * 0.5
    assert holdfee.unit_fee(table, sigma, 0.0) == pytest.approx(expected)
    assert np.array_equal(holdfee.bound_table(FLAT, 0.25, 0.5), holdfee.central_table(FLAT, ((0.0, 3600.0, 1.0),), 0.25, 0.5))


def test_a_spread_takes_the_fee_off_the_gain_it_eats():
    # Two bps a fill on a 15-minute hold with a 1.2% hourly sigma: the fee is on E[max(gain - 2 s, 0)].
    table = holdfee.bound_table(FLAT, 0.25, 0.5)
    sigma, s = 0.012, 2e-4
    expected = 0.1 * sigma * 0.5 * float(holdfee.psi(2 * s / (sigma * 0.5)))
    assert float(holdfee.unit_fee(table, sigma, 2 * s)) == pytest.approx(expected, rel=1e-4)
    assert float(holdfee.unit_fee(table, 0.0, 2 * s)) == 0.0  # no variation, no gain


def test_every_bound_support_has_the_class_moments():
    grid = holdfee.hold_grid()
    idx, p = holdfee.bound_supports(grid, 0.0308, 0.13064)
    h = grid / 3600
    assert p.sum(axis=1) == pytest.approx(np.ones(len(p)))
    assert (p * h[idx]).sum(axis=1) == pytest.approx(np.full(len(p), 0.0308))
    assert (p * np.sqrt(h[idx])).sum(axis=1) == pytest.approx(np.full(len(p), 0.13064))


def test_the_bound_is_at_most_the_fee_of_any_distribution_with_the_moments():
    # Two-hold distributions on the bound's grid with known moments: none pays less than the bound.
    grid = holdfee.hold_grid()
    rng = np.random.default_rng(1)
    v = np.array([0.0, 0.02, 0.2])
    checked = 0
    while checked < 12:
        i, j = sorted(rng.choice(len(grid), size=2, replace=False))
        p = rng.uniform(0.05, 0.95)
        h = np.array([grid[i], grid[j]])
        m1 = float(p * h[0] / 3600 + (1 - p) * h[1] / 3600)
        m2 = float(p * np.sqrt(h[0] / 3600) + (1 - p) * np.sqrt(h[1] / 3600))
        if holdfee.single_hold(m1, m2):
            continue
        exact = holdfee.weights(FEE, h, v) @ np.array([p, 1 - p])
        bound = holdfee.unit_fee(holdfee.bound_table(FEE, m1, m2), 1.0, v)
        assert (bound <= exact * (1 + 2e-3) + 1e-12).all()
        checked += 1


def test_the_central_estimate_has_the_moments_and_sits_above_the_bound():
    holding = ((0.0, 10.0, 0.066), (10.0, 30.0, 0.198), (30.0, 60.0, 0.151), (60.0, 300.0, 0.285), (300.0, 900.0, 0.134),
               (900.0, 3600.0, 0.108), (3600.0, 14400.0, 0.044))
    holds, prior = holdfee.profile_holds(holding)
    q = holdfee.tilt(holds, prior, 0.0308, 0.13064)
    assert q @ (holds / 3600) == pytest.approx(0.0308, rel=1e-6)
    assert q @ np.sqrt(holds / 3600) == pytest.approx(0.13064, rel=1e-6)
    bound = holdfee.bound_table(FEE, 0.0308, 0.13064)
    central = holdfee.central_table(FEE, holding, 0.0308, 0.13064)
    assert (central >= bound - 1e-12).all()


def test_a_class_held_past_the_curve_pays_the_discounted_base_on_every_hold():
    # The target flow's kept class: holds of 15 minutes to an hour, where only the base applies.
    lo, hi = 0.25, 1.0
    m1 = (hi - lo) / math.log(hi / lo)
    m2 = 2 * (math.sqrt(hi) - math.sqrt(lo)) / math.log(hi / lo)
    bound = holdfee.bound_table(FEE, m1, m2, min_hold_s=900.0)
    assert bound[0] == pytest.approx(0.08 * m2 * simulate.GAIN_PER_SD, rel=1e-9)
