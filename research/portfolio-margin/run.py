"""Portfolio margin's offset groups, shocks, quantile and floor (#914, for #913's decisions).

Usage (from a virtualenv with requirements.txt, in this directory):
    python -I run.py fetch       the daily history from 2016 (public series; raw pulls in gitignored data/raw/)
    python -I run.py derive      data/derived/daily_logclose.csv from them
    python -I run.py horizon     how far BTC-USD, ETH-USD and SOL-USD move over each horizon (50 ms grids)
    python -I run.py comove      every pair's co-movement at an hour, a day and across closures, and the group lags
    python -I run.py groups      fit the candidate groups, the floor share, the groups kept (results/groups.json)
    python -I run.py coverage    the final requirement's coverage under joint sampling, and the lone positions
    python -I run.py checks      crisis days, the alternatives, the bounds, the close-spread shortfalls
    python -I run.py outage      one member shut: matched opposite accounts, an operator choosing the timing
    python -I run.py proposal    results/proposal.toml, the backtest tables and results/report.md
    python -I run.py cadence     crypto's 50 ms grids read every 60 s against the hourly bars' one-jump reading (results/cadence.csv)
    python -I run.py sensitivity the groups stage again at other quantiles and horizons (results/sensitivity.csv)
    python -I run.py all         every stage after `derive`, from the committed data

Each stage reads committed data and earlier stages' results, is deterministic, and writes
`results/manifest_<stage>.json` with its inputs' hashes and run time.
"""

import argparse
import hashlib
import json
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from pm import io  # noqa: E402
from pm.common import CANDIDATES, DERIVED, RESULTS  # noqa: E402

STAGES = ("horizon", "comove", "groups", "coverage", "checks", "outage", "proposal", "cadence", "sensitivity")


def manifest(stage: str, started: float, **extra) -> None:
    from pm import context

    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()[:16] for p in sorted(DERIVED.iterdir()) if p.is_file()}
    record = {"command": sys.argv[1:], "derived_sha256": hashes, "seconds": round(time.time() - started),
              "quantile": context.QUANTILE, "horizon_s": context.HORIZON_S, "level": context.LEVEL, "seed": context.SEED}
    io.write_json(f"manifest_{stage}.json", record | extra)


def proposal_params():
    from pm import context

    return context.params_from(io.read_json("groups.json"))


def stage_horizon(ctx):
    from pm import horizon

    t = horizon.table()
    io.write_rows("horizon.csv", t)
    out = {"scale": {f"{q}_{h}s": horizon.ratio(t, h, q) for q in (0.99, 0.995, 0.999) for h in horizon.HORIZONS[:-1]}}
    io.write_json("horizon_scale.json", out)


def stage_comove(ctx):
    from pm import comove

    pairs = comove.pairs(ctx.hr)
    io.write_rows("comove_pairs.csv", pairs)
    io.write_rows("comove_lags.csv", comove.lags(ctx.hr, CANDIDATES))
    io.write_rows("comove_groups.csv", comove.group_summary(pairs, CANDIDATES))


def stage_groups(ctx):
    from pm import groups

    out = groups.run(ctx, CANDIDATES)
    rows = out.pop("rows")
    io.write_rows("groups_rows.csv", rows)
    io.write_rows("saving_by_allowance.csv", out.pop("saving_table"))
    io.write_json("groups.json", out)


def stage_coverage(ctx):
    from pm import context, coverage

    params = proposal_params()
    rows = coverage.run(ctx, params)
    io.write_rows("coverage_books.csv", rows)
    io.write_rows("coverage.csv", coverage.summarise(rows, ctx.q))
    io.write_rows("lone_leverage.csv", coverage.lone_leverage(ctx, params))
    # every candidate group, kept or not, at the proposal's share and at none and half, for BTC-USD and ETH-USD's 100x
    every = [coverage.lone_leverage(ctx, context.params_from(io.read_json("groups.json"), phi, kept_only=False), f"all_groups_phi{phi}")
             for phi in (0.0, 0.5)]
    io.write_rows("lone_leverage_all_groups.csv", [r for rows in every for r in rows])


def stage_checks(ctx):
    from pm import coverage, extra

    params = proposal_params()
    io.write_rows("crisis_days.csv", extra.crisis_days(ctx, params))
    io.write_rows("alternatives.csv", extra.alternatives(ctx, params))
    io.write_rows("bounds.csv", extra.bounds(ctx, params))
    io.write_rows("ceiling_shortfall.csv", coverage.ceiling_shortfall(ctx))
    io.write_rows("implied_horizon.csv", extra.implied_horizon(ctx))


def stage_outage(ctx):
    from pm import extra

    io.write_rows("outage.csv", extra.outage(ctx, proposal_params()))


def stage_proposal(ctx):
    import csv

    from pm import report

    g = io.read_json("groups.json")
    (RESULTS / "proposal.toml").write_text(report.toml(g))
    with (RESULTS / "groups_rows.csv").open() as fh:
        rows = [{k: (float(v) if k not in ("group", "pair") else v) for k, v in r.items()} for r in csv.DictReader(fh)]
    hvs, bt = report.backtest(rows, g)
    io.write_rows("hedged_vs_single.csv", hvs)
    io.write_rows("backtest.csv", bt)


def stage_cadence(ctx):
    from pm import cadence

    io.write_rows("cadence.csv", cadence.table())
    io.write_rows("cadence_z_of_bands.csv", cadence.z_of_bands(ctx))


def stage_sensitivity(ctx):
    from dataclasses import replace

    from pm import groups

    rows = []
    for q, h in ((0.99, 60), (0.995, 60), (0.999, 60), (0.995, 10), (0.995, 300), (0.995, 3600)):
        c = replace(ctx, q=q, h=h)
        out = groups.run(c, CANDIDATES)
        out.pop("rows")
        row = {"quantile": q, "horizon_s": h, "scale": c.scale, "kept": "|".join(n for n, g in out["groups"].items() if g["kept"])}
        for n, g in out["groups"].items():
            row[f"{n}_phi"], row[f"{n}_saving"], row[f"{n}_mean_rho_bps"] = g["phi"], g["saving_at_phi"], 1e4 * sum(g["rho"].values()) / len(g["rho"])
        rows.append(row)
    io.write_rows("sensitivity.csv", rows)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("stage", choices=("fetch", "derive", "all", *STAGES))
    args = ap.parse_args()
    if args.stage in ("fetch", "derive"):
        from pm import daily

        getattr(daily, args.stage)()
        return
    from pm import context

    ctx = context.make()
    for stage in [x for x in STAGES if x != "sensitivity"] if args.stage == "all" else (args.stage,):
        started = time.time()
        globals()[f"stage_{stage}"](ctx)
        manifest(stage, started)
        print(f"{stage}: {round(time.time() - started)} s", flush=True)


if __name__ == "__main__":
    main()
