"""Paired positions across session gaps on hand cases."""

import numpy as np
import pytest

from caps import gaps, pricing
from caps.common import use

use("cap")
from model.prices import CLOSED, WEEK  # noqa: E402


def test_a_closure_is_a_run_of_closed_hours_between_open_ones():
    is_open = np.array([True, True, False, False, True, False, True, True, False])
    assert gaps.closures(is_open) == [(1, 4), (4, 6)]


def test_a_gap_past_the_balance_pays_the_pair_and_is_the_houses_bad_debt():
    # A 2% gap at 1% margin, 1 bp a fill and a 10% fee: the long keeps 2% less both fills' spreads
    # less the fee on that; the short holds 1% plus its opening spread, loses it, and the 1.01% past
    # it is bad debt.
    long_leg, long_bad = gaps.leg_results(0.02, 1e-4, 1e-4, 0.01, 0.10, 0.0)
    short_leg, short_bad = gaps.leg_results(-0.02, 1e-4, 1e-4, 0.01, 0.10, 0.0)
    assert long_leg == pytest.approx(0.0198 * 0.9)
    assert long_bad == 0.0
    assert short_leg == pytest.approx(-0.0101)
    assert short_bad == pytest.approx(0.0101)
    # Inside the balance the pair pays both legs' spreads, the fee and financing.
    a, _ = gaps.leg_results(0.004, 1e-4, 1e-4, 0.01, 0.10, 1e-5)
    b, bad = gaps.leg_results(-0.004, 1e-4, 1e-4, 0.01, 0.10, 1e-5)
    assert a + b == pytest.approx(-4e-4 - 0.1 * 0.0038 - 2e-5)
    assert bad == 0.0


def bars_with_weekend(gap: float, closed: range) -> np.ndarray:
    b = np.zeros((WEEK, 2, 4), dtype=np.int32)
    b[list(closed), 0] = CLOSED
    b[closed.stop, 0, 0] = round(np.log1p(gap) * 1e6)
    return b


SPEC = {"id": 20, "asset_class": "index", "margin_tiers": [{"up_to": "5000000", "bps": 50}, {"bps": 500}],
        "spread_tiers": [{"up_to": "5000000", "tenth_bps": 7}, {"tenth_bps": 14}]}


@pytest.mark.parametrize("tier, margin", [(0, 0.05), (1, 0.02), (3, 0.005)])
def test_each_tier_holds_its_own_balance_through_the_gap(tier, margin):
    b = bars_with_weekend(-0.03, range(100, 148))
    specs = {"US500": SPEC, "BTC-USD": {"id": 1, "asset_class": "crypto"}}
    rows = gaps.pair_rows(b, ["US500", "BTC-USD"], specs, {"US500": 0.0}, {"US500": 0.0}, {tier: pricing.TIERS[tier]}, 0.10)
    (row,) = rows
    assert row["kind"] == "weekend" and row["closed_hours"] == 48 and row["margin"] == margin
    fee = 0.10 * (1 - pricing.TIERS[tier].discount)
    s = 0.7e-4
    gap = -row["gap"]  # the bars store the log move to a millionth
    assert gap == pytest.approx(0.03, abs=1e-6)
    winner = (gap - 2 * s) * (1 - fee)
    loser = max(-gap - 2 * s, -(margin + s))
    assert row["payoff"] == pytest.approx(winner + loser, rel=1e-9)
    assert row["bad_debt"] == pytest.approx(max(gap + 2 * s - margin - s, 0.0), rel=1e-9)


def test_a_yen_market_converts_its_gap_at_usd_jpys_move():
    # Japan 225 gaps 2% while USD-JPY rises 1% (the yen falls): each leg's USD PnL is the gap times
    # USD-JPY's old price over its new one.
    b = np.zeros((WEEK, 2, 4), dtype=np.int32)
    b[100:148, 0] = CLOSED
    b[148, 0, 0] = round(np.log(1.02) * 1e6)
    b[120, 1, 3] = round(np.log(1.01) * 1e6)
    specs = {"JP225": dict(SPEC, id=25, quote_currency={"currency": "jpy", "fx_market": 12}), "USD-JPY": dict(SPEC, id=12, asset_class="fx")}
    tiers = {0: pricing.TIERS[0]}
    rows = gaps.pair_rows(b, ["JP225", "USD-JPY"], specs, {"JP225": 0.0, "USD-JPY": 0.0}, {"JP225": 0.0, "USD-JPY": 0.0}, tiers, 0.10)
    row = next(r for r in rows if r["symbol"] == "JP225")
    s = 0.7e-4
    usd = row["gap"] * np.exp(-round(np.log(1.01) * 1e6) / 1e6)
    assert usd == pytest.approx(0.02 / 1.01, rel=1e-4)  # the bars store each log move to a millionth
    assert row["payoff"] == pytest.approx((usd - 2 * s) * 0.9 + (-usd - 2 * s), rel=1e-9)


def test_the_summary_counts_closures_a_year_and_the_pairs_payoff():
    rows = [{"symbol": "X", "asset_class": "index", "tier": 0, "kind": k, "margin": 0.05, "gap": g, "payoff": p, "bad_debt": 0.0, "past_balance": False}
            for k, g, p in (("weekend", 0.01, -0.001), ("weekend", -0.02, 0.003), ("break", 0.0, -0.0002))]
    table = {r["kind"]: r for r in gaps.summary(rows, years=0.5)}
    assert table["all"]["closures_year"] == 6.0
    assert table["weekend"]["payoff_bps_year"] == pytest.approx(0.002 / 0.5 * 1e4)
    assert table["weekend"]["payoff_per_margin_year"] == pytest.approx(0.002 / 0.5 / 0.10)
    assert table["weekend"]["mean_abs_gap_bps"] == pytest.approx(150.0)


def test_the_breakeven_margin_is_the_least_at_which_weekend_pairs_stop_paying():
    # One 3% weekend gap: at 0.5% and 1% the loser's balance caps its loss and the pair pays; from
    # 3% the winner's fee and both legs' spreads make it lose.
    b = bars_with_weekend(-0.03, range(100, 148))
    specs = {"US500": SPEC, "BTC-USD": {"id": 1, "asset_class": "crypto"}}
    out = gaps.breakeven(b, ["US500", "BTC-USD"], specs, {"US500": 0.0}, {"US500": 0.0}, 0.10, pricing.TIERS[3])["US500"]
    assert out["payoff_bps_year"][50] > 0 and out["payoff_bps_year"][100] > 0
    assert out["payoff_bps_year"][300] < 0
    assert out["breakeven_margin_bps"] == min(bps for bps, pay in out["payoff_bps_year"].items() if pay <= 0)
