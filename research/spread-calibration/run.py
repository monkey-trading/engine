"""Calibrates spreads, the holding-time fee and the toxicity design, and re-runs the studies they change.

Usage (from a virtualenv with requirements.txt, in this directory):
    python -I run.py spreads                    per-market lag moves and spread tiers
    python -I run.py latency [--workers 3]      the latency rule on every sampled hour
    python -I run.py scaled [--workers 3]       BTC-USD's latency rule at a range of spreads
    python -I run.py fee                        the holding-time fee's proposal and requirements
    python -I run.py frontier                   a family of fee schedules against latency takes
    python -I run.py fx_tiers                   what FX and index first tiers leave
    python -I run.py toxicity                   the toxicity score's calibration
    python -I run.py carrot                     the volume tiers and what recycling a wallet forfeits
    python -I run.py attacks [--workers 3]      #549's unpriced-spell measure at the calibrated spreads
    python -I run.py per_market --paths 2000    #500: BTC-USD and ETH-USD alone at 100x
    python -I run.py gaps                       close-out gaps at the first band's margins
    python -I run.py retained --paths 1000      #134's retained NAV on current and target flow
    python -I run.py report                     results/report.md

Each stage reads only committed data and earlier stages' results, and is deterministic.
"""

import argparse
import csv
import gzip
import io
import json
import sys
from multiprocessing import Pool
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
RESULTS = HERE / "results"


def write_gz(path: Path, rows: list[dict]):
    keys = sorted({k for r in rows for k in r})
    with path.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as gz, io.TextIOWrapper(gz, newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=keys)
        w.writeheader()
        w.writerows(rows)


def read_gz(path: Path) -> list[dict]:
    with gzip.open(path, "rt") as fh:
        return list(csv.DictReader(fh))


def stage_spreads(args):
    from calib import spreads, volatility

    moves = volatility.lag_moves()
    rows = spreads.calibrate(moves)
    (RESULTS / "lag_moves.json").write_text(json.dumps(moves, indent=1))
    (RESULTS / "spreads.json").write_text(json.dumps(rows, indent=1))
    (RESULTS / "spread_tiers.toml").write_text(spreads.toml_lines(rows))
    print(f"{len(rows)} markets; rho {moves['rho']}")


def _latency_job(args):
    """Both index treatments for one hour: Coinbase's trades as recorded, and held wherever they sit
    past the divergence bound from Kraken's (`divergence_clamped`), where the engine has no price."""
    from dataclasses import replace

    h, spread = args
    from calib import latency
    from model.paths import divergence_clamped, window_max, window_min

    from calib.jump import with_jump
    from model.stress import levels, min_spread

    held = replace(h, x=divergence_clamped(h.x, h.kraken, DIVERGENCE_BPS))
    rows = []
    for path, index in ((h, "proxy"), (held, "held")):
        for stress, fn in (("genesis", min_spread), ("jump", with_jump(levels))):
            rows += [dict(r, index=index, stress=stress) for r in latency.hour_rows(path, spread, window_max, window_min, fn)]
    return rows


DIVERGENCE_BPS = 25
# BTC-USD's latency strategy at these first-tier spreads, with no stress minimum: scaled by each
# market's lag move, they give the take a tighter first tier would leave in FX and index markets.
SCALED_SPREADS_BPS = (0.25, 0.5, 1.0, 2.0, 3.0, 5.0)


def _scaled_job(args):
    from dataclasses import replace

    h, spread_bps = args
    from calib import latency
    from model.paths import divergence_clamped, window_max, window_min

    held = replace(h, x=divergence_clamped(h.x, h.kraken, DIVERGENCE_BPS))
    rows = latency.hour_rows(held, spread_bps / 1e4, window_max, window_min, None)
    return [dict(r, spread_bps=spread_bps) for r in rows if r["lag_s"] in (0.25, 3.0)]


def stage_scaled(args):
    """BTC-USD's sampled hours at `SCALED_SPREADS_BPS`, for `fx_tiers` in the report."""
    from calib.common import use

    use("rogue")
    from model.paths import load_hours

    jobs = [(h, s) for h in load_hours() if h.market == "BTC-USD" and h.stratum != "stress" for s in SCALED_SPREADS_BPS]
    rows = []
    with Pool(args.workers) as pool:
        for i, out in enumerate(pool.imap_unordered(_scaled_job, jobs, chunksize=1), 1):
            rows += out
            if i % 26 == 0:
                print(f"{i}/{len(jobs)}", flush=True)
    rows.sort(key=lambda r: (r["spread_bps"], r["hour"], r["lag_s"], r["hold_s"], r["theta_bps"]))
    write_gz(RESULTS / "scaled_btc.csv.gz", rows)


def stage_latency(args):
    from calib.common import use

    use("rogue")
    from model.paths import load_hours

    tiers = {r["symbol"]: r["tiers"][0]["bps"] / 1e4 for r in json.loads((RESULTS / "spreads.json").read_text())}
    jobs = [(h, tiers[h.market]) for h in load_hours() if h.stratum != "stress"]
    rows = []
    with Pool(args.workers) as pool:
        for i, out in enumerate(pool.imap_unordered(_latency_job, jobs, chunksize=1), 1):
            rows += out
            if i % 20 == 0:
                print(f"{i}/{len(jobs)} hours", flush=True)
    rows.sort(key=lambda r: (r["index"], r["stress"], r["market"], r["hour"], r["lag_s"], r["hold_s"], r["theta_bps"]))
    write_gz(RESULTS / "latency_hours.csv.gz", rows)
    print(f"{len(rows)} rows")


def latency_year(lag_s: float, index: str = "held", stress: str = "genesis") -> dict:
    """(hold, theta) -> year sums (positive, negative) per unit of size, summed over the crypto
    markets, each hour weighted by its stratum and by the share of its starts a hold fits."""
    from calib.common import use

    use("rogue")
    from model.paths import population

    pop = population()
    rows = [r for r in read_gz(RESULTS / "latency_hours.csv.gz") if float(r["lag_s"]) == lag_s and r["index"] == index and r["stress"] == stress]
    counts = {}
    for r in rows:
        if r["hold_s"] == "1" and r["theta_bps"] == "1":
            counts[(r["market"], r["stratum"])] = counts.get((r["market"], r["stratum"]), 0) + 1
    out = {}
    for r in rows:
        strata, year_hours = pop[r["market"]]
        w = strata[int(r["stratum"])] / counts[(r["market"], r["stratum"])] * 8760 / year_hours / max(float(r["covered"]), 1e-9)
        key = (float(r["hold_s"]), float(r["theta_bps"]))
        pos, neg = out.get(key, (0.0, 0.0))
        out[key] = (pos + w * float(r["positive"]), neg + w * float(r["negative"]))
    return out


def latency_market_year(lag_s: float, market: str, index: str = "held", stress: str = "genesis") -> dict:
    """(hold, theta) -> one market's year sums (positive, negative, trades) per unit of size."""
    from calib.common import use

    use("rogue")
    from model.paths import population

    strata, year_hours = population()[market]
    rows = [r for r in read_gz(RESULTS / "latency_hours.csv.gz")
            if r["market"] == market and float(r["lag_s"]) == lag_s and r["index"] == index and r["stress"] == stress]
    counts = {}
    for r in rows:
        if r["hold_s"] == "1" and r["theta_bps"] == "1":
            counts[r["stratum"]] = counts.get(r["stratum"], 0) + 1
    out = {}
    for r in rows:
        w = strata[int(r["stratum"])] / counts[r["stratum"]] * 8760 / year_hours / max(float(r["covered"]), 1e-9)
        key = (float(r["hold_s"]), float(r["theta_bps"]))
        pos, neg, n = out.get(key, (0.0, 0.0, 0.0))
        out[key] = (pos + w * float(r["positive"]), neg + w * float(r["negative"]), n + w * float(r["trades"]))
    return out


# The fee's measured holds end at 30 minutes (the sample's hours leave no longer hold); past that the
# proposal falls linearly in the logarithm of the hold to the standard fee at two hours.
TAIL = {3600.0: 0.20, 7200.0: 0.10}
FAST_BUDGET = 10_000.0  # a day: what an honest node's fastest client may still make, near zero
SIZE = 6.9e6  # the operator's position a market, 69% of the $10M launch cap


# The launch criterion's yardstick before this study: #134's distributions at launch flow, without
# spread income, times the 20% share. The fee's requirements use it, so they do not depend on the
# retained re-run that uses the fee; `retained.json` gives the shares with spread income.
OPERATOR_SHARE_134 = 76_058_460.83 / 90 * 0.20
# The proposal (#614): the full fee up to 5 minutes, falling linearly in the logarithm of the hold to
# the standard 10% at 30 minutes, so strategies over seconds to single-digit minutes keep little of
# their gains while the target client's holds, 15 minutes or more, pay close to the standard fee.
PROPOSAL = {"peak": 1.0, "held_to_s": 300.0, "until_s": 1800.0}


def proposal_schedule() -> dict[float, float]:
    from calib import fee

    return fee.family(PROPOSAL["peak"], PROPOSAL["until_s"], PROPOSAL["held_to_s"])


def stage_fee(args):
    from calib import fee

    budget = OPERATOR_SHARE_134
    out = {"size_per_market": SIZE, "rogue_budget_per_day": budget, "fast_budget_per_day": FAST_BUDGET, "variants": {}}
    with (HERE.parent / "cap-calibration" / "data" / "derived" / "papertrade_holding.csv").open() as fh:
        holding = list(csv.DictReader(fh))
    for index in ("held", "proxy"):
        for stress in ("genesis", "jump"):
            years = {lag: latency_year(lag, index, stress) for lag in (0.25, 1.0, 3.0)}
            fast = fee.required(years[0.25], SIZE, FAST_BUDGET)
            rogue_ = fee.required(years[3.0], SIZE, budget)
            schedule = fee.envelope(fast, rogue_) | TAIL
            out["variants"][f"{index}/{stress}"] = {
                "required_fast_0.25s": {str(k): v for k, v in fast.items()},
                "required_rogue_3s": {str(k): v for k, v in rogue_.items()},
                "schedule": {str(k): v for k, v in schedule.items()},
                "take": {f"{lag}s": {str(k): v for k, v in fee.schedule_take(y, SIZE, schedule).items()} for lag, y in years.items()},
                "honest_cost": fee.honest_cost(holding, schedule),
            }
    # The proposal (`PROPOSAL`); the variants' envelopes are what latency arbitrage alone would ask.
    schedule = proposal_schedule()
    out["proposal"] = {str(k): v for k, v in schedule.items()}
    out["proposal_honest_cost"] = {"target": fee.honest_cost(holding, schedule, from_s=900.0),
                                   "short": fee.honest_cost(holding, schedule, to_s=900.0),
                                   "all": fee.honest_cost(holding, schedule)}
    (RESULTS / "fee.json").write_text(json.dumps(out, indent=1))
    for k, v in out["variants"].items():
        print(k, v["schedule"], {kk: round(vv, 3) for kk, vv in v["honest_cost"].items()})


def stage_retained(args):
    """#134's retained NAV at launch (37 markets, 1,000 paths) with every fill paying its market's
    best-tier spread and the profit fee falling with holding time, on the cap study's own model."""
    from calib.common import use

    use("cap")
    import catalogue
    from model import expansion, prices, retained, simulate
    from model.expansion import require_profiles
    from model.flow import load, load_mix, with_genesis_profiles

    derived = HERE.parent / "cap-calibration" / "data" / "derived"
    bars, symbols = prices.load_bars(derived / "hourly_bars.npz")
    genesis = catalogue.load_genesis()
    flow = with_genesis_profiles(load(derived), [m for m in genesis["markets"] if m["symbol"] in symbols])
    with (derived / "benchmarks.csv").open() as fh:
        benchmarks = {r["benchmark"]: float(r["bps"]) for r in csv.DictReader(fh)}
    markets = simulate.load_markets(genesis, symbols, load_mix(derived, symbols), bars, benchmarks)
    require_profiles(markets, flow)
    classes = expansion.asset_classes(derived)
    with (derived / "market_mix.csv").open() as fh:
        listed = {r["symbol"] for r in csv.DictReader(fh) if r["listed"] == "True"}
    tiers = {r["symbol"]: r["tiers"][0]["bps"] for r in json.loads((RESULTS / "spreads.json").read_text())}
    schedule = json.loads((RESULTS / "fee.json").read_text())["proposal"]
    fee_schedule = tuple(sorted((float(h), float(f)) for h, f in schedule.items()))
    pricing = {"spread_bps": tuple(tiers[s] for s in markets.symbols), "fee_schedule": fee_schedule}
    from calib import target

    with (derived / "papertrade_holding.csv").open() as fh:
        holding = list(csv.DictReader(fh))
    flows = {"current": flow, "target": target.target_flow(flow, holding)}
    grid = retained.floor_grid(retained.GRID_LOW, retained.GRID_HIGH)
    out = {"paths": args.paths, "target_within_hour_share": target.within_hour_split(holding), "keep_short": target.KEEP_SHORT,
           "variants": {}}
    for name, (flow_name, changes) in (("current flow, spreads and fee", ("current", pricing)),
                                       ("target flow, spreads and fee", ("target", pricing)),
                                       ("current flow, genesis pricing", ("current", {})),
                                       ("target flow, genesis pricing", ("target", {}))):
        scenario = simulate.Scenario(seed=1, paths=args.paths, batch_paths=retained.batch_paths(37), **changes)
        rows = retained.run_variant(scenario, markets, bars, flows[flow_name], classes, listed, 37, "baseline", args.paths, grid,
                                    retained.workers_for(37, args.workers))
        answer = retained.retained_nav(rows)
        row = answer["row"]
        out["variants"][name] = {"retained_nav": answer["retained_nav"], "row": row, "rows": rows,
                                 "operator_share_per_day": row["distributed_mean"] / 90 * 0.20 if row else None}
        print(name, answer["retained_nav"], row and row["distributed_mean"], row and row["spread_income_mean"], flush=True)
    (RESULTS / "retained.json").write_text(json.dumps(out, indent=1, default=float))


def stage_toxicity(args):
    """Per market, on the sampled hours: random fills' markouts at the score's horizon (an honest
    account's noise) and the fast latency trader's (0.25 s lead, trading moves past twice the
    best-tier spread), in z-units under a fixed year-wide sigma and under the trailing hour's sigma
    floored at half of it; then the fills a toxic account takes to reach each tier, the share of
    honest accounts that ever reach one, and what diluting the score costs."""
    import numpy as np

    from calib import toxicity
    from calib.common import use

    use("rogue")
    from model.paths import load_hours, population

    pop = population()
    tiers = {r["symbol"]: r["tiers"][0]["bps"] / 1e4 for r in json.loads((RESULTS / "spreads.json").read_text())}
    hours = [h for h in load_hours() if h.stratum != "stress"]
    counts = {}
    for h in hours:
        counts[(h.market, h.stratum)] = counts.get((h.market, h.stratum), 0) + 1
    per = {}
    for h in hours:
        rng = np.random.default_rng([h.hour, 615])
        span = h.analysis
        length = span.stop - span.start
        sigma_h = toxicity.hour_sigma(h.x, h.step_s, span.start, length)
        honest = toxicity.random_markouts(h.x, h.step_s, span.start, length, rng)
        k, lag = h.steps(toxicity.HORIZON_S), h.steps(0.25)
        t = np.arange(span.start, span.stop - k, h.steps(0.25))
        known = h.x[t] - h.x[t - lag]
        sel = np.abs(known) >= 2 * tiers[h.market]
        d = np.sign(known[sel])
        attack = d * (h.x[t[sel] + k] - h.x[t[sel] - lag])
        strata, _ = pop[h.market]
        w = strata[h.stratum] / counts[(h.market, h.stratum)]
        per.setdefault(h.market, []).append((w, sigma_h, honest, attack))
    out = {"horizon_s": toxicity.HORIZON_S, "half_life_days": toxicity.HALF_LIFE_DAYS, "min_fills": toxicity.MIN_FILLS,
           "weight_cap": toxicity.WEIGHT_CAP, "tiers": toxicity.Z_TIERS, "markets": {}}
    for m, rows in per.items():
        rng = np.random.default_rng([615, len(m)])
        wsum = sum(r[0] for r in rows)
        year_sigma = float(np.sqrt(sum(r[0] * np.mean(r[2] ** 2) for r in rows) / wsum))
        floor = 0.5 * year_sigma
        res = {"sigma_bps": year_sigma * 1e4, "fast_fills_per_day": sum(r[0] * len(r[3]) for r in rows) / 365}
        for name, sig in (("fixed", lambda r: year_sigma), ("trailing", lambda r: max(r[1], floor))):
            hz = np.concatenate([r[2] / sig(r) for r in rows])
            hp = np.concatenate([np.full(len(r[2]), r[0] / len(r[2])) for r in rows])
            az = np.concatenate([r[3] / sig(r) for r in rows])
            ap = np.concatenate([np.full(len(r[3]), r[0]) for r in rows])
            pooled = rng.choice(hz, size=200000, p=hp / hp.sum())
            edge_z = float((az * ap).sum() / ap.sum())
            kurt = float(((pooled - pooled.mean()) ** 4).mean() / pooled.var() ** 2)
            reach = {str(n): toxicity.honest_reach(pooled, n, rng, accounts=4000) for n in (50, 200, 1000)}
            res[name] = {"honest_sd_z": float(pooled.std()), "honest_kurtosis": kurt, "attack_edge_z": edge_z,
                         "fills_to_tier": {str(z): toxicity.fills_to_tier(edge_z, z) for z, _ in toxicity.Z_TIERS},
                         "honest_reach": reach,
                         "dilution": {str(z): toxicity.dilution_cost_share(edge_z, res["fast_fills_per_day"],
                                                                           tiers[m] * 1e4, year_sigma * 1e4, z)
                                      for z, _ in toxicity.Z_TIERS}}
        out["markets"][m] = res
    (RESULTS / "toxicity.json").write_text(json.dumps(out, indent=1))
    print(json.dumps(out["markets"], indent=1))


def stage_carrot(args):
    """The volume tiers: per crypto market, the dollar bounds each tier's shares give, the cost of
    farming each threshold, and one toxic life's counted volume; then, for the latency strategy at
    an honest node's lead (0.25 s) and at the operator's (3 s), the best a day for a trader who keeps
    its wallet and pays the stick against one who recycles wallets at tier 0, and what recycling
    gains and forfeits at each one's best size."""
    from calib import carrot, fee, toxicity

    schedule = {float(k): v for k, v in json.loads((RESULTS / "fee.json").read_text())["proposal"].items()}
    spreads = {r["symbol"]: r for r in json.loads((RESULTS / "spreads.json").read_text())}
    tox = json.loads((RESULTS / "toxicity.json").read_text())["markets"]
    margins = {m["symbol"]: float(m["margin_tiers"][0]["bps"]) / 1e4 for m in genesis_markets_cached()}
    add_on = toxicity.Z_TIERS[0][1]
    out = {"tiers": [t.__dict__ for t in carrot.TIERS], "days": carrot.DAYS, "min_hold_s": carrot.MIN_HOLD_S,
           "wallet_cost_usd": carrot.WALLET_COST_USD, "add_on_bps": add_on, "markets": {}}
    for m in ("BTC-USD", "ETH-USD", "SOL-USD"):
        sp = spreads[m]
        bound = float(sp["tiers"][0]["up_to"])
        s1, s2 = float(sp["tiers"][0]["bps"]), float(sp["tiers"][1]["bps"])
        market = {"bound": bound, "s1_bps": s1, "s2_bps": s2, "margin": margins[m]}
        sigma_1s = tox[m]["sigma_bps"] / toxicity.HORIZON_S ** 0.5
        farm = carrot.farming_cost_bps(s1, sigma_1s, float(fee.fee_at(schedule, carrot.MIN_HOLD_S)))
        res = {"market": market, "farming_cost_bps": farm,
               "tiers": [{"volume": t.volume, "best_price_usd": t.best_price_share * bound,
                          "max_leverage": t.leverage_share / margins[m], "farming_cost_usd": farm / 1e4 * t.volume,
                          "per_day_cap_usd": t.volume / max(carrot.DAYS[i], 1)} for i, t in enumerate(carrot.TIERS)],
               "toxic_life_volume": carrot.toxic_life_volume(bound, toxicity.MIN_FILLS), "strategies": {}}
        for lag in (0.25, 3.0):
            year = latency_market_year(lag, m)
            res["strategies"][f"{lag}s"] = carrot.best_responses(year, schedule, market, add_on, toxicity.MIN_FILLS, SIZE)
        out["markets"][m] = res
    (RESULTS / "carrot.json").write_text(json.dumps(out, indent=1, default=float))
    for m, r in out["markets"].items():
        print(m, "farm", round(r["farming_cost_bps"], 2), "life", r["toxic_life_volume"])
        for k, v in r["strategies"].items():
            print("  ", k, {kk: (round(vv, 1) if isinstance(vv, float) else vv) for kk, vv in v.items() if not isinstance(vv, dict)})


CRYPTO = ("BTC-USD", "ETH-USD", "SOL-USD")
OPERATOR_KEEPS = 0.8  # of its own accounts' take: the house it shares in pays the rest (#549)
FRONTIER_UNTIL_S = (600.0, 900.0, 1200.0, 1800.0, 2700.0, 3600.0)
FRONTIER_PEAK = ((1.0, 1.0), (1.0, 10.0), (1.0, 60.0), (1.0, 300.0), (0.5, 1.0), (0.75, 1.0))  # (peak fee, held to)


def latency_takes(schedule: dict, years: dict, markets: dict) -> dict:
    """The latency strategy's best a day under `schedule`, over the crypto markets, for an honest
    node's fastest client (0.25 s) and the operator's own accounts (3 s), each the better of keeping
    a wallet under the stick and recycling wallets at tier 0 (`carrot.best_responses`)."""
    from calib import carrot, toxicity

    out = {}
    for lag in (0.25, 3.0):
        total = 0.0
        for m in CRYPTO:
            r = carrot.best_responses(years[(lag, m)], schedule, markets[m], toxicity.Z_TIERS[0][1], toxicity.MIN_FILLS, SIZE)
            total += max(r["keeper"]["per_day"], r["recycler"]["per_day"])
        out[f"{lag}s"] = total
    return out


def crypto_markets() -> dict:
    spreads = {r["symbol"]: r for r in json.loads((RESULTS / "spreads.json").read_text())}
    margins = {m["symbol"]: float(m["margin_tiers"][0]["bps"]) / 1e4 for m in genesis_markets_cached()}
    return {m: {"bound": float(spreads[m]["tiers"][0]["up_to"]), "s1_bps": float(spreads[m]["tiers"][0]["bps"]),
                "s2_bps": float(spreads[m]["tiers"][1]["bps"]), "margin": margins[m]} for m in CRYPTO}


def stage_frontier(args):
    """A family of schedules, each at a peak fee up to a hold and falling linearly in the logarithm
    of the hold to 10% at `until`: per schedule, the extra fee on the target client's gross profit
    (holds of 15 minutes or more) and on the short holds', against the latency strategy's best a
    day at the calibrated spreads, the size tiers and the stick."""
    from calib import fee

    with (HERE.parent / "cap-calibration" / "data" / "derived" / "papertrade_holding.csv").open() as fh:
        holding = list(csv.DictReader(fh))
    markets = crypto_markets()
    years = {(lag, m): latency_market_year(lag, m) for lag in (0.25, 3.0) for m in CRYPTO}
    schedules = [("standard 10%", fee.family(fee.BASE, 1.0))]
    schedules += [(f"{peak:.0%} to {held:g} s, 10% at {until / 60:g} min", fee.family(peak, until, held))
                  for until in FRONTIER_UNTIL_S for peak, held in FRONTIER_PEAK if held < until]
    rows = []
    for name, schedule in schedules:
        target = fee.honest_cost(holding, schedule, from_s=900.0)
        short = fee.honest_cost(holding, schedule, to_s=900.0)
        takes = latency_takes(schedule, years, markets)
        rows.append({"schedule": name, "points": {str(k): v for k, v in schedule.items()},
                     "target_extra_share": target["extra_fee_share_of_profit"],
                     "short_fee_share": short["extra_fee_share_of_profit"] + fee.BASE,
                     "fast_take_per_day": takes["0.25s"], "operator_latency_per_day": takes["3.0s"] * OPERATOR_KEEPS})
        print(f"{name:40s} target +{rows[-1]['target_extra_share']:.1%} short {rows[-1]['short_fee_share']:.0%} "
              f"fast ${takes['0.25s']:,.0f} operator ${rows[-1]['operator_latency_per_day']:,.0f}", flush=True)
    (RESULTS / "frontier.json").write_text(json.dumps(rows, indent=1))


FX_TIER_TRIALS_BPS = (0.1, 0.25, 0.5, 1.0)
REFERENCE_TIERS = ((2e6, 0.1), (10e6, 1.0), (float("inf"), 3.0))  # the owner's EUR/USD reference
OPEN_HOURS = {"fx": 24 * 5 * 52, "index": 23 * 5 * 52}


def stage_fx_tiers(args):
    """What a tighter first tier would leave in FX and index markets. With no tick data for them,
    each market's moves are BTC-USD's scaled by the ratio `r` of their busy-hour 3 s moves: the
    latency strategy's results are homogeneous in the scale of the path, the spread and the
    threshold together, so a market's take at spread `s` is `r` times BTC-USD's at `s / r` (read off
    `scaled_btc.csv.gz` by linear interpolation between its spreads), over the market's open hours.
    Under the proposed fee, at the first tier's spread on the whole operator-sized position, without
    the stick or the size tiers: an upper bound on what the tier leaves."""
    import numpy as np

    from calib import fee
    from calib.common import use

    use("rogue")
    from model.paths import population

    strata, year_hours = population()["BTC-USD"]
    rows = read_gz(RESULTS / "scaled_btc.csv.gz")
    counts = {}
    for r in rows:
        if r["hold_s"] == "1" and r["theta_bps"] == "1" and r["lag_s"] == "3.0" and float(r["spread_bps"]) == SCALED_SPREADS_BPS[0]:
            counts[r["stratum"]] = counts.get(r["stratum"], 0) + 1
    schedule = proposal_schedule()
    year = {}
    for r in rows:
        w = strata[int(r["stratum"])] / counts[r["stratum"]] * 8760 / year_hours / max(float(r["covered"]), 1e-9)
        key = (float(r["spread_bps"]), float(r["lag_s"]), float(r["hold_s"]), float(r["theta_bps"]))
        pos, neg = year.get(key, (0.0, 0.0))
        year[key] = (pos + w * float(r["positive"]), neg + w * float(r["negative"]))
    best = {}  # (spread, lag) -> BTC's best take per unit of size per hour
    for (sp, lag, hold, theta), (pos, neg) in year.items():
        f = float(fee.fee_at(schedule, hold))
        v = ((1 - f) * pos - neg) / 8760
        best[(sp, lag)] = max(best.get((sp, lag), 0.0), v)
    spreads = {r["symbol"]: r for r in json.loads((RESULTS / "spreads.json").read_text())}
    btc_move = spreads["BTC-USD"]["move_3s_p90_bps"]
    out = {"trials_bps": FX_TIER_TRIALS_BPS, "btc_best_per_unit_hour": {f"{k[0]}@{k[1]}": v for k, v in best.items()}, "markets": {}}
    for m, r in spreads.items():
        if r["asset_class"] not in OPEN_HOURS:
            continue
        ratio = r["move_3s_p90_bps"] / btc_move
        res = {"ratio": ratio, "move_3s_p90_bps": r["move_3s_p90_bps"], "needed_bps": r["move_3s_p90_bps"] + r["adverse_bps"],
               "per_day": {}}
        def per_unit_hour(s_bps, lag):
            # Below the smallest measured spread the take is held at that spread's: an underestimate.
            xs = np.array(SCALED_SPREADS_BPS)
            ys = np.array([best[(x, lag)] for x in SCALED_SPREADS_BPS])
            return ratio * float(np.interp(s_bps / ratio, xs, ys, left=ys[0], right=0.0))

        days = OPEN_HOURS[r["asset_class"]] / 365
        for s_bps in FX_TIER_TRIALS_BPS:
            res["per_day"][str(s_bps)] = {f"{lag}s": per_unit_hour(s_bps, lag) * SIZE * days for lag in (0.25, 3.0)}
        # The owner's reference for EUR/USD-like markets, each fill paying its slices' tiers.
        res["per_day"]["reference"] = {}
        for lag in (0.25, 3.0):
            takes = []
            for size in (1e6, 2e6, SIZE, 10e6):
                lo, paid = 0.0, 0.0
                for up_to, bps in REFERENCE_TIERS:
                    part = max(0.0, min(size, up_to) - lo)
                    paid += part * bps
                    lo = up_to
                takes.append(per_unit_hour(paid / size, lag) * size * days)
            res["per_day"]["reference"][f"{lag}s"] = max(takes)
        out["markets"][m] = res
    (RESULTS / "fx_tiers.json").write_text(json.dumps(out, indent=1))
    for m, r in out["markets"].items():
        print(m, round(r["ratio"], 3), {k: {kk: round(vv) for kk, vv in v.items()} for k, v in r["per_day"].items()})


def _unprice_job(args):
    from calib import unprice

    h, s1 = args
    return unprice.hour_rows(h, s1)


def stage_attacks(args):
    """#549's attack classes under the proposed values, as year totals a day with #549's
    stratified estimates: the unpriced-spell measure re-run at the calibrated first tiers on every
    sampled hour (and the four stress hours, reported apart), with #549's genesis results beside
    it. The other classes' changes come from the frontier, the FX check and #549's own results."""
    from calib.common import use

    use("rogue")
    from model.paths import load_hours
    from model.report import PAIR_NOTIONAL, Year, unprice_tables

    tiers = {r["symbol"]: r["tiers"][0]["bps"] / 1e4 for r in json.loads((RESULTS / "spreads.json").read_text())}
    hours = load_hours()
    jobs = [(h, tiers[h.market]) for h in hours]
    rows = []
    with Pool(args.workers) as pool:
        for i, out in enumerate(pool.imap_unordered(_unprice_job, jobs, chunksize=1), 1):
            rows += out
            if i % 40 == 0:
                print(f"{i}/{len(jobs)} hours", flush=True)
    sampled = [r for r in rows if r["stratum"] != "stress"]
    table = unprice_tables(sampled, Year())
    stress_day = {}
    for r in rows:
        if r["stratum"] == "stress" and r["kind"] == "causal" and r["spell_s"] == 300 and r["trigger"] == 0.004:
            stress_day[str(r["margin"])] = stress_day.get(str(r["margin"]), 0.0) + r["payoff"] * PAIR_NOTIONAL
    out = {"pair_notional": PAIR_NOTIONAL, "unprice": table, "stress_hours_causal_300s_40bps": stress_day}
    (RESULTS / "attacks.json").write_text(json.dumps(out, indent=1, default=float))
    for r in table:
        if r["kind"] == "causal" and r["spell_s"] == 300 and r["trigger_bps_5s"] == 40:
            print(r["markets"], r["margin"], round(r["usd_year"] / 365), round(r["usd_year_lo"] / 365), round(r["usd_year_hi"] / 365))
    print("stress hours", stress_day)


PER_MARKET = ("BTC-USD", "ETH-USD")


def cap_inputs():
    """The cap study's markets, bars and flow at the genesis margins (100x on BTC-USD and ETH-USD)."""
    from calib.common import use

    use("cap")
    import catalogue
    from model import prices, simulate
    from model.expansion import require_profiles
    from model.flow import load, load_mix, with_genesis_profiles

    derived = HERE.parent / "cap-calibration" / "data" / "derived"
    bars, symbols = prices.load_bars(derived / "hourly_bars.npz")
    genesis = catalogue.load_genesis()
    flow = with_genesis_profiles(load(derived), [m for m in genesis["markets"] if m["symbol"] in symbols])
    with (derived / "benchmarks.csv").open() as fh:
        benchmarks = {r["benchmark"]: float(r["bps"]) for r in csv.DictReader(fh)}
    markets = simulate.load_markets(genesis, symbols, load_mix(derived, symbols), bars, benchmarks)
    require_profiles(markets, flow)
    return derived, bars, flow, markets


def stage_per_market(args):
    """#500's question for BTC-USD and ETH-USD at 100x: each market alone at its share of the flow
    over the cap study's floor grid (#111's per-market stage, 2,000 paths), at genesis pricing and at
    the calibrated spreads and proposed fee, beside the committed 10x results: where refusals end,
    the chance NAV ever falls below zero there, and bad debt (close-outs that gap past a balance)."""
    import numpy as np

    derived, bars, flow, markets = cap_inputs()
    from model import experiments, simulate

    tiers = {r["symbol"]: r["tiers"][0]["bps"] for r in json.loads((RESULTS / "spreads.json").read_text())}
    schedule = proposal_schedule()
    fee_schedule = tuple(sorted(schedule.items()))
    floors = experiments.MARKET_FLOORS[:, None]
    total = np.full(len(experiments.MARKET_FLOORS), experiments.NO_TOTAL_LIMIT)
    out = {"committed_10x": {}, "variants": {}}
    with (HERE.parent / "cap-calibration" / "results" / "per_market_sweep.csv").open() as fh:
        committed = [dict((k, float(v) if k != "symbol" else v) for k, v in r.items()) for r in csv.DictReader(fh)]
    for sym in PER_MARKET:
        mine = sorted((r for r in committed if r["symbol"] == sym), key=lambda r: r["floor"])
        bind = experiments.binding_point(mine)
        out["committed_10x"][sym] = next((r for r in mine if r["floor"] == bind), None)
    for name, changes in (("genesis pricing", {}), ("spreads and fee", {"fee_schedule": fee_schedule})):
        for sym in PER_MARKET:
            i = markets.symbols.index(sym)
            sub = markets.subset([i])
            extra = dict(changes)
            if name == "spreads and fee":
                extra["spread_bps"] = (tiers[sym],)
            scenario = simulate.Scenario(seed=1, paths=args.paths, batch_paths=500, **extra)
            (res,) = simulate.run_many([(scenario, sub, bars, flow, floors, total)], args.workers)
            rows = [{"symbol": sym, "floor": float(f), **experiments.lane_stats(res, lane)} for lane, f in enumerate(experiments.MARKET_FLOORS)]
            bind = experiments.binding_point(rows)
            at = next((r for r in rows if r["floor"] == bind), None)
            out["variants"].setdefault(name, {})[sym] = {"binding_floor": bind, "at_binding": at, "rows": rows}
            print(name, sym, bind, at and {k: at[k] for k in ("p_ever", "p_short", "bad_debt_mean", "bad_debt_p99", "fee_mean", "spread_income_mean")}, flush=True)
    (RESULTS / "per_market_100x.json").write_text(json.dumps(out, indent=1, default=float))


def stage_report(args):
    from calib import report

    report.write(RESULTS, PROPOSAL, SIZE)
    print((RESULTS / "report.md").read_text()[:400])


GAP_LAGS_S = (1.0, 3.0)
GAP_MARGINS = (0.01, 0.02, 0.05)


def stage_gaps(args):
    """Close-out gaps at the first band's margins, which the cap study's hourly bars cannot see: a
    position is closed out once its equity falls to half its margin, at the index the engine next
    prices, up to the price-lag bound later; past the other half of the margin the loss is the
    house's. Per BTC-USD and ETH-USD hour, at each second, the larger of the index's moves either
    way over the next `lag`; the seconds a day (year estimate) and on the four stress hours where
    that move passes half the margin, and its mean excess over it."""
    import numpy as np

    from calib.common import use

    use("rogue")
    from model.paths import load_hours, window_max, window_min
    from model.report import Year

    rows, stress = [], {}
    for h in load_hours():
        if h.market not in PER_MARKET:
            continue
        span = h.analysis
        t = np.arange(span.start, span.stop, h.steps(1.0))
        base = {"market": h.market, "hour": h.hour, "stratum": h.stratum}
        for lag in GAP_LAGS_S:
            k = h.steps(lag)
            hi, lo = window_max(h.x, k), window_min(h.x, k)
            t_ok = t[t < len(hi)]
            move = np.maximum(np.expm1(hi[t_ok] - h.x[t_ok]), -np.expm1(lo[t_ok] - h.x[t_ok]))
            for m in GAP_MARGINS:
                over = move > m / 2
                row = dict(base, lag_s=lag, margin=m, seconds=float(over.sum()), excess=float(np.clip(move - m / 2, 0, None).sum()))
                if h.stratum == "stress":
                    key = f"{lag}s@{m}"
                    stress[key] = {"seconds": stress.get(key, {}).get("seconds", 0.0) + row["seconds"],
                                   "excess_sum": stress.get(key, {}).get("excess_sum", 0.0) + row["excess"]}
                else:
                    rows.append(row)
    y = Year()
    out = {"year": [], "stress_hours": stress}
    for lag in GAP_LAGS_S:
        for m in GAP_MARGINS:
            sel = [r for r in rows if r["lag_s"] == lag and r["margin"] == m]
            sec = y.total(sel, lambda r, _: r["seconds"], PER_MARKET)
            exc = y.total(sel, lambda r, _: r["excess"], PER_MARKET)
            out["year"].append({"lag_s": lag, "margin": m, "seconds_per_day": sec.year / 365, "seconds_per_day_hi": sec.year_hi / 365,
                                "mean_excess": exc.year / sec.year if sec.year > 0 else 0.0})
    (RESULTS / "gaps.json").write_text(json.dumps(out, indent=1, default=float))
    for r in out["year"]:
        print(r)
    print(stress)


def genesis_markets_cached():
    from calib.common import genesis_markets

    return genesis_markets()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", choices=["spreads", "latency", "scaled", "fee", "frontier", "fx_tiers", "per_market", "gaps", "attacks", "toxicity", "carrot", "retained", "report"])
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--paths", type=int, default=1000)
    args = parser.parse_args()
    RESULTS.mkdir(exist_ok=True)
    globals()[f"stage_{args.stage}"](args)


if __name__ == "__main__":
    main()
