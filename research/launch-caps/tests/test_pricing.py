"""The launch pricing bundle per volume tier against hand cases from the genesis."""

import numpy as np
import pytest

from caps import pricing
from caps.common import genesis, use

use("cap")

BTC = next(m for m in genesis()["markets"] if m["symbol"] == "BTC-USD")


@pytest.mark.parametrize("tier, bands", [
    (0, ((5e5, 2.0), (1e7, 4.0), (None, 8.0))),  # a quarter of the $2M first bound
    (1, ((1e6, 2.0), (1e7, 4.0), (None, 8.0))),
    (2, ((2e6, 2.0), (1e7, 4.0), (None, 8.0))),
    (3, ((5e6, 2.0), (1e7, 4.0), (None, 8.0))),  # two and a half times it, under the second bound
])
def test_a_tier_moves_the_first_spread_bound_and_no_rate(tier, bands):
    assert pricing.spread_bands(BTC, tier) == bands


def test_the_first_band_ends_at_most_at_the_second_bound():
    spec = {"spread_tiers": [{"up_to": "2000000", "tenth_bps": 20}, {"up_to": "3000000", "tenth_bps": 40}, {"tenth_bps": 80}]}
    assert pricing.spread_bands(spec, 3) == ((3e6, 2.0), (None, 8.0))


def test_tenths_of_a_bp_price_fx_below_one_bp():
    eur = next(m for m in genesis()["markets"] if m["symbol"] == "EUR-USD")
    assert pricing.spread_bands(eur, 2)[0] == (1e7, 0.4)


@pytest.mark.parametrize("tier, hold_s, fee", [(0, 5.0, 1.0), (0, 10.0, 1.0), (0, 900.0, 0.10), (2, 3600.0, 0.09), (3, 900.0, 0.08)])
def test_the_fee_holds_the_full_fee_to_10s_and_the_discounted_base_from_15_minutes(tier, hold_s, fee):
    schedule = pricing.fee_schedule(tier, genesis()["params"])
    assert float(schedule.bps(hold_s)) / 1e4 == pytest.approx(fee)
    # Just short of 15 minutes every tier still pays the curve, at or above the base.
    assert float(schedule.bps(899.0)) / 1e4 >= 0.10


def test_the_fee_falls_with_the_hold_between_its_points():
    schedule = pricing.fee_schedule(0, genesis()["params"])
    holds = np.geomspace(1.0, 3600.0, 400)
    fees = schedule.bps(holds)
    assert (np.diff(fees) <= 0).all()
    assert 0.10 < float(schedule.bps(60.0)) / 1e4 < 1.0


@pytest.mark.parametrize("tiers, floor, out", [
    (BTC["margin_tiers"], 500, [{"bps": 500}]),  # 20x where BTC-USD allows 100x
    (BTC["margin_tiers"], 200, [{"up_to": "10000000", "bps": 200}, {"bps": 500}]),
    ([{"up_to": "500000", "bps": 1000}, {"bps": 2000}], 500, [{"up_to": "500000", "bps": 1000}, {"bps": 2000}]),  # a 10x share is unchanged
])
def test_a_tiers_floor_raises_the_initial_bands(tiers, floor, out):
    assert pricing.initial_bands(tiers, floor) == out


def test_no_genesis_band_reaches_its_close_spread_ceiling_at_level_0():
    widest = pricing.check_ceilings(genesis())
    assert widest["BTC-USD"] == (8.0, 30)
    bad = genesis()
    bad["close_spread_ceilings"] = [dict(c, max_close_spread_bps=8) if c["market"] == BTC["id"] else c for c in bad["close_spread_ceilings"]]
    with pytest.raises(ValueError):
        pricing.check_ceilings(bad)


@pytest.mark.parametrize("leverage", [np.full(10, 50.0), np.full(10, 12.0), np.array([5.0, 15.0, 25.0, 45.0]), np.array([2.0, 30.0])])
def test_the_straddling_bins_leverage_lies_within_its_bounds(leverage):
    # g(c) = E[min(L, c)] for a bin up to 50x, its 10x value and its mean known: the estimate is the
    # midpoint of bounds that hold the true value, exact when every position sits at the bin's top.
    truth = np.minimum(leverage, 20.0).mean()
    source, mean = np.minimum(leverage, 10.0).mean(), leverage.mean()
    est = pricing.straddling_leverage(source, 10.0, 20.0, 50.0, mean)
    lower = source + (mean - source) * 10 / 40
    upper = min(2 * source, mean, 20.0)
    assert lower - 1e-12 <= truth <= upper + 1e-12
    assert est == pytest.approx((lower + upper) / 2)
    if (leverage == 50).all():
        assert est == pytest.approx(20.0)


def test_a_flat_schedule_caps_each_bins_leverage_and_scales_its_sizes():
    from model.flow import Flow, FlowClass

    source = [{"up_to": "1000000", "bps": 1000}, {"bps": 2000}]
    src = "1000@1000000,2000"
    bounds, shares = np.array([1e3, 1e5, 1e6, 2e6]), np.array([0.2, 0.6, 0.9, 1.0])
    flow = Flow(classes=[FlowClass(k, 1, b, 1 / 6, 1.0, 1.0, 0.5) for k in ("directional", "hedged") for b in range(3)],
                leverage={(k, "capped", src): np.array([8.0, 10.0, 10.0]) for k in ("directional", "hedged")},
                daily_margin=np.array([1.0]), sizes={(k, "capped", src): (bounds, shares) for k in ("directional", "hedged")},
                long_share_ar1=0.0, long_share_resid_sd=0.0, long_share_slope=0.0)
    means = {(k, label): mean for k in ("directional", "hedged") for label, mean in (("1-50", 28.0), ("51-300", 160.0), ("301-1000", 570.0))}
    out = pricing.with_flat_schedule(flow, 500, source, means, ["1-50", "51-300", "301-1000"])
    lev = out.leverage[("directional", "capped", "500")]
    assert lev[1] == lev[2] == 20.0
    assert lev[0] == pytest.approx(pricing.straddling_leverage(8.0, 10.0, 20.0, 50.0, 28.0))
    scale = (lev.sum()) / 28.0
    new_bounds, new_shares = out.sizes[("directional", "capped", "500")]
    assert new_bounds[:3] == pytest.approx(bounds[:3] * scale)
    # Past the 10x schedule's $1M bound an order's margin is what bought it there, 100k + 1M * 20%.
    assert new_bounds[3] == pytest.approx(20 * (1e5 + 1e6 * 0.2))
    assert (new_shares == shares).all()
