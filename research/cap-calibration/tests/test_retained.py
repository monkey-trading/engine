"""The retained-NAV study: capital and sweeps in the simulation, cloned markets and their
correlation, and the floor the grid picks."""

import numpy as np
import pytest

from model import expansion, prices, retained, simulate
from tests.test_simulate import bars, flow, market


def run_with(b, m, f, capital, sweep=0, floor=1e12, **overrides):
    settings = dict(paths=1, batch_paths=1, days=7, block_weeks=1, demean=False, withdraw_on_exit=0.0, idle_withdraw_per_day=0.0, run_withdraw_per_day=0.0, sweep_epoch_h=sweep)
    scenario = simulate.Scenario(**(settings | overrides))
    out = simulate.run_batch(scenario, m, b, f, np.array([[floor]]), np.array([floor]), seed=0, n_paths=1, capital=np.array([capital]))
    return {k: v[0, 0] if np.ndim(v) >= 2 else v for k, v in out.items()}


def test_capital_is_house_cash_and_nav_from_the_first_hour():
    out = run_with(bars(), market(), flow(), capital=500.0)
    assert out["nav_end"] == pytest.approx(500.0)
    assert out["min_cash"] == pytest.approx(500.0)
    assert float(out["identity_gap"]) < 1e-9


def test_capital_absorbs_a_loss_that_would_leave_cash_short():
    # The winning close at hour 50 costs the house 1.8 it never had.
    assert run_with(bars(gap_at=50, gap=0.01), market(), flow(), capital=0.0)["min_cash"] == pytest.approx(-1.8, rel=1e-4)
    out = run_with(bars(gap_at=50, gap=0.01), market(), flow(), capital=5.0)
    assert out["min_cash"] == pytest.approx(3.2, rel=1e-4)


def test_a_sweep_pays_out_only_what_is_above_the_capital():
    # A 1% fall closes the long at a gain to the house, which builds NAV unless it is swept.
    kept = run_with(bars(gap_at=50, gap=-0.01), market(), flow(), capital=10.0)
    swept = run_with(bars(gap_at=50, gap=-0.01), market(), flow(), capital=10.0, sweep=1)
    assert kept["nav_end"] > 10.0
    assert swept["nav_end"] == pytest.approx(10.0)
    assert swept["distributed"] == pytest.approx(kept["nav_end"] - 10.0, rel=1e-6)
    assert float(swept["identity_gap"]) < 1e-9


def test_a_sweep_never_pays_below_the_capital():
    out = run_with(bars(gap_at=50, gap=0.01), market(), flow(), capital=10.0, sweep=1)
    assert out["distributed"] == 0.0
    assert out["min_cash"] == pytest.approx(8.2, rel=1e-4)


def test_a_longer_epoch_sweeps_at_its_boundaries_only():
    daily = run_with(bars(gap_at=50, gap=-0.01), market(), flow(), capital=10.0, sweep=24)
    assert daily["distributed"] > 0.0 and daily["nav_end"] == pytest.approx(10.0)


def history() -> np.ndarray:
    rng = np.random.default_rng(0)
    b = np.zeros((prices.WEEK * 20, 2, 4), dtype=np.int32)
    ret = rng.normal(0, 3000, size=(len(b), 2)).round().astype(np.int32)
    b[..., 3] = ret
    b[..., 1] = np.maximum(ret, 0) + 2000
    b[..., 2] = np.minimum(ret, 0) - 2000
    return b


def hourly_returns(p):
    return np.diff(np.log(p.close), axis=1, prepend=0.0)


def test_a_fully_shared_market_is_unchanged_and_an_unshared_one_is_independent_of_it():
    b = history()
    rng = np.random.default_rng(1)
    weeks = prices.sample_weeks(rng, 400, 2, 20, 1)
    own = np.stack([prices.sample_weeks(rng, 400, 2, 20, 1) for _ in range(2)], axis=-1)
    plain = prices.build(b, weeks, 336)
    same = prices.build(b, weeks, 336, own_weeks=own, shared=np.array([1.0, 1.0]))
    assert np.allclose(plain.close, same.close)
    mixed = hourly_returns(prices.build(b, weeks, 336, own_weeks=own, shared=np.array([1.0, 0.0])))
    assert abs(np.corrcoef(mixed[..., 0].ravel(), mixed[..., 1].ravel())[0, 1]) < 0.05


def test_shared_weight_sets_the_correlation_with_the_template_and_keeps_the_variance():
    # Column 1 repeats column 0's history and mixes it with weight 0.7 on column 0's weeks.
    b = history()
    b[:, 1] = b[:, 0]
    rng = np.random.default_rng(2)
    weeks = prices.sample_weeks(rng, 600, 2, 20, 1)
    own = np.stack([prices.sample_weeks(rng, 600, 2, 20, 1) for _ in range(2)], axis=-1)
    template = prices.build(b, weeks, 336)
    clone = prices.build(b, weeks, 336, own_weeks=own, shared=np.array([1.0, 0.7]))
    r0, r1 = hourly_returns(template)[..., 0], hourly_returns(clone)[..., 1]
    assert np.corrcoef(r0.ravel(), r1.ravel())[0, 1] == pytest.approx(0.7, abs=0.03)
    assert r1.std() == pytest.approx(r0.std(), rel=0.06)


def test_mixed_bars_contain_their_open_and_close():
    b = history()
    rng = np.random.default_rng(3)
    weeks = prices.sample_weeks(rng, 100, 2, 20, 1)
    own = np.stack([prices.sample_weeks(rng, 100, 2, 20, 1) for _ in range(2)], axis=-1)
    p = prices.build(b, weeks, 336, own_weeks=own, shared=np.array([0.6, 0.6]))
    assert (p.high >= np.maximum(p.open, p.close) - 1e-12).all()
    assert (p.low <= np.minimum(p.open, p.close) + 1e-12).all()


LAUNCH = ["BTC-USD", "AAPL", "MSFT", "EUR-USD", "DE40"]
CLASSES = {"BTC-USD": "crypto", "AAPL": "equity", "MSFT": "equity", "EUR-USD": "fx", "DE40": "index"}


def launch_markets() -> simulate.Markets:
    n = len(LAUNCH)
    return simulate.Markets(
        symbols=list(LAUNCH),
        margin_rate=np.full(n, 0.1),
        schedule=["S"] * n,
        cap_bps=np.full(n, 1000),
        total_cap_bps=20000,
        fin_long=np.zeros(n),
        fin_short=np.zeros(n),
        mix=np.array([0.6, 0.2, 0.1, 0.05, 0.05]),
        open_hours_per_week=np.full(n, 168.0),
        columns=np.arange(n),
        fx_columns=np.full(n, -1),
        fx_invert=np.zeros(n, dtype=bool),
    )


def test_clones_cycle_through_their_class_and_never_use_a_foreign_currency_template():
    seq = expansion.template_sequence(LAUNCH, CLASSES, {"equity": 1.0}, 5)
    assert seq == ["AAPL", "MSFT", "AAPL", "MSFT", "AAPL"]
    mixed = expansion.template_sequence(LAUNCH, CLASSES, {"equity": 3.0, "fx": 1.0}, 40)
    assert mixed.count("AAPL") + mixed.count("MSFT") == 30 and mixed.count("EUR-USD") == 10
    assert expansion.template_sequence(["USD-JPY", "AAPL"], {"USD-JPY": "fx", "AAPL": "equity"}, {"equity": 1.0}, 1) == ["AAPL"]
    with pytest.raises(ValueError):
        expansion.template_sequence(["USD-JPY"], {"USD-JPY": "fx"}, {"fx": 1.0}, 1)


def expanded(n=9, **kw):
    b = np.zeros((prices.WEEK, 5, 4), dtype=np.int32)
    return expansion.expand(launch_markets(), b, CLASSES, {"BTC-USD", "AAPL", "MSFT"}, n, {"equity": 1.0}, **kw)


def test_a_clone_takes_its_templates_terms_and_columns_and_follows_its_history():
    m, b = expanded(shared=0.5)
    assert m.symbols[5:] == ["AAPL~1", "MSFT~2", "AAPL~3", "MSFT~4"]
    assert b.shape[1] == 9 and m.columns.tolist() == list(range(9))
    assert (m.shared[:5] == 1).all() and (m.shared[5:] == 0.5).all()
    assert m.mix[5:].tolist() == [0.2, 0.1, 0.2, 0.1]
    assert m.subset([0, 5]).shared.tolist() == [1.0, 0.5]


def test_flow_modes():
    assert expanded(flow_scale=2.0)[0].mix[5:].tolist() == [0.4, 0.2, 0.4, 0.2]
    assert expanded(flow_mode="tail")[0].mix[5:].tolist() == [0.1] * 4  # the smallest listed equity
    assert expanded(flow_mode="fixed_total")[0].mix.sum() == pytest.approx(1.0)
    with pytest.raises(ValueError):
        expanded(flow_mode="other")


def test_the_launch_count_expands_to_itself():
    m, b = expanded(n=5)
    assert m.symbols == LAUNCH and b.shape[1] == 5


def row(floor, refused, p):
    return {"floor": floor, "refused_share_directional": refused, "p_short": p}


def test_the_retained_floor_serves_the_flow_and_stays_safe_at_every_higher_floor():
    rows = [row(1, 0.5, 0.0), row(2, 0.02, 0.0), row(3, 0.005, 0.02), row(4, 0.0, 0.0), row(5, 0.0, 0.0)]
    got = retained.retained_nav(rows)
    assert got["serve_floor"] == 3 and got["safe_floor"] == 4 and got["retained_nav"] == 4
    # Throttled small floors look safe but are not served: only the service limit moves them.
    assert retained.retained_nav([row(1, 0.5, 0.0), row(2, 0.0, 0.0)])["retained_nav"] == 2
    assert retained.retained_nav([row(1, 0.0, 0.5), row(2, 0.0, 0.5)])["retained_nav"] is None


def test_the_grid_spans_its_ends_and_the_plan_runs_every_headline_count():
    grid = retained.floor_grid(retained.GRID_LOW, retained.GRID_HIGH)
    assert grid[0] == retained.GRID_LOW and grid[-1] >= retained.GRID_HIGH
    assert {n for v, n, _ in retained.plan() if v == "baseline"} == set(retained.MARKET_COUNTS)
    assert retained.batch_paths(500) == 5 and retained.batch_paths(37) == 25


def test_the_standard_error_is_binomial():
    assert retained.standard_error(0.01, 2000) == pytest.approx(0.002224, rel=1e-3)


def test_a_schedule_with_the_same_first_tier_keeps_small_orders_and_shrinks_large_ones():
    from model.flow import FlowClass, margin_for_notional, banded_notional, schedule_key, with_schedule

    source = [{"up_to": "2000000", "bps": 100}, {"bps": 500}]
    tiers = [{"up_to": "1000000", "bps": 100}, {"bps": 2000}]
    assert margin_for_notional(banded_notional(123_456.0, source), source) == pytest.approx(123_456.0)
    assert float(margin_for_notional(3e6, source)) == pytest.approx(20_000 + 1e6 * 0.05)
    bounds = np.array([1e5, 1e6, 2e6, 1e7])
    classes = [FlowClass(kind="directional", hold_h=1, bucket=b, margin_share=1 / 3, mean_hold_h=1.0, mean_sqrt_hold_h=1.0, long_share=0.5) for b in range(3)]
    base = flow()
    base = base.__class__(classes=classes, leverage={("directional", "capped", schedule_key(source)): np.array([25.0, 80.0, 100.0])}, daily_margin=base.daily_margin, sizes={("directional", "capped", schedule_key(source)): (bounds, np.array([0.25, 0.5, 0.75, 1.0]))}, long_share_ar1=0.0, long_share_resid_sd=0.0, long_share_slope=0.0)
    out = with_schedule(base, source, tiers)
    moved, shares = out.sizes[("directional", "capped", schedule_key(tiers))]
    assert moved[0] == 1e5 and moved[1] == 1e6  # at or under the first bound: unchanged
    assert moved[2] == pytest.approx(1e6 + (20_000 - 10_000) / 0.2) and moved[3] < 1e7
    assert shares.tolist() == [0.25, 0.5, 0.75, 1.0]
    ratio = out.leverage[("directional", "capped", schedule_key(tiers))]
    assert ratio[0] == 25.0 and (ratio[1:] < np.array([80.0, 100.0])).all()
    assert with_schedule(out, source, tiers) is out
    with pytest.raises(ValueError):
        with_schedule(base, source, [{"up_to": "1000000", "bps": 200}, {"bps": 2000}])


def test_genesis_profiles_fill_schedules_without_one_from_a_profiled_schedule_with_the_same_first_rate():
    from model.flow import FlowClass, schedule_key, with_genesis_profiles

    source = [{"up_to": "2000000", "bps": 100}, {"bps": 500}]
    banded = [{"up_to": "2000000", "bps": 100}, {"up_to": "10000000", "bps": 200}, {"bps": 500}]
    bounds = np.array([1e5, 1e6, 2e6, 1e7])
    classes = [FlowClass(kind=k, hold_h=1, bucket=b, margin_share=1 / 6, mean_hold_h=1.0, mean_sqrt_hold_h=1.0, long_share=0.5)
               for k in ("directional", "hedged") for b in range(3)]
    base = flow()
    keyed = {(k, "capped", schedule_key(source)) for k in ("directional", "hedged")}
    base = base.__class__(classes=classes, leverage={key: np.array([25.0, 80.0, 100.0]) for key in keyed}, daily_margin=base.daily_margin,
                          sizes={key: (bounds, np.array([0.25, 0.5, 0.75, 1.0])) for key in keyed}, long_share_ar1=0.0,
                          long_share_resid_sd=0.0, long_share_slope=0.0)
    out = with_genesis_profiles(base, [{"symbol": "A", "margin_tiers": source}, {"symbol": "B", "margin_tiers": banded}])
    for kind in ("directional", "hedged"):
        assert (kind, "capped", schedule_key(banded)) in out.sizes and (kind, "capped", schedule_key(banded)) in out.leverage
    with pytest.raises(ValueError):
        with_genesis_profiles(base, [{"symbol": "C", "margin_tiers": [{"bps": 1000}]}])
