"""The retained house NAV at each market count (#134).

The retained NAV is the staking liquidity floor: an epoch close distributes the NAV and cash above
it, so once the house has earned it the floor is the NAV the house keeps. The study holds the house
at that floor, the worst case for solvency: it opens each 90-day run with the floor as capital and
sweeps everything above it at every `sweep_epoch_h`, so fees never build a cushion. Exposure caps
are the genesis NAV shares (`house_cap_bps`, `house_total_cap_bps`) with no dollar floors, so the
floor is also what the caps are a share of. Markets beyond the launch 37 are clones
(`expansion.py`).

For each market count and variant the floor goes up a geometric grid on the same paths. The
retained NAV is the lowest grid floor that both serves the flow, refusing at most `REFUSAL_LIMIT`
of the directional notional (below that the caps throttle the house into looking safe), and keeps
house cash below zero at or under `TARGET` of paths at it and at every higher grid floor.
"""

import csv
from dataclasses import replace

import numpy as np

from model import experiments, simulate
from catalogue import PLANNED_CRYPTO_TIERS, PREVIOUS_CRYPTO_TIERS, load_genesis
from model.flow import with_schedule
from model.expansion import expand, require_profiles, with_crypto_bands

TARGET = 0.01
REFUSAL_LIMIT = 0.01
MARKET_COUNTS = (37, 50, 75, 100, 150, 200, 300, 500)
GRID_RATIO = 1.6
GRID_LOW, GRID_HIGH = 4e6, 3e9
BASE_CLASS_WEIGHTS = {"equity": 1.0}
BASE_SHARED = 0.7

CRYPTO_BANDS = {"planned": PLANNED_CRYPTO_TIERS, "previous": PREVIOUS_CRYPTO_TIERS, "genesis": None}
PREVIOUS = "crypto bands 1% to $1M, 20% above"
GENESIS = "crypto margin 10% (genesis)"
# Fixed dollar caps instead of NAV shares: each market capped at this many dollars of net client
# exposure, the total at their sum, whatever the NAV. The shares of NAV make the loss a gap can
# cause scale with the NAV, so only dollar caps make the NAV a cushion that is sized.
DOLLAR_1M = "dollar caps $1M a market"
DOLLAR_10M = "dollar caps $10M a market"
DOLLAR_GRID = np.array([0.0, 5e5, 1e6, 2e6, 4e6, 8e6, 16e6, 32e6, 64e6])
DOLLAR_PATHS = {DOLLAR_10M: {37: 2000, 100: 1000, 500: 300}, DOLLAR_1M: {37: 1000, 500: 150}}

# name -> (scenario changes, expansion changes). The baseline is the planned bands on BTC-USD and
# ETH-USD (`catalogue.PLANNED_CRYPTO_TIERS`), and clones that are equity markets
# correlated 0.7 with their template, each as busy as it, with a sweep every hour.
VARIANTS = {
    "baseline": ({}, {}),
    PREVIOUS: ({}, {"crypto_bands": "previous"}),
    GENESIS: ({}, {"crypto_bands": "genesis"}),
    "no farming": ({"hedged_scale": 0.0}, {}),
    "farming x2": ({"hedged_scale": 2.0}, {}),
    "vol x1.5": ({"vol_scale": 1.5}, {}),
    "vol x2": ({"vol_scale": 2.0}, {}),
    "correlation 0.3": ({}, {"shared": 0.3}),
    "correlation 0.9": ({}, {"shared": 0.9}),
    "correlation 1": ({}, {"shared": 1.0}),
    "one client direction": ({"common_direction": True}, {}),
    "flow x0.25": ({}, {"flow_scale": 0.25}),
    "flow x4": ({}, {"flow_scale": 4.0}),
    "total flow fixed": ({}, {"flow_mode": "fixed_total"}),
    "tail flow": ({}, {"flow_mode": "tail"}),
    "sweep daily": ({"sweep_epoch_h": 24}, {}),
    "sweep weekly": ({"sweep_epoch_h": 168}, {}),
    "no sweep": ({"sweep_epoch_h": 0}, {}),
    DOLLAR_1M: ({}, {"dollar_cap": 1e6}),
    DOLLAR_10M: ({}, {"dollar_cap": 1e7}),
    "crypto clones": ({}, {"class_weights": {"crypto": 1.0}}),
    "index and commodity clones": ({}, {"class_weights": {"index": 1.0, "commodity": 1.0}}),
    "mixed clones": ({}, {"class_weights": {"equity": 6.0, "fx": 2.0, "index": 1.0, "commodity": 1.0, "crypto": 1.0}}),
}


def floor_grid(lo: float, hi: float) -> np.ndarray:
    return lo * GRID_RATIO ** np.arange(int(np.ceil(np.log(hi / lo) / np.log(GRID_RATIO))) + 1)


# (variant, market count) -> paths. The headline is the baseline at every count; the rest are
# sensitivities at the launch count, a mid count and the largest.
HEADLINE_PATHS = {37: 4000, 50: 1500, 75: 1000, 100: 1000, 150: 500, 200: 400, 300: 300, 500: 200}
COMPARISON_PATHS = {37: 500, 50: 400, 75: 300, 100: 300, 150: 150, 200: 150, 300: 100, 500: 60}  # genesis crypto margin
SENSITIVITY_PATHS = {37: 300, 100: 150}
# Variants whose answer changes with the market count are run at every count, on fewer paths.
SCALING_VARIANTS = ("no farming", "vol x2", "correlation 1", "flow x4", "crypto clones", "mixed clones")
SCALING_PATHS = {37: 100, 100: 100, 200: 100, 500: 60}
STABILITY_PATHS = (500, 1000, 2000, 4000)  # the first this many paths of the 37-market baseline


def batch_paths(n: int) -> int:
    """Paths per batch: fewer as the markets grow, to bound memory. Part of the random stream."""
    return int(max(5, min(25, 2500 // n)))


def plan() -> list[tuple[str, int, int]]:
    """(variant, market count, paths) for the whole stage, in run order."""
    runs = [("baseline", n, paths) for n, paths in HEADLINE_PATHS.items()]
    runs += [(v, n, paths) for v in (PREVIOUS, GENESIS) for n, paths in COMPARISON_PATHS.items()]
    runs += [(v, n, paths) for v, counts in DOLLAR_PATHS.items() for n, paths in counts.items()]
    for n, paths in SENSITIVITY_PATHS.items():
        runs += [(v, n, paths) for v in VARIANTS if v not in ("baseline", PREVIOUS, GENESIS, *DOLLAR_PATHS)]
    runs += [(v, n, paths) for v in SCALING_VARIANTS for n, paths in SCALING_PATHS.items()]
    first = {}
    for v, n, paths in runs:
        first.setdefault((v, n), paths)  # a run in two groups takes the first one's paths
    return [(v, n, paths) for (v, n), paths in first.items()]


def take(out: dict, k: int) -> dict:
    """The first `k` paths of a run's outputs."""
    return {key: (v[:k] if getattr(v, "ndim", 0) >= 1 and key != "identity_gap" else v) for key, v in out.items()}


def run_variant(scenario, markets, bars, flow, classes, listed, n: int, variant: str, paths: int, grid: np.ndarray, workers: int, slices: tuple = ()) -> list[dict]:
    """One market count under one variant: a row per grid floor (and per path count in `slices`,
    the first that many paths)."""
    changes, expansion = VARIANTS[variant]
    scenario = replace(scenario, name=f"{variant} {n}", paths=paths, batch_paths=batch_paths(n), **({"sweep_epoch_h": 1} | changes))
    spec = {"class_weights": BASE_CLASS_WEIGHTS, "shared": BASE_SHARED, "crypto_bands": "planned"} | expansion
    tiers = CRYPTO_BANDS[spec.pop("crypto_bands")]
    if tiers is not None:
        genesis = load_genesis()["markets"]
        markets = with_crypto_bands(markets, tiers)
        source = next(m["margin_tiers"] for m in genesis if m["margin_tiers"][0]["bps"] == tiers[0]["bps"])
        flow = with_schedule(flow, source, tiers)
    require_profiles(markets, flow)
    dollar = spec.pop("dollar_cap", None)
    m, b = expand(markets, bars, classes, listed, n, **spec)
    floors, total = np.zeros((len(grid), n)), np.zeros(len(grid))
    if dollar is not None:
        m = replace(m, cap_bps=np.zeros(n, dtype=int), total_cap_bps=0)
        floors, total = np.full((len(grid), n), dollar), np.full(len(grid), dollar * n)
    (out,) = simulate.run_many([(scenario, m, b, flow, floors, total, grid)], workers)
    return [row for count in slices or (scenario.paths,) for row in lane_rows(take(out, count), variant, n, grid)]


STAT_KEYS = ("paths", "p_short", "short_paths", "p_short_upper", "p_ever", "refused_share_directional", "refused_share_week1", "fee_mean", "bad_debt_mean", "bad_debt_p99", "nav_end_mean")


def lane_rows(out: dict, variant: str, n: int, grid: np.ndarray) -> list[dict]:
    """A row per grid floor: the lane's measures, what the sweeps distributed, and the 1st
    percentile of the lowest house cash."""
    rows = []
    for lane, capital in enumerate(grid):
        stats = experiments.lane_stats(out, lane)
        lowest = np.where(np.isfinite(out["min_cash"][:, lane]), out["min_cash"][:, lane], 0.0)
        rows.append(
            {"variant": variant, "markets": n, "floor": float(capital)}
            | {key: stats[key] for key in STAT_KEYS}
            | {"distributed_mean": float(out["distributed"][:, lane].mean()), "min_cash_p01": float(np.quantile(lowest, 0.01))}
        )
    return rows


def workers_for(n: int, workers: int) -> int:
    """Fewer worker processes as the markets grow, to bound memory."""
    return max(1, min(workers, 3 if n <= 150 else 2))


def run_stage(scenario, markets, bars, flow, classes, listed, workers: int, rows_path, stability_path, only=None, log=print) -> None:
    """Runs `plan()`, appending each run's rows to `rows_path` as it finishes and skipping runs
    already there, so an interrupted stage resumes. The 37-market baseline also writes its first
    `STABILITY_PATHS` paths' rows to `stability_path`. `only` limits the stage to those variants."""
    grid = floor_grid(GRID_LOW, GRID_HIGH)
    done = {(r["variant"], int(r["markets"])) for r in read_rows(rows_path)} if rows_path.exists() else set()
    for variant, n, paths in plan():
        if (variant, n) in done or (only and variant not in only):
            continue
        slices = STABILITY_PATHS if (variant, n) == ("baseline", 37) else ()
        rows = run_variant(scenario, markets, bars, flow, classes, listed, n, variant, paths, DOLLAR_GRID if variant in DOLLAR_PATHS else grid, workers_for(n, workers), slices)
        full = [r for r in rows if r["paths"] == paths]
        append_rows(rows_path, full)
        if slices:
            append_rows(stability_path, [r for r in rows if r["paths"] in slices])
        log(f"{variant} {n} markets, {paths} paths done")


def append_rows(path, rows: list[dict]) -> None:
    new = not path.exists()
    with open(path, "a", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(rows[0]))
        if new:
            writer.writeheader()
        writer.writerows(rows)


def retained_nav(rows: list[dict], target: float = TARGET, refusal: float | None = REFUSAL_LIMIT) -> dict:
    """From one (variant, count)'s rows: the lowest floor that serves the flow and keeps house
    cash below zero in at most `target` of paths, here and at every higher grid floor. None when
    the top of the grid does not."""
    rows = sorted(rows, key=lambda r: r["floor"])
    serve = 0.0 if refusal is None else next((r["floor"] for i, r in enumerate(rows) if all(x["refused_share_directional"] <= refusal for x in rows[i:])), None)
    safe = next((r["floor"] for i, r in enumerate(rows) if all(x["p_short"] <= target for x in rows[i:])), None)
    both = None if serve is None or safe is None else max(serve, safe)
    at = next((r for r in rows if r["floor"] == both), None)
    return {"serve_floor": serve, "safe_floor": safe, "retained_nav": both, "row": at}


def standard_error(p: float, n: int) -> float:
    """The binomial standard error of a share of `n` paths."""
    return float(np.sqrt(max(p * (1 - p), 0.0) / n))


def read_rows(path) -> list[dict]:
    with open(path) as fh:
        rows = list(csv.DictReader(fh))
    for row in rows:
        for k, v in row.items():
            try:
                row[k] = float(v)
            except ValueError:
                pass
    return rows
