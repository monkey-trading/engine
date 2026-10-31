"""Entry point for the cap calibration: fetch, derive, simulate, and report.

Run with `python -I run.py <command>` from any directory; every path is relative to this file.
`all` runs every simulation stage and the report from the committed derived data. Each stage's
paths and batch size default to those of the committed results; the batch size is part of the
random stream, so a different one gives different (equally valid) paths.
"""

import argparse
import hashlib
import json
import sys
import time
from dataclasses import asdict, replace
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

RAW = ROOT / "data" / "raw"
DERIVED = ROOT / "data" / "derived"
RESULTS = ROOT / "results"
STAGES = ("per-market", "joint", "sensitivity")
DEFAULTS = {"per-market": (2000, 500), "joint": (2000, 25), "sensitivity": (1000, 25)}  # paths, batch paths
TOXICITY_MC_TRIALS = 40_000  # benign false-positive trials per market and threshold
BASE_TARGET = 0.01  # the total-floor sweep and the sensitivities start from the floors chosen for this target
SENSITIVITY_MEASURE = "p_short"  # whose choice at BASE_TARGET the sensitivities start from


def inputs():
    import numpy as np

    from catalogue import load_genesis
    from model import prices, simulate
    from model.flow import load, load_mix

    bars, symbols = prices.load_bars(DERIVED / "hourly_bars.npz")
    flow = load(DERIVED)
    benchmarks = {r["benchmark"]: float(r["bps"]) for r in read_rows(DERIVED / "benchmarks.csv")}
    markets = simulate.load_markets(load_genesis(), symbols, load_mix(DERIVED, symbols), bars, benchmarks)
    return np, simulate, bars, flow, markets


def manifest(args, scenario) -> dict:
    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()[:16] for p in sorted(DERIVED.iterdir()) if p.is_file()}
    record = {"command": sys.argv[1:], "derived_sha256": hashes, "workers": getattr(args, "workers", None)}
    if scenario is not None:
        record["scenario"] = asdict(scenario)
    return record


def read_rows(path: Path) -> list[dict]:
    import csv

    with path.open() as fh:
        rows = list(csv.DictReader(fh))
    for row in rows:
        for k, v in row.items():
            try:
                row[k] = float(v)
            except ValueError:
                pass
    return rows


def simulate_stage(stage: str, args) -> None:
    from model import experiments

    np, simulate, bars, flow, markets = inputs()
    paths, batch_paths = DEFAULTS[stage]
    scenario = simulate.Scenario(seed=args.seed, paths=args.paths or paths, batch_paths=args.batch_paths or batch_paths)
    RESULTS.mkdir(exist_ok=True)
    started = time.time()
    if stage == "per-market":
        rows = experiments.per_market(scenario, markets, bars, flow, args.workers)
        experiments.write_rows(RESULTS / "per_market_sweep.csv", rows)
    elif stage == "joint":
        sweep = read_rows(RESULTS / "per_market_sweep.csv")
        families = experiments.joint(scenario, markets, bars, flow, sweep, args.workers)
        for family, rows in families.items():
            experiments.write_rows(RESULTS / f"joint_{family}.csv", rows)
        base_rows = {}
        for key in experiments.MEASURES:
            options = experiments.candidates(families, key)
            base_rows[key] = experiments.chosen(options, key, BASE_TARGET) or min(options, key=lambda r: r["total_floor"])
        for key, rows in experiments.total_sweep(scenario, markets, bars, flow, base_rows, args.workers).items():
            for row in rows:
                row["base_family"] = base_rows[key]["family"]
                row["base_level"] = base_rows[key]["level"]
            experiments.write_rows(RESULTS / f"joint_total_{key}.csv", rows)
    elif stage == "sensitivity":
        families = {f: read_rows(RESULTS / f"joint_{f}.csv") for f in (SENSITIVITY_MEASURE, "uniform")}
        options = experiments.candidates(families, SENSITIVITY_MEASURE)
        chosen = experiments.chosen(options, SENSITIVITY_MEASURE, BASE_TARGET)
        met = chosen is not None
        chosen = chosen or min(options, key=lambda r: r["total_floor"])
        floors = experiments.market_floors(chosen, markets.symbols)
        out = experiments.sensitivity(scenario, markets, bars, flow, floors, args.workers)
        for row in out:
            row["base_family"] = chosen["family"]
            row["base_level"] = chosen["level"]
            row["base_meets_target"] = met
            row["base_measure"] = SENSITIVITY_MEASURE
        experiments.write_rows(RESULTS / "sensitivity.csv", out)
    record = manifest(args, scenario) | {"seconds": round(time.time() - started)}
    (RESULTS / f"manifest_{stage}.json").write_text(json.dumps(record, indent=1, default=str))


def toxicity_stage(experiments, np, scenario, markets, bars, flow, args) -> dict:
    """The toxicity calibration: fill-size quantiles per market from the observed size
    distribution, markout scales from the bars' Parkinson volatility, then memory, prior, tier
    thresholds and spreads from the drift, climb, and benign false-positive bounds. Writes the
    tables and the choice."""
    import json

    from catalogue import load_genesis
    from model import toxicity

    genesis = load_genesis()
    symbols = list(markets.symbols)
    sigma = toxicity.sigma_markout_bps(bars)
    sizes = toxicity.size_quantiles(flow.sizes, genesis)
    rows = []
    for m, symbol in enumerate(symbols):
        if symbol not in sizes:
            continue
        rows.append(
            {
                "symbol": symbol,
                "sigma_markout_bps": round(float(sigma[m]), 4),
                **{k: round(v, 0) for k, v in sizes[symbol].items()},
            }
        )
    memory_choice = toxicity.choose_memory(sigma, symbols, sizes)
    prior_choice = toxicity.choose_prior(memory_choice["memory_usd"], sizes, symbols)
    tier_choice = toxicity.choose_tiers(sigma, symbols, sizes, memory_choice["memory_usd"], prior_choice["prior_usd"], args.seed, TOXICITY_MC_TRIALS)
    # per-market drift at the chosen memory, for the table
    for row in rows:
        m = symbols.index(row["symbol"])
        for q in ("n50", "n99"):
            row[f"drift_{q}_bps"] = round(toxicity.drift_bound_sigma(sigma[m], row[q], memory_choice["memory_usd"]), 4)
    experiments.write_rows(RESULTS / "toxicity_markets.csv", rows)
    choice = {
        "toxicity": {
            "horizon_ms": toxicity.HORIZON_MS,
            "prior_usd": prior_choice["prior_usd"],
            "memory_usd": memory_choice["memory_usd"],
            "tiers": [
                {"min_score_bps": tier_choice["tier1_bps"], "extra_spread_bps": tier_choice["spread1_bps"]},
                {"min_score_bps": tier_choice["tier2_bps"], "extra_spread_bps": tier_choice["spread2_bps"]},
            ],
        },
        "method": {"memory": memory_choice, "prior": prior_choice, "tiers": tier_choice},
    }
    (RESULTS / "toxicity_choice.json").write_text(json.dumps(choice, indent=1))
    return {"toxicity_mc_trials": TOXICITY_MC_TRIALS}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    pt = sub.add_parser("fetch-papertrade", help="pull papertrade.xyz Exchange logs from HyperEVM")
    pt.add_argument("--to-block", type=int, help="last block; defaults to the manifest's, else the latest")
    sub.add_parser("fetch-markets", help="pull public price history and volume for the launch markets")
    sub.add_parser("derive-papertrade", help="decode papertrade positions into the behaviour dataset")
    sub.add_parser("derive-markets", help="build the hourly price dataset, market statistics, and mix")
    tx = sub.add_parser("toxicity", help="calibrate the toxicity parameters (prior, memory, tiers); writes results/toxicity_*")
    tx.add_argument("--seed", type=int, default=1)
    for name in (*STAGES, "all"):
        sim = sub.add_parser(name, help=f"run the {name} stage" if name != "all" else "run every stage and the report")
        sim.add_argument("--workers", type=int, default=4)
        sim.add_argument("--paths", type=int, help="paths per run; defaults per stage (DEFAULTS)")
        sim.add_argument("--seed", type=int, default=1)
        sim.add_argument("--batch-paths", type=int, help="paths per worker job, part of the random stream; defaults per stage (DEFAULTS)")
    sub.add_parser("report", help="write results/report.md from the stage outputs")
    args = parser.parse_args()

    if args.command == "fetch-papertrade":
        from fetch import papertrade

        papertrade.fetch(RAW / "papertrade", args.to_block)
    elif args.command == "fetch-markets":
        from fetch import markets

        markets.fetch(RAW / "markets")
    elif args.command == "derive-papertrade":
        from derive import papertrade

        papertrade.derive(RAW / "papertrade", RAW / "markets", DERIVED)
    elif args.command == "derive-markets":
        from derive import markets

        markets.derive(RAW / "markets", DERIVED)
    elif args.command in STAGES:
        simulate_stage(args.command, args)
    elif args.command == "toxicity":
        class ToxArgs:
            pass

        tox_args = ToxArgs()
        tox_args.seed = args.seed
        from model import experiments as experiments_mod

        np_, sim, bars, flow, markets = inputs()
        started = time.time()
        extras = toxicity_stage(experiments_mod, np_, None, markets, bars, flow, tox_args)
        record = {
            "command": sys.argv[1:],
            "scenario": None,  # no Monte Carlo: the calibration is exact recurrences and trials
            "derived_sha256": manifest(args, None)["derived_sha256"],
            "seconds": round(time.time() - started),
        } | extras
        (RESULTS / "manifest_toxicity.json").write_text(json.dumps(record, indent=1, default=str))
    elif args.command == "all":
        for stage in STAGES:
            simulate_stage(stage, args)
        from model import report

        report.write(DERIVED, RESULTS)
    elif args.command == "report":
        from model import report

        report.write(DERIVED, RESULTS)


if __name__ == "__main__":
    main()
