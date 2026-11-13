"""The cap study's floor calibration (#111) at the launch pricing bundle, for accounts at one tier.

The study's stages run as `research/cap-calibration` runs them, with every fill priced through its
market's spread bands at the tier's best-price size and every close paying the tier's fee
(`caps/pricing.py`). The tier distribution of the flow is unknown (the derived flow has no account
volumes), so each stage runs at the two ends of the tier range: every account at tier 0, a fresh
wallet, which pays the most spread and fee and takes at most 20x; and every account at tier 3, which
pays the least and takes the market's own leverage. Every account starts at tier 0, and tier 2 takes
five days of counted volume, so the house's first days, when the floors carry it, are tier 0's.

A floor set meets a target only if it meets it at both ends (`propose`).
"""

import csv
import json
from dataclasses import replace

import numpy as np

from caps import pricing
from caps.common import STUDIES, genesis, use

BRACKET = (0, 3)  # the ends of the tier range the stages run at
UNIFORM_FLOORS = (1e5, 3e5, 1e6, 3e6, 1e7, 3e7)  # the same floor on every market
TARGET = 0.01


def derived():
    return STUDIES["cap"] / "data" / "derived"


def inputs(tier: int):
    """The cap model's bars, flow and markets for accounts at `tier`, and the scenario fields that
    price its fills: the genesis's 34 launch markets, its margins, and the tier's pricing."""
    use("cap")
    from model import prices, simulate
    from model.expansion import require_profiles
    from model.flow import load, load_mix, with_genesis_profiles

    g = genesis()
    path = derived()
    bars, symbols = prices.load_bars(path / "hourly_bars.npz")
    listed = {m["symbol"] for m in g["markets"]}
    missing = sorted(listed - set(symbols))
    if missing:
        raise ValueError(f"no hourly bars for genesis markets {missing}")
    keep = [i for i, s in enumerate(symbols) if s in listed]
    bars, symbols = bars[:, keep], [symbols[i] for i in keep]
    specs = [m for m in g["markets"] if m["symbol"] in symbols]
    flow = with_genesis_profiles(load(path), specs)
    with (path / "benchmarks.csv").open() as fh:
        benchmarks = {r["benchmark"]: float(r["bps"]) for r in csv.DictReader(fh)}
    markets = simulate.load_markets(g, symbols, load_mix(path, symbols), bars, benchmarks)
    with (path / "papertrade_flow.csv").open() as fh:
        rows = list(csv.DictReader(fh))
    labels = sorted({r["leverage_bin"] for r in rows}, key=lambda b: int(b.split("-")[0]))
    markets, flow = pricing.with_tier(markets, flow, specs, tier, pricing.bin_leverage(rows), labels)
    require_profiles(markets, flow)
    pricing.check_ceilings(g)
    by_symbol = {s["symbol"]: s for s in specs}
    priced = {"spread_bands": tuple(pricing.spread_bands(by_symbol[s], tier) for s in markets.symbols),
              "fee_schedule": pricing.fee_schedule(tier, g["params"])}
    return bars, flow, markets, priced


def scenario(tier: int, priced: dict, paths: int, batch_paths: int, seed: int = 1, **changes):
    from model import simulate

    return simulate.Scenario(name=f"tier {tier}", seed=seed, paths=paths, batch_paths=batch_paths, **priced, **changes)


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


def candidate_lanes(sweeps: dict[int, list[dict]], symbols: list[str]) -> list[tuple[str, float, np.ndarray]]:
    """Every floor set the joint run compares, as (family, level, floors): each measure's risk-level
    floors from each end's per-market sweep (`experiments.level_floors`), and the uniform floors."""
    from model import experiments

    out = []
    for tier, sweep in sweeps.items():
        for key in experiments.MEASURES:
            for level, floors in zip(experiments.LEVELS, experiments.level_floors(sweep, symbols, key)):
                out.append((f"{key}@tier{tier}", float(level), floors))
    for floor in UNIFORM_FLOORS:
        out.append(("uniform", floor, np.full(len(symbols), floor)))
    return out


def joint(scenario, markets, bars, flow, lanes: list[tuple[str, float, np.ndarray]], workers: int) -> list[dict]:
    """All markets together on the same paths at every floor set in `lanes`, each distinct set run
    once; a row per lane."""
    from model import experiments, simulate

    unique, index = distinct([f for _, _, f in lanes])
    (out,) = simulate.run_many([(replace(scenario, name=f"{scenario.name} joint"), markets, bars, flow, unique, unique.sum(axis=1))], workers)
    rows = experiments.joint_rows(markets, out, unique, unique.sum(axis=1), list(range(len(unique))))
    return [{"family": family, **rows[i], "level": level} for (family, level, _), i in zip(lanes, index)]


def distinct(floor_sets: list[np.ndarray]) -> tuple[np.ndarray, list[int]]:
    """The distinct floor sets, (n, markets), and each given set's row among them."""
    unique, index = np.unique(np.array(floor_sets), axis=0, return_inverse=True)
    return unique, [int(i) for i in np.ravel(index)]


REFUSAL_LIMIT = 0.01  # a floor set serves a market's flow when it refuses at most this share of its directional notional over 90 days (as `model/retained.py`)


def worst_market_refusal(row: dict) -> float:
    """The largest share of any market's directional opening notional the floor set refuses over the
    90 days."""
    refused = row["refused_by_market"]
    refused = json.loads(refused) if isinstance(refused, str) else refused
    return max(refused.values())


def propose(joints: dict[int, list[dict]], key: str, target: float = TARGET, refusal: float = REFUSAL_LIMIT) -> dict | None:
    """Of the floor sets whose `key` meets `target` at both ends of the tier range, the smallest in
    total that serves every market's flow at both, refusing at most `refusal` of each market's
    directional notional over the 90 days; when none does, the one whose worst market refuses the
    least. Each floor dollar is unbacked exposure the house may carry at zero NAV, so of the sets that
    meet the target and serve the flow the least is taken. None when no set meets the target. The
    rows of each end are matched by family and level, and the family and level break a tie between
    identical floor sets."""
    by_end = {tier: {(r["family"], r["level"]): r for r in rows} for tier, rows in joints.items()}
    common = set.intersection(*(set(rows) for rows in by_end.values()))
    meeting = [k for k in common if all(by_end[t][k][key] <= target for t in by_end)]
    if not meeting:
        return None
    worst = {k: max(worst_market_refusal(by_end[t][k]) for t in by_end) for k in meeting}
    total = {k: by_end[BRACKET[0]][k]["total_floor"] for k in meeting}
    serving = [k for k in meeting if worst[k] <= refusal]
    best = min(serving, key=lambda k: (total[k], worst[k], k)) if serving else min(meeting, key=lambda k: (worst[k], total[k], k))
    return {"family": best[0], "level": best[1], "serves_flow": bool(serving), "worst_market_refusal": worst[best],
            "rows": {t: by_end[t][best] for t in by_end}}


def market_floors(row: dict, symbols: list[str]) -> np.ndarray:
    floors = row["market_floors"]
    floors = json.loads(floors) if isinstance(floors, str) else floors
    return np.array([floors[s] for s in symbols])


def total_sweep(scenario, markets, bars, flow, floors: np.ndarray, workers: int) -> list[dict]:
    """The proposed market floors with the total floor at `experiments.TOTAL_FRACTIONS` of their
    sum, on the same paths."""
    from model import experiments, simulate

    lanes = np.repeat(floors[None, :], len(experiments.TOTAL_FRACTIONS), axis=0)
    totals = experiments.TOTAL_FRACTIONS * floors.sum()
    (out,) = simulate.run_many([(replace(scenario, name=f"{scenario.name} total"), markets, bars, flow, lanes, totals)], workers)
    return experiments.joint_rows(markets, out, lanes, totals, experiments.TOTAL_FRACTIONS.tolist())


def write_rows(path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    flat = [{k: (json.dumps(v) if isinstance(v, dict) else v) for k, v in r.items()} for r in rows]
    keys = list(dict.fromkeys(k for r in flat for k in r))
    with open(path, "w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=keys)
        writer.writeheader()
        writer.writerows(flat)


def per_market(scenario, markets, bars, flow, workers: int) -> list[dict]:
    """Each market alone, at its own share of the flow and its own bands, over the cap study's floor
    grid with no total cap (`experiments.per_market`)."""
    from model import experiments, simulate

    floors = experiments.MARKET_FLOORS[:, None]
    total = np.full(len(experiments.MARKET_FLOORS), experiments.NO_TOTAL_LIMIT)
    tasks = [(replace(scenario, spread_bands=(scenario.spread_bands[i],)), markets.subset([i]), bars, flow, floors, total)
             for i in range(len(markets.symbols))]
    rows = []
    for symbol, out in zip(markets.symbols, simulate.run_many(tasks, workers)):
        for lane, floor in enumerate(experiments.MARKET_FLOORS):
            rows.append({"symbol": symbol, "floor": float(floor), **experiments.lane_stats(out, lane)})
    return rows


def income(scenario, markets, bars, flow, paths: int, grid: np.ndarray, workers: int) -> list[dict]:
    """The retained-NAV run (`model/retained.py`, its baseline at the launch markets): the house
    opens at each grid floor as its capital, sweeps everything above it every hour, and trades under
    the genesis's NAV-share caps with no dollar floors; a row per floor with what it distributed."""
    from model import retained, simulate

    n = len(markets.symbols)
    run = replace(scenario, name=f"{scenario.name} income", paths=paths, batch_paths=retained.batch_paths(n), sweep_epoch_h=1)
    floors, total = np.zeros((len(grid), n)), np.zeros(len(grid))
    (out,) = simulate.run_many([(run, replace(markets, shared=np.ones(n)), bars, flow, floors, total, grid)], workers)
    return retained.lane_rows(out, "baseline", n, grid)


SENSITIVITIES = {  # name -> scenario changes; "target flow" changes the flow (`calib.target`)
    "baseline": {}, "target flow": {}, "flow x0.25": {"flow_scale": 0.25}, "no farming": {"hedged_scale": 0.0},
    "vol x2": {"vol_scale": 2.0}, "one client direction": {"common_direction": True},
}
SENSITIVITY_SCALES = (0.3, 1.0)  # the proposed floors scaled


def sensitivity(scenario, markets, bars, flow, target_flow, floors: np.ndarray, workers: int) -> list[dict]:
    """The proposed floors, and the same scaled down, under each of `SENSITIVITIES`, on the same paths."""
    from model import experiments, simulate

    lanes = np.array(SENSITIVITY_SCALES)[:, None] * floors[None, :]
    tasks = [(replace(scenario, name=name, **change), markets, bars, target_flow if name == "target flow" else flow, lanes, lanes.sum(axis=1))
             for name, change in SENSITIVITIES.items()]
    rows = []
    for name, out in zip(SENSITIVITIES, simulate.run_many(tasks, workers)):
        for lane, scale in enumerate(SENSITIVITY_SCALES):
            rows.append({"scenario": name, "floor_scale": scale, **experiments.lane_stats(out, lane)})
    return rows
