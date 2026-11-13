"""The launch exposure-cap floors and stress levels at the launch pricing bundle (#500, E1).

Usage (from a virtualenv with requirements.txt, in this directory):
    python -I run.py sweep --tier 0|3        each market alone over the floor grid, at a tier's pricing
    python -I run.py joint --tier 0|3 [--flow target]   every candidate floor set together (needs both sweeps)
    python -I run.py propose                 the floor sets that meet each target at both tiers
    python -I run.py total --tier 0|3        the total floor below the proposed floors' sum
    python -I run.py stress --tier 0|3       the utilisation levels at the proposed floors
    python -I run.py sensitivity --tier 0|3  the proposed floors under the cap study's main sensitivities
    python -I run.py income --tier 0|3 --flow current|target   retained NAV and the honest share
    python -I run.py gaps                    paired positions across session gaps
    python -I run.py unprice                 paired positions across unpriced moves at tier 2
    python -I run.py report                  results/report.md

Each stage reads committed data and earlier stages' results and is deterministic; each house-model
stage writes `results/manifest_<stage>[_tier<n>].json` with its scenario, inputs and run time.
"""

import argparse
import hashlib
import json
import sys
import time
from dataclasses import asdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from caps.common import RESULTS  # noqa: E402

PATHS = {"sweep": (2000, 500), "joint": (2000, 25), "total": (2000, 25), "stress": (2000, 25), "sensitivity": (1000, 25), "income": (1000, None)}  # paths, batch paths


def tier_dir(tier: int) -> Path:
    return RESULTS / f"tier{tier}"


def manifest(name: str, args, scenario, started: float, **extra) -> None:
    from caps import house

    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()[:16] for p in sorted(house.derived().iterdir()) if p.is_file()}
    record = {"command": sys.argv[1:], "derived_sha256": hashes, "workers": args.workers, "seconds": round(time.time() - started)}
    if scenario is not None:
        record["scenario"] = asdict(scenario)
    (RESULTS / f"manifest_{name}.json").write_text(json.dumps(record | extra, indent=1, default=str))


def stage_sweep(args):
    from caps import house

    started = time.time()
    bars, flow, markets, priced = house.inputs(args.tier)
    sc = house.scenario(args.tier, priced, *PATHS["sweep"])
    rows = house.per_market(sc, markets, bars, flow, args.workers)
    house.write_rows(tier_dir(args.tier) / "per_market_sweep.csv", rows)
    manifest(f"sweep_tier{args.tier}", args, sc, started)


def target_flow(flow):
    """The cap study's flow reshaped to the owner's target client (`calib.target`)."""
    import csv

    from caps import house
    from caps.common import use_spread

    use_spread()
    from calib import target

    with (house.derived() / "papertrade_holding.csv").open() as fh:
        return target.target_flow(flow, list(csv.DictReader(fh)))


def joint_name(flow: str) -> str:
    return "joint.csv" if flow == "current" else f"joint_{flow}.csv"


def stage_joint(args):
    """Every candidate floor set together, on the current flow or, with `--flow target`, the target
    client's."""
    from caps import house

    started = time.time()
    bars, flow, markets, priced = house.inputs(args.tier)
    if args.flow == "target":
        flow = target_flow(flow)
    sweeps = {t: house.read_rows(tier_dir(t) / "per_market_sweep.csv") for t in house.BRACKET}
    lanes = house.candidate_lanes(sweeps, markets.symbols)
    sc = house.scenario(args.tier, priced, *PATHS["joint"])
    rows = house.joint(sc, markets, bars, flow, lanes, args.workers)
    house.write_rows(tier_dir(args.tier) / joint_name(args.flow), rows)
    suffix = "" if args.flow == "current" else f"_{args.flow}"
    manifest(f"joint_tier{args.tier}{suffix}", args, sc, started, lanes=len(lanes), flow=args.flow)


def stage_propose(args):
    from caps import house

    use_cap()
    from model import experiments

    out = {}
    for flow in ("current", "target"):
        if not all((tier_dir(t) / joint_name(flow)).exists() for t in house.BRACKET):
            continue
        joints = {t: house.read_rows(tier_dir(t) / joint_name(flow)) for t in house.BRACKET}
        for key in experiments.MEASURES:
            for target in experiments.TARGETS:
                choice = house.propose(joints, key, target)
                out[f"{key}@{target}" + ("" if flow == "current" else f"@{flow}")] = choice
                print(flow, key, target, None if choice is None else (choice["family"], choice["level"], {t: round(r["total_floor"]) for t, r in choice["rows"].items()}))
    (RESULTS / "proposal.json").write_text(json.dumps(out, indent=1, default=float))


def proposed_floors(symbols: list[str]):
    """The proposed market floors: the house-cash measure's choice at 1% (`propose`)."""
    from caps import house

    choice = json.loads((RESULTS / "proposal.json").read_text())[f"{PROPOSAL_MEASURE}@{house.TARGET}"]
    if choice is None:
        raise ValueError("no floor set meets the target at both tiers")
    return house.market_floors(next(iter(choice["rows"].values())), symbols), choice


PROPOSAL_MEASURE = "p_short"


def stage_total(args):
    from caps import house

    started = time.time()
    bars, flow, markets, priced = house.inputs(args.tier)
    floors, choice = proposed_floors(markets.symbols)
    sc = house.scenario(args.tier, priced, *PATHS["total"])
    rows = house.total_sweep(sc, markets, bars, flow, floors, args.workers)
    house.write_rows(tier_dir(args.tier) / "total.csv", rows)
    manifest(f"total_tier{args.tier}", args, sc, started, proposal=[choice["family"], choice["level"]])


def stage_stress(args):
    """The utilisation levels at the proposed floors, beside uniform $1M and $10M floors and the
    floors where each market's refusals end at this tier."""
    import numpy as np

    from caps import house

    started = time.time()
    bars, flow, markets, priced = house.inputs(args.tier)
    from catalogue import load_genesis
    from model import stress
    from model.experiments import LEVELS, level_floors

    floors, choice = proposed_floors(markets.symbols)
    sweep = house.read_rows(tier_dir(args.tier) / "per_market_sweep.csv")
    # Each market's floor where its refusals end, or the grid's top where they never do (risk level 1).
    binding = level_floors(sweep, markets.symbols, PROPOSAL_MEASURE)[LEVELS.index(1.0)]
    families = {"proposed": floors, "uniform_1M": np.full(len(floors), 1e6), "uniform_10M": np.full(len(floors), 1e7), "binding": binding}
    sc = house.scenario(args.tier, priced, *PATHS["stress"])
    gaps = stress.gap_tables(bars, markets.symbols)
    rows = stress.utilisation(sc, markets, bars, flow, families, gaps, args.workers)
    house.write_rows(tier_dir(args.tier) / "stress_utilisation.csv", rows)
    mix = {s: float(w) for s, w in zip(markets.symbols, markets.mix)}
    choices = {name: stress.choose_utilisation([r for r in rows if r["family"] == name], mix) for name in families if name != "uniform_1M"}
    (tier_dir(args.tier) / "stress_choice.json").write_text(json.dumps(choices, indent=1))
    params = load_genesis()["params"]
    manifest(f"stress_tier{args.tier}", args, sc, started, proposal=[choice["family"], choice["level"]],
             families={k: v.tolist() for k, v in families.items()}, stress_levels=params["stress_levels"])
    print(json.dumps(choices, indent=1))


def stage_sensitivity(args):
    """The proposed floors under the sensitivities (`house.SENSITIVITIES`)."""
    from caps import house

    started = time.time()
    bars, flow, markets, priced = house.inputs(args.tier)
    floors, choice = proposed_floors(markets.symbols)
    sc = house.scenario(args.tier, priced, args.paths or PATHS["sensitivity"][0], PATHS["sensitivity"][1])
    rows = house.sensitivity(sc, markets, bars, flow, target_flow(flow), floors, args.workers)
    house.write_rows(tier_dir(args.tier) / "sensitivity.csv", rows)
    manifest(f"sensitivity_tier{args.tier}", args, sc, started, proposal=[choice["family"], choice["level"]])


INCOME_FLOORS = slice(3, 9)  # the retained grid from $16.4M to $172M: #623 retained $107M, and tier 0 trades less notional


def stage_income(args):
    from caps import house

    started = time.time()
    bars, flow, markets, priced = house.inputs(args.tier)
    from model import retained

    if args.flow == "target":
        flow = target_flow(flow)
    grid = retained.floor_grid(retained.GRID_LOW, retained.GRID_HIGH)[INCOME_FLOORS]
    paths = args.paths or PATHS["income"][0]
    sc = house.scenario(args.tier, priced, paths, retained.batch_paths(len(markets.symbols)))
    rows = house.income(sc, markets, bars, flow, paths, grid, retained.workers_for(len(markets.symbols), args.workers))
    answer = retained.retained_nav(rows)
    path = RESULTS / "income.json"
    out = json.loads(path.read_text()) if path.exists() else {}
    out[f"tier {args.tier}, {args.flow}"] = {"tier": args.tier, "flow": args.flow, "paths": paths, "retained_nav": answer["retained_nav"],
                                              "row": answer["row"], "rows": rows}
    path.write_text(json.dumps(out, indent=1, default=float))
    manifest(f"income_tier{args.tier}_{args.flow}", args, sc, started)
    print(args.tier, args.flow, answer["retained_nav"], answer["row"] and {k: round(v) for k, v in answer["row"].items() if k.endswith("_mean")})


def stage_gaps(args):
    """Paired positions across session gaps on the FX, index and commodity markets, at each tier's
    leverage, spread and fee (`caps/gaps.py`)."""
    from caps import gaps, house, pricing
    from caps.common import genesis

    bars, _, markets, _ = house.inputs(3)
    g = genesis()
    specs = {m["symbol"]: m for m in g["markets"]}
    fin_long = dict(zip(markets.symbols, markets.fin_long))
    fin_short = dict(zip(markets.symbols, markets.fin_short))
    rows = gaps.pair_rows(bars, markets.symbols, specs, fin_long, fin_short, pricing.TIERS, g["params"]["profit_fee_bps"] / 1e4)
    years = bars.shape[0] / 8760
    table = gaps.summary(rows, years)
    house.write_rows(RESULTS / "session_gaps.csv", table)
    worst = sorted(rows, key=lambda r: -r["payoff"])[:20]
    weekend = gaps.breakeven(bars, markets.symbols, specs, fin_long, fin_short, g["params"]["profit_fee_bps"] / 1e4, pricing.TIERS[3])
    (RESULTS / "session_gaps.json").write_text(json.dumps({"years": years, "closures": len(rows) // len(pricing.TIERS), "largest": worst,
                                                           "weekend_breakeven": weekend}, indent=1, default=float))
    print({s: w["breakeven_margin_bps"] for s, w in weekend.items()})
    for r in table:
        if r["kind"] == "all":
            print(r["tier"], r["symbol"], round(r["closures_year"]), round(r["payoff_bps_year"], 1), round(r["payoff_per_margin_year"], 4), round(r["past_balance_share"], 4))


def _unprice_job(job):
    from caps import unprice

    h, s1, fees, terms = job
    rows = unprice.hour_rows(h, s1, fees, terms)
    stress = h.stratum == "stress"
    return rows, unprice.check_697(h, s1) if stress else 0.0, unprice.check_734(h, s1, terms) if stress else {}


def stage_unprice(args):
    """Paired positions across unpriced moves on BTC-USD and ETH-USD at 1% margin through tier-2 and
    tier-3 accounts (`caps/unprice.py`), on #549's sampled hours and 2025-10-10's four stress hours."""
    from multiprocessing import Pool

    from caps import pricing, unprice
    from caps.common import genesis, use

    use("rogue")
    from model.paths import load_hours
    from model.report import PAIR_NOTIONAL

    started = time.time()
    g = genesis()
    first = {m["symbol"]: m["spread_tiers"][0]["tenth_bps"] / 1e5 for m in g["markets"]}
    base = g["params"]["profit_fee_bps"] / 1e4
    fees = {t: base * (1 - pricing.TIERS[t].discount) for t in unprice.FEE_TIERS}
    terms = unprice.stress_terms(g["params"])
    hours = [h for h in load_hours() if h.market in unprice.MARKETS]
    rows, check, check_734 = [], 0.0, {o: 0.0 for o in unprice.OPENS}
    with Pool(args.workers) as pool:
        for out, c, c734 in sorted(pool.imap_unordered(_unprice_job, [(h, first[h.market], fees, terms) for h in hours], chunksize=2),
                                   key=lambda o: (o[0][0]["market"], o[0][0]["hour"])):  # a fixed summation order
            rows += out
            check += c
            for o, v in c734.items():
                check_734[o] += v
    rows.sort(key=lambda r: tuple(str(r[k]) for k in ("market", "hour", "tier", "opens", "closes", "spell_s", "trigger")))  # a fixed summation order
    out = {"pair_notional": PAIR_NOTIONAL, "margin": unprice.MARGIN, "fees": fees, "first_band": {m: first[m] for m in unprice.MARKETS},
           "stress_terms": terms, "hours": len(hours), "check_697_stress_hours": check * PAIR_NOTIONAL,
           "check_734_stress_hours": {o: v * PAIR_NOTIONAL for o, v in check_734.items()},
           "rules": unprice.tables(rows, PAIR_NOTIONAL), "seconds": round(time.time() - started)}
    (RESULTS / "unpriced_pairs.json").write_text(json.dumps(out, indent=1, default=float))
    print("#697's measure on the stress hours:", round(out["check_697_stress_hours"]), "#734's:", {o: round(v) for o, v in out["check_734_stress_hours"].items()})
    for r in out["rules"]:
        if r["spell_s"] == unprice.SPELL_S and r["trigger_bps_5s"] == unprice.TRIGGER * 1e4:
            print(r)


def stage_report(args):
    from caps import report

    use_cap()
    print(report.write(RESULTS)[:1500])


def use_cap():
    from caps.common import use

    use("cap")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("stage", choices=["sweep", "joint", "propose", "total", "stress", "sensitivity", "income", "gaps", "unprice", "report"])
    parser.add_argument("--tier", type=int, choices=(0, 3))
    parser.add_argument("--flow", choices=("current", "target"), default="current")
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--paths", type=int, default=None)
    args = parser.parse_args()
    if args.stage in ("sweep", "joint", "total", "stress", "sensitivity", "income") and args.tier is None:
        parser.error(f"{args.stage} needs --tier")
    RESULTS.mkdir(exist_ok=True)
    globals()[f"stage_{args.stage}"](args)


if __name__ == "__main__":
    main()
