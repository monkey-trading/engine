"""The stress-trigger calibration: the engine move-measure replication, the hysteresis level
series, the choice rules, and the utilisation instrumentation."""

import numpy as np
import pytest

from model import simulate, stress


def test_a_constant_mid_reads_zero_move():
    mids = np.full((2, 600), 100.0)
    assert stress.engine_move_measures(mids, window_s=60).max() == 0.0


def test_the_move_measure_tracks_the_window_extremes():
    # window 10s, buckets of 1s. A rise of 1% from the window low, then a flat mid, reads the
    # distance to the low; a fall back reads the distance to the high.
    mids = np.ones((1, 30))
    mids[0, 10] = 1.01  # new high; the reading at t=10 vs the window low (1.0) is 100 bps
    mids[0, 11:] = 1.01
    measure = stress.engine_move_measures(mids, window_s=10)
    assert measure[0, 10] == pytest.approx(100.0, rel=1e-9)  # (1.01 - 1.0) / 1.0
    # t=11..19: the mid sits at the window's high, 0.99% above its low.
    assert measure[0, 15] == pytest.approx((1.01 - 1.0) / 1.0 * 1e4, rel=1e-9)
    # From t=20 the t=10 bucket has left the window (>10s old): only 1.01 mids remain.
    assert measure[0, 25] == 0.0


def test_the_move_measure_uses_the_larger_side():
    # A mid 2% above the window low and 1% below the window high reads the larger, low side.
    mids = np.ones((1, 20))
    mids[0, 5:] = 1.02  # high 1.02, then the mid drops to 1.01 at t=10
    mids[0, 10:] = 1.01
    measure = stress.engine_move_measures(mids, window_s=10)
    assert measure[0, 12] == pytest.approx((1.01 - 1.0) / 1.0 * 1e4, rel=1e-9)


def test_a_bucket_holds_its_lowest_and_highest_mid():
    # window 20s: buckets of 2s, so t=0 and t=1 share a bucket. A dip inside the bucket stays
    # the window's low after the mid recovers.
    mids = np.ones((1, 30))
    mids[0, 1] = 0.99
    measure = stress.engine_move_measures(mids, window_s=20)
    assert measure[0, 2] == pytest.approx((1.0 - 0.99) / 0.99 * 1e4, rel=1e-9)
    # Once the dip's bucket leaves the window (newest mid over 20s old), the reading is zero.
    assert measure[0, 25] == 0.0


def test_the_level_rises_at_once_and_steps_down_only_after_the_cooldown():
    measure = np.zeros(1000)
    measure[100:103] = 200  # crosses both thresholds for 3 seconds
    measure[500:502] = 60  # crosses t1 only
    levels = stress.stress_levels(measure, t1=50, t2=150, cooldown_s=300)
    assert levels[99] == 0
    assert levels[100] == 2  # up at once
    assert levels[103] == 2  # holds past the crossing
    assert levels[399] == 2  # 300s after the rise it may step down only on a lower reading
    assert levels[400] == 0  # the step-down goes to the current target, skipping level 1
    assert levels[499] == 0
    assert levels[500] == 1  # a t1 crossing after the cooldown raises to 1 at once
    assert levels[799] == 1
    assert levels[800] == 0  # and steps back down 300s later


def test_the_fast_level_algorithm_matches_the_second_by_second_loop():
    rng = np.random.default_rng(3)
    for _ in range(20):
        measure = rng.choice([0.0, 40.0, 160.0, 300.0], size=5000, p=[0.9, 0.06, 0.03, 0.01])
        slow = stress.stress_levels(measure, t1=50, t2=150, cooldown_s=300)
        fast = stress._crossing_events(measure, t1=50, t2=150, cooldown_s=300)
        assert np.array_equal(slow, fast)


def move_row(symbol, mix, t1, t2, time_l1, time_l2, vol_pct):
    return {
        "symbol": symbol,
        "window_s": 60,
        "mix": mix,
        "sigma_min_bps": 0,
        "q50": 0,
        "q90": 0,
        "q99": 0,
        "q999": 0,
        "t1": t1,
        "t2": t2,
        "time_l1": time_l1,
        "time_l2": time_l2,
        "vol_pct_l1": vol_pct,
    }


def test_choose_move_takes_the_most_protective_candidate_passing_every_bound():
    rows = []
    # The loosest candidate reports the best selection quality, a metric that rises with the
    # threshold; the choice still falls on the tightest candidate within the time bounds.
    for t1, t2, l1, l2, pct in ((50, 150, 0.002, 0.0002, 0.8), (60, 150, 0.001, 0.0001, 0.9), (150, 300, 0.0001, 0.0, 0.99)):
        rows.append(move_row("BTC-USD", 1.0, t1, t2, l1, l2, pct))
        rows.append(move_row("EUR-USD", 0.0, t1, t2, 0.0, 0.0, None))
    choice = stress.choose_move(rows, window_s=60)
    assert (choice["move_bps_l1"], choice["move_bps_l2"]) == (50, 150)
    assert choice["median_vol_pct_l1"] == 0.8


def test_choose_move_rejects_a_candidate_over_the_time_bounds():
    rows = [move_row("BTC-USD", 1.0, 50, 150, 0.05, 0.0, 0.95)]
    with pytest.raises(ValueError):
        stress.choose_move(rows, window_s=60)


def genesis_with_bounds(bounds):
    return {"markets": [{"symbol": s, "price_rules": {"max_divergence_bps": b}} for s, b in bounds.items()]}


def test_choose_divergence_covers_the_spread_on_the_tightest_market():
    genesis = genesis_with_bounds({"EUR-USD": 5, "BTC-USD": 25})
    choice = stress.choose_divergence(genesis)
    # L1: d/2 >= 2 bps needs d >= 4, a share of 4/5 of the tightest bound; L2: d/2 >= 10 needs
    # d >= 20, 4x the tightest bound, capped at the highest reachable share, 9500.
    assert choice == {"divergence_share_l1": 8000, "divergence_share_l2": 9500, "tightest_bound_bps": 5}


def test_divergence_tables_report_bps_costs_at_fire():
    rows = stress.divergence_tables(genesis_with_bounds({"EUR-USD": 5, "BTC-USD": 25}))
    by = {(r["symbol"], r["share"]): r for r in rows}
    eur = by[("EUR-USD", "calibrated")]
    assert eur["divergence_bps_l1"] == 4.0
    assert eur["worst_cost_l1"] == 2.0  # equals the level-1 spread: the protection rule binds
    btc = by[("BTC-USD", "launch")]
    assert btc["divergence_bps_l1"] == 15.0
    assert btc["expected_cost_l1"] == pytest.approx(3.75)


def test_utilisation_rows_aggregate_histograms_and_keep_levels_ordered():
    from run import inputs

    _, sim, bars, flow, markets = inputs()
    scenario = sim.Scenario(name="util-test", paths=4, batch_paths=4, days=6, utilisation_stats=True)
    floors = np.full((1, len(markets.symbols)), 1e7)
    out = sim.run_batch(scenario, markets, bars, flow, floors, np.array([1e15]), 99, 4)
    gaps = stress.gap_tables(bars, markets.symbols)
    rows = stress.utilisation_rows(out, markets.symbols, ["uniform_10M"], gaps)
    by_market = {}
    for row in rows:
        by_market.setdefault(row["symbol"], []).append(row)
    assert by_market, "expected utilisation rows"
    hist = out["util_hist"].sum(axis=0)
    expo = out["util_expo"].sum(axis=0)
    for symbol, market_rows in by_market.items():
        m = markets.symbols.index(symbol)
        levels = [r["level_bps"] for r in market_rows]
        fires = [r["fire_rate"] for r in market_rows]
        assert levels == sorted(levels)
        assert fires == sorted(fires, reverse=True)  # higher levels fire no more often
        assert all(0.0 <= f <= 1.0 for f in fires)
        # Open hours fill the histogram; the exposure rows never exceed it.
        assert hist[:, m, 0].sum() == pytest.approx(market_rows[0]["open_hours"])
        assert expo[:, m, 0].sum() <= 1e7 * hist[:, m, 0].sum()  # exposure never exceeds the cap
        assert market_rows[0]["gap_p99_bps"] == gaps[symbol]["gap_p99_bps"]


def test_utilisation_records_exact_buckets_exposure_and_batches():
    """On a deterministic week — each hour 1 USD of margin opens a 200:1 long that closes two hours
    later, a flat price, floors of $1,000 and $2,000 — every steady hour's book is $200: bucket 20
    on the first lane, bucket 10 on the second, exact exposure sums, merged across two batches."""
    from model import prices
    from model.flow import Flow, FlowClass

    week = np.zeros((prices.WEEK, 1, 4), dtype=np.int32)
    markets = simulate.Markets(
        symbols=["TEST"],
        margin_rate=np.array([0.005]),
        schedule=["S"],
        cap_bps=np.array([5000]),
        total_cap_bps=20000,
        fin_long=np.array([0.0]),
        fin_short=np.array([0.0]),
        mix=np.array([1.0]),
        open_hours_per_week=np.array([168.0]),
        columns=np.array([0]),
        fx_columns=np.array([-1]),
        fx_invert=np.array([False]),
    )
    tiny = (np.array([1e-6, 1e9]), np.array([1.0, 1.0]))  # every order fits any positive room
    flow = Flow(
        classes=[FlowClass(kind="directional", hold_h=2, bucket=0, margin_share=1.0, mean_hold_h=2.0, mean_sqrt_hold_h=np.sqrt(2.0), long_share=1.0)],
        leverage={(kind, mapping, "S"): np.array([200.0]) for kind in simulate.KINDS for mapping in ("capped", "relative")},
        daily_margin=np.array([24.0]),
        sizes={(kind, mapping, "S"): tiny for kind in simulate.KINDS for mapping in ("capped", "relative")},
        long_share_ar1=0.0,
        long_share_resid_sd=0.0,
        long_share_slope=0.0,
    )
    scenario = simulate.Scenario(name="det", paths=2, batch_paths=1, days=7, utilisation_stats=True)
    floors = np.array([[1000.0], [2000.0]])
    (out,) = simulate.run_many([(scenario, markets, week, flow, floors, np.array([1e15, 1e15]))], 2)
    hist = out["util_hist"].sum(axis=0)[:, 0, :]
    expo = out["util_expo"].sum(axis=0)[:, 0, :]
    # Hour 0's book is empty; from hour 1 through the last the $200 long opened an hour earlier
    # is open: 200/1000 = 20% on the first lane, 200/2000 = 10% on the second.
    assert hist[0, 0] == 2 and hist[20, 0] == 2 * 167
    assert hist[10, 1] == 2 * 167
    assert expo[20, 0] == pytest.approx(200.0 * 2 * 167)
    assert expo[10, 1] == pytest.approx(200.0 * 2 * 167)
    assert hist.sum() == 2 * 2 * 168  # two lanes, every hour of the week recorded
    assert hist[21:, 0].sum() == 0 and expo[:10, 1].sum() == 0
