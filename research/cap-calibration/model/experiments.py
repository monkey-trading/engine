"""The calibration runs: per-market floor sweeps, joint runs of candidate floor sets, and the
sensitivities.

Insolvency measures, each the share of paths in which, over the 90 days:
- `p_end`: house NAV is below zero at the end;
- `p_ever`: house NAV is below zero at some hour-end;
- `p_short`: at some hour-end house cash is below zero, so the vault holds less than clients'
  balances. What clients may withdraw at once is never more than their balances, so this bounds
  from above the chance that the vault cannot pay a withdrawal on demand;
- `p_queue`: under the modelled withdrawals, some withdrawal waits in the #49 queue.
Each is the share the paths show; nothing is extrapolated past what they count. `p_ever` and
`p_short` also carry their count and `_upper`, the exact one-sided 95% upper bound, which with no
failing path is about 3 in the number of paths. `p_dip_100k` and `p_dip_1m` give the chance the
lowest NAV falls below -$100k and -$1M.

Calibration, per measure: each market alone gives a curve of the measure against its floor. A
risk level `q` sets every market's floor to the largest tested floor at which the market alone
stays at or below `q`, every smaller floor too, no higher than the floor at which it stops
refusing orders, and zero when even a zero floor exceeds `q`; the total floor is the sum. The
joint run puts every level's floors, for both measures, and uniform floors on every market
(`UNIFORM_FLOORS`) on the same paths. A target takes, of the candidates whose measure meets it,
the one that refuses the least directional notional over the 90 days. At the choice for
`BASE_TARGET`, a second joint run sweeps the total floor below the sum.
"""

import csv
import json
import math
from dataclasses import replace
from pathlib import Path

import numpy as np

from model import simulate

TARGETS = (0.01, 0.001, 0.0001)
MEASURES = ("p_ever", "p_short")
MARKET_FLOORS = np.concatenate([[0.0], np.geomspace(1e4, 1e8, 17)])
LEVELS = (0.0, 1e-4, 3e-4, 1e-3, 3e-3, 1e-2, 3e-2, 0.1, 0.2, 0.3, 0.5, 1.0)  # 1.0: every market at its binding point
UNIFORM_FLOORS = np.array([1e5, 3e5, 1e6, 3e6, 1e7])  # the same floor on every market
TOTAL_FRACTIONS = np.array([0.25, 0.5, 0.75, 1.0])  # total floor as a share of the market floors' sum
SCALES = np.array([0.1, 0.3, 1.0])  # sensitivity floor scales
NO_TOTAL_LIMIT = 1e15


def upper_bound(hits: int, n: int, confidence: float = 0.95) -> float:
    """The exact (Clopper-Pearson) one-sided upper bound on a probability seen `hits` times in `n`
    paths: the rate under which `hits` or fewer would happen with probability 1 - `confidence`."""
    if hits >= n:
        return 1.0
    i = np.arange(hits + 1)
    log_fact = np.concatenate([[0.0], np.cumsum(np.log(np.arange(1, n + 1)))])
    log_choose = log_fact[n] - log_fact[i] - log_fact[n - i]

    def cdf(p: float) -> float:
        logs = log_choose + i * math.log(p) + (n - i) * math.log1p(-p)
        top = logs.max()
        return float(math.exp(top) * np.exp(logs - top).sum())

    lo, hi = hits / n, 1.0
    for _ in range(60):
        mid = (lo + hi) / 2
        if mid > 0 and cdf(mid) > 1 - confidence:
            lo = mid
        else:
            hi = mid
    return hi


def lane_stats(out: dict, lane: int) -> dict:
    nav_end = out["nav_end"][:, lane]
    n = len(nav_end)
    opened = bool(np.isfinite(out["min_nav"][:, lane]).any())  # some path had cash or exposure
    first_negative = out["first_negative_h"][:, lane][out["first_negative_h"][:, lane] >= 0]
    lowest_nav = np.where(np.isfinite(out["min_nav"][:, lane]), out["min_nav"][:, lane], 0.0)  # 0 where nothing opened
    ever = int(out["ever_negative"][:, lane].sum())
    short = int((out["min_cash"][:, lane] < 0).sum())
    return {
        "p_end": float((nav_end < 0).mean()),
        "p_ever": ever / n,
        "ever_paths": ever,
        "p_ever_upper": upper_bound(ever, n) if opened else 0.0,  # nothing open, nothing to lose
        "p_short": short / n,
        "short_paths": short,
        "p_short_upper": upper_bound(short, n) if opened else 0.0,
        "p_queue": float(out["queued"][:, lane].mean()),
        "p_dip_100k": float((out["min_nav"][:, lane] < -1e5).mean()),
        "p_dip_1m": float((out["min_nav"][:, lane] < -1e6).mean()),
        "refused_share": float((out["refused"][:, lane].sum() + out["refused_hedged"][:, lane].sum()) / max(out["desired"][:, lane].sum(), 1e-9)),
        "refused_share_directional": float(out["refused"][:, lane].sum() / max(out["desired_directional"][:, lane].sum(), 1e-9)),
        "refused_share_week1": float(out["refused_week1"][:, lane].sum() / max(out["desired_week1"][:, lane].sum(), 1e-9)),
        "first_negative_day_p50": float(np.median(first_negative) / 24) if len(first_negative) else -1.0,
        "nav_end_mean": float(nav_end.mean()),
        "nav_end_p01": float(np.quantile(nav_end, 0.01)),
        "nav_end_p50": float(np.quantile(nav_end, 0.5)),
        "min_nav_p01": float(np.quantile(lowest_nav, 0.01)),
        "min_nav_p50": float(np.quantile(lowest_nav, 0.5)),
        "fee_mean": float(out["fee"][:, lane].mean()),
        "spread_income_mean": (float(out["spread_income"][:, lane].mean()) if "spread_income" in out else 0.0),
        "financing_mean": float(out["financing"][:, lane].mean()),
        "bad_debt_mean": float(out["bad_debt"][:, lane].mean()),
        "bad_debt_p99": float(np.quantile(out["bad_debt"][:, lane], 0.99)),
        "mint_basis_mean": float(out["mint_basis"][:, lane].mean()),
        "max_queue_p99": float(np.quantile(out["max_queue"][:, lane], 0.99)),
        "max_gross_exposure_p50": float(np.quantile(out["max_gross_exposure"][:, lane], 0.5)),
        "paths": int(len(nav_end)),
    }


def binding_point(rows: list[dict], share: float = 0.01) -> float | None:
    """The smallest floor at which the directional refused share falls below `share`."""
    for row in sorted(rows, key=lambda r: r["floor"]):
        if row["refused_share_directional"] < share:
            return row["floor"]
    return None


def floor_at_level(rows: list[dict], key: str, level: float) -> float:
    """One market's floor at risk level `level`: the largest floor with every floor up to it at or
    below the level, no higher than the binding point; zero when a zero floor exceeds the level."""
    bind = binding_point(rows)
    best = 0.0
    for row in sorted(rows, key=lambda r: r["floor"]):
        if row[key] > level:
            break
        best = row["floor"]
    return min(best, bind) if bind is not None else best


def level_floors(rows: list[dict], symbols: list[str], key: str) -> np.ndarray:
    """(levels, markets) floors for `LEVELS`."""
    by_market = {s: [r for r in rows if r["symbol"] == s] for s in symbols}
    return np.array([[floor_at_level(by_market[s], key, level) for s in symbols] for level in LEVELS])


def per_market(scenario, markets, bars, flow, workers: int) -> list[dict]:
    """Each market alone, at its own share of the flow, over the floor grid, with no total cap."""
    floors = MARKET_FLOORS[:, None]
    total = np.full(len(MARKET_FLOORS), NO_TOTAL_LIMIT)
    tasks = [(scenario, markets.subset([i]), bars, flow, floors, total) for i in range(len(markets.symbols))]
    rows = []
    for symbol, out in zip(markets.symbols, simulate.run_many(tasks, workers)):
        for lane, floor in enumerate(MARKET_FLOORS):
            rows.append({"symbol": symbol, "floor": float(floor), **lane_stats(out, lane)})
    return rows


def joint_rows(markets, out: dict, floors: np.ndarray, totals: np.ndarray, labels: list) -> list[dict]:
    rows = []
    for lane, label in enumerate(labels):
        refused = out["refused_by_market"][:, lane].sum(axis=0) / np.maximum(out["desired_by_market"][:, lane].sum(axis=0), 1e-9)
        pnl = out["pnl_by_market"][:, lane]
        rows.append(
            {
                "level": label,
                "total_floor": float(totals[lane]),
                **lane_stats(out, lane),
                "market_floors": dict(zip(markets.symbols, floors[lane].tolist())),
                "refused_by_market": dict(zip(markets.symbols, np.round(refused, 4).tolist())),
                "pnl_by_market_mean": dict(zip(markets.symbols, np.round(pnl.mean(axis=0), 0).tolist())),
                "pnl_by_market_p01": dict(zip(markets.symbols, np.round(np.quantile(pnl, 0.01, axis=0), 0).tolist())),
                "identity_gap": out["identity_gap"],
            }
        )
    return rows


def joint(scenario, markets, bars, flow, sweep: list[dict], workers: int) -> dict[str, list[dict]]:
    """All markets together on the same paths: each measure's floors at every level in `LEVELS`,
    and `UNIFORM_FLOORS`. Returns the rows by family: each measure's levels, and "uniform"."""
    families, lanes, labels = [], [], []
    for key in MEASURES:
        lanes.append(level_floors(sweep, markets.symbols, key))
        families += [key] * len(LEVELS)
        labels += list(LEVELS)
    lanes.append(UNIFORM_FLOORS[:, None] * np.ones(len(markets.symbols)))
    families += ["uniform"] * len(UNIFORM_FLOORS)
    labels += UNIFORM_FLOORS.tolist()
    floors = np.concatenate(lanes)
    (out,) = simulate.run_many([(replace(scenario, name="joint"), markets, bars, flow, floors, floors.sum(axis=1))], workers)
    rows = [{"family": f, **r} for f, r in zip(families, joint_rows(markets, out, floors, floors.sum(axis=1), labels))]
    return {family: [r for r in rows if r["family"] == family] for family in dict.fromkeys(families)}


def total_sweep(scenario, markets, bars, flow, base: dict[str, dict], workers: int) -> dict:
    """For each measure, the floors chosen for it (`base`) with the total floor at
    `TOTAL_FRACTIONS` of their sum, all on the same paths."""
    floors = [market_floors(base[key], markets.symbols) for key in MEASURES]
    lanes = np.concatenate([np.repeat(f[None, :], len(TOTAL_FRACTIONS), axis=0) for f in floors])
    totals = np.concatenate([TOTAL_FRACTIONS * f.sum() for f in floors])
    (out,) = simulate.run_many([(replace(scenario, name="total"), markets, bars, flow, lanes, totals)], workers)
    rows = joint_rows(markets, out, lanes, totals, TOTAL_FRACTIONS.tolist() * len(MEASURES))
    n = len(TOTAL_FRACTIONS)
    return {key: rows[i * n : (i + 1) * n] for i, key in enumerate(MEASURES)}


def market_floors(row: dict, symbols: list[str]) -> np.ndarray:
    """A joint row's market floors, from the run or as read back from its CSV."""
    floors = row["market_floors"]
    floors = json.loads(floors) if isinstance(floors, str) else floors
    return np.array([floors[s] for s in symbols])


def candidates(families: dict[str, list[dict]], key: str) -> list[dict]:
    """The floor sets a target for `key` chooses from: its own levels and the uniform floors."""
    return families[key] + families.get("uniform", [])


def chosen(rows: list[dict], key: str, target: float) -> dict | None:
    """Of the rows whose measure meets `target`, the one that refuses the least directional
    notional over the 90 days, the smaller total floor on a tie; None when none meets it."""
    meeting = [r for r in rows if r[key] <= target]
    return min(meeting, key=lambda r: (r["refused_share_directional"], r["total_floor"]), default=None)


SENSITIVITIES = {
    "no farming": {"hedged_scale": 0.0},
    "farming x2": {"hedged_scale": 2.0},
    "farming decays, 30-day half-life": {"farm_half_life_days": 30.0},
    "farm pairs held through closures": {"gap_pair_share": 0.1},
    "vol x1.5": {"vol_scale": 1.5},
    "vol x2": {"vol_scale": 2.0},
    "leverage relative to max": {"leverage_mapping": "relative"},
    "markets independent": {"independent_markets": True},
    "one client direction for all markets": {"common_direction": True},
    "flow x0.25": {"flow_scale": 0.25},
    "flow x4": {"flow_scale": 4.0},
    "exits before closures": {"hold_through_closures": False},
    "keep drift": {"demean": False},
}


def sensitivity(scenario, markets, bars, flow, floors: np.ndarray, workers: int) -> list[dict]:
    """Every variant at `floors` scaled by `SCALES`, per-market and total together."""
    lanes = SCALES[:, None] * floors[None, :]
    names, tasks = [], []
    for name, change in {"baseline": {}, **SENSITIVITIES}.items():
        names.append(name)
        tasks.append((replace(scenario, name=name, **change), markets, bars, flow, lanes, lanes.sum(axis=1)))
    rows = []
    for name, out in zip(names, simulate.run_many(tasks, workers)):
        for lane, scale in enumerate(SCALES):
            rows.append({"scenario": name, "floor_scale": float(scale), **lane_stats(out, lane)})
    return rows


def write_rows(path: Path, rows: list[dict]) -> None:
    flat = [{k: (json.dumps(v) if isinstance(v, dict) else v) for k, v in r.items()} for r in rows]
    with path.open("w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(flat[0]))
        writer.writeheader()
        writer.writerows(flat)

