import numpy as np
import pytest

from calib import latency, spreads


def test_the_best_tier_rounds_up_to_whole_bps_and_is_at_least_one():
    assert spreads.best_tier_bps(0.2, 0.18) == 1
    assert spreads.best_tier_bps(1.18, 0.18) == 2
    assert spreads.best_tier_bps(0.0, 0.0) == 1


def test_positions_do_not_overlap():
    t = np.arange(0, 100, 5)
    eligible = np.ones(len(t), dtype=bool)
    taken = latency.trades(t, eligible, 12)
    starts = t[taken]
    assert list(starts) == [0, 15, 30, 45, 60, 75, 90]


class Hour:
    """A sampled hour at 0.25 s steps: `lead` steps before the analysed hour, then the hour and a tail."""

    step_s, lead, market, hour, stratum = 0.25, 20, "TEST", 0, 0

    def __init__(self, x):
        self.x = np.asarray(x, dtype=float)

    @property
    def analysis(self):
        return slice(self.lead, self.lead + int(round(3600 / self.step_s)))

    def steps(self, seconds):
        return int(round(seconds / self.step_s))


def window_max(x, width):
    return np.lib.stride_tricks.sliding_window_view(x, width + 1).max(axis=1)


def window_min(x, width):
    return np.lib.stride_tricks.sliding_window_view(x, width + 1).min(axis=1)


def path(steps: dict) -> np.ndarray:
    """A flat log index that moves by each `steps[i]` from index `i` on."""
    x = np.zeros(20 + 14400 + 40)
    for i, move in steps.items():
        x[i:] += move
    return x


def row(rows, lag_s, hold_s, theta_bps):
    return next(r for r in rows if (r["lag_s"], r["hold_s"], r["theta_bps"]) == (lag_s, hold_s, theta_bps))


J, SPREAD = 10e-4, 1e-4  # a 10 bps step; a 1 bp spread on each fill


def test_a_step_the_trader_sees_first_pays_it_less_both_fills_spreads():
    rows = latency.hour_rows(Hour(path({100: J})), SPREAD, window_max, window_min)
    # A 0.25 s lead sees the step once and holds through it: one trade worth the step less two spreads.
    r = row(rows, 0.25, 1, 5)
    assert r["trades"] == 1
    assert r["positive"] == pytest.approx(J - 2 * SPREAD)
    assert r["negative"] == 0.0
    # A 3 s lead sees it for 3 s, so 1 s holds trade it three times; each closes on a window that
    # ends before it opened (`end < t`) and takes the step once.
    r = row(rows, 3.0, 1, 5)
    assert r["trades"] == 3
    assert r["positive"] == pytest.approx(3 * (J - 2 * SPREAD))
    # A threshold above the step trades nothing.
    assert row(rows, 0.25, 1, 12)["trades"] == 0


def test_a_move_after_the_open_counts_both_ways():
    # The index steps up 10 bps, then down 4 bps half a second later, inside a 1 s hold. The fall
    # the trader could not see is mirrored: 10 - 4 - 2 = 4 bps and 10 + 4 - 2 = 12 bps, averaging 8.
    rows = latency.hour_rows(Hour(path({100: J, 102: -4e-4})), SPREAD, window_max, window_min)
    r = row(rows, 0.25, 1, 5)
    assert r["trades"] == 1
    assert r["positive"] == pytest.approx(J - 2 * SPREAD)
    assert r["negative"] == 0.0


def test_a_spread_wider_than_half_the_step_loses():
    rows = latency.hour_rows(Hour(path({100: J})), 6e-4, window_max, window_min)
    r = row(rows, 0.25, 1, 5)
    assert r["positive"] == 0.0
    assert r["negative"] == pytest.approx(2 * 6e-4 - J)


def test_a_jump_holds_level_two_for_the_cooldown_and_a_later_jump_renews_it():
    from calib import jump

    min_spread = jump.with_jump(lambda x, step_s: np.zeros(len(x), dtype=np.int64))
    x = np.zeros(1000)
    x[100:] += np.log1p(25e-4)  # a 25 bps move within a second
    x[200:] += np.log1p(15e-4)  # 15 bps: under the trigger, but inside the first cooldown
    out = min_spread(x, 1.0) * 1e4
    assert (out[:100] == 0).all()
    assert (out[100:400] == 10).all()
    assert (out[400:] == 0).all()
    x[300:] += np.log1p(30e-4)  # a second jump renews the cooldown
    out = min_spread(x, 1.0) * 1e4
    assert (out[100:600] == 10).all()
    assert (out[600:] == 0).all()


def test_the_tier_lines_read_back_as_genesis_spread_tiers():
    import tomllib

    rows = [{"symbol": "BTC-USD", "tiers": [{"up_to": "2000000", "bps": 2}, {"up_to": "10000000", "bps": 4}, {"up_to": None, "bps": 8}]},
            {"symbol": "EUR-USD", "tiers": [{"up_to": "10000000", "bps": 1}, {"up_to": None, "bps": 4}]}]
    lines = [line for line in spreads.toml_lines(rows).splitlines() if line.startswith("spread_tiers")]
    assert [tomllib.loads(line)["spread_tiers"] for line in lines] == [
        [{"up_to": "2000000", "bps": 2}, {"up_to": "10000000", "bps": 4}, {"bps": 8}],
        [{"up_to": "10000000", "bps": 1}, {"bps": 4}],
    ]
