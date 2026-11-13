import numpy as np
import pytest

from trigger import honest, paths
from trigger.paths import BINS_PER_HOUR

BTC_MARGIN = [{"up_to": "2000000", "bps": 100}, {"up_to": "10000000", "bps": 200}, {"bps": 500}]
BTC_SPREAD = [(2e6, 2e-4), (10e6, 4e-4), (np.inf, 8e-4)]


def flow(spread=BTC_SPREAD, classes=()):
    sizes = honest.Sizes(lo=np.array([1e5]), hi=np.array([1e6]), prob=np.array([1.0]))
    return honest.MarketFlow("BTC-USD", BTC_MARGIN, spread, tuple(classes), {"directional": sizes, "hedged": sizes})


def test_staircase_charges_each_slice_its_band():
    got = honest.staircase(np.array([1e6, 6e6, 20e6]), BTC_SPREAD)
    assert got == pytest.approx([2e-4, (400 + 1600) / 6e6, (400 + 3200 + 8000) / 20e6])
    assert honest.margin_for(np.array([1e6, 20e6]), BTC_MARGIN) == pytest.approx([1e4, 2e4 + 1.6e5 + 5e5])


def test_lognormal_hold_matches_the_class_moments():
    mu, sigma = honest.lognormal_hold(0.0766, 0.19719)
    h = np.exp(np.random.default_rng(1).normal(mu, sigma, 2_000_000))
    assert h.mean() / 3600 == pytest.approx(0.0766, rel=0.03)
    assert np.sqrt(h / 3600).mean() == pytest.approx(0.19719, rel=0.01)


def test_count_sizes_weighs_each_bin_by_margin_share_over_margin():
    bounds = np.array([1.0, 1e6, 2e7])
    s = honest.count_sizes(bounds, np.array([0.0, 0.5, 1.0]), BTC_MARGIN)
    per_margin = 0.5 / honest.margin_for(np.sqrt(bounds[:-1] * bounds[1:]), BTC_MARGIN)
    assert s.prob == pytest.approx(per_margin / per_margin.sum())
    assert s.draw(np.random.default_rng(2), 1000).max() <= 2e7


def one_position(x, side, hold_bins, notional=1e6, fee=0.1, opened=0):
    p = paths.from_array(np.asarray(x, dtype=float))
    out = honest.settle(p, paths.Crossings(p), np.array([opened]), np.array([hold_bins]), np.array([notional]), np.array([float(side)]),
                        flow(), np.array([100.0]), lambda h: np.full(np.shape(h), fee), paths.next_open(p.is_open))
    return [v[0] for v in out]


def test_a_winning_close_pays_both_spreads_and_the_fee():
    x = np.zeros(BINS_PER_HOUR)
    x[5:] = np.log(1.01)
    _, closed, _, client, loss, bad = one_position(x, 1, 10)
    assert closed == 10 and client == 1e6
    assert loss == pytest.approx((1e6 * 0.01 - 2 * 2e-4 * 1e6) * 0.9)
    assert bad == 0.0


def test_a_close_out_loses_at_most_the_balance_and_the_rest_is_bad_debt():
    x = np.zeros(BINS_PER_HOUR)
    x[5:] = np.log(1 - 0.02)  # a gap past the whole 1% margin
    _, closed, _, _, loss, bad = one_position(x, 1, 100)
    assert closed == 5
    assert loss == pytest.approx(-1e4)  # the house collects the balance, no fee on a loss
    assert bad == pytest.approx(1e6 * 0.02 + 400 - 1e4)


def test_a_close_out_fires_at_half_the_margin_less_the_opening_spread():
    adverse = 0.5 * 1e4 / 1e6 - 2e-4
    x = np.zeros(BINS_PER_HOUR)
    x[3] = np.log(1 - adverse * 0.99)
    x[4:] = np.log(1 - adverse * 1.01)
    _, closed, *_ = one_position(x, 1, 100)
    assert closed == 4
    _, closed, *_ = one_position(-x, -1, 100)  # the mirror image for a short
    assert closed == 4


def test_a_pair_ends_when_either_leg_is_closed_out():
    x = np.zeros(BINS_PER_HOUR)
    x[7:] = np.log(1 - 0.02)
    p = paths.from_array(x)
    closed, long_leg, short_leg, bad = honest.settle_pairs(p, paths.Crossings(p), np.array([0]), np.array([100]), np.array([1e6]),
                                                           flow(), np.array([100.0]), lambda h: np.full(np.shape(h), 0.1),
                                                           paths.next_open(p.is_open))
    assert closed[0] == 7
    assert long_leg[0] == pytest.approx(-1e4)
    assert short_leg[0] == pytest.approx((1e6 * 0.02 - 400) * 0.9)
    assert bad[0] == pytest.approx(1e6 * 0.02 + 400 - 1e4)


def test_a_close_planned_in_a_closed_spell_waits_for_the_open():
    x = np.zeros(2 * BINS_PER_HOUR)
    is_open = np.ones(2 * BINS_PER_HOUR, dtype=bool)
    is_open[50:BINS_PER_HOUR] = False
    p = paths.from_array(x, is_open)
    out = honest.settle(p, paths.Crossings(p), np.array([0]), np.array([60]), np.array([1e6]), np.array([1.0]), flow(),
                        np.array([100.0]), lambda h: np.full(np.shape(h), 0.1), paths.next_open(p.is_open))
    assert out[1][0] == BINS_PER_HOUR


def test_on_a_flat_path_the_house_collects_every_spread_and_marks_nothing():
    hours = 48
    x = np.zeros(hours * BINS_PER_HOUR)
    p = paths.from_array(x)
    classes = [honest.FlowClass("directional", 2000.0, np.log(600.0), 0.5, 1.0, 3600.0, 0.5, 100.0),
               honest.FlowClass("hedged", 1000.0, np.log(9000.0), 0.3, 3600.0, np.inf, 0.5, 100.0)]
    mf = flow(spread=[(np.inf, 3e-4)], classes=classes)
    out = honest.simulate(p, mf, np.zeros(hours), lambda h: np.full(np.shape(h), 0.1), np.random.default_rng(3))
    assert out.gross.sum() == 0.0 and out.bad.sum() == 0.0
    assert np.allclose(out.marked(x), out.net)
    # Every leg pays 3 bps on each fill: the notional settled is the house's take over 6 bps, which
    # is the classes' rate over the hours positions had to close in.
    settled = -out.net.sum() / 6e-4
    assert settled == pytest.approx(3000.0 * hours * 3600, rel=0.08)
    assert np.cumsum(out.exposure)[-1] == pytest.approx(0.0, abs=1e-3)
