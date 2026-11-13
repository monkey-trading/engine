"""The house loss-rate trigger (#698): its measure, threshold and window, and the bound it gives.

Usage (from a virtualenv with requirements.txt, in this directory):
    python -I run.py honest --flow current|target       honest flow's loss over each window, per market and house-wide
    python -I run.py grid [--flow current]              honest flow, bad debt included, on the rogue study's recorded hours
    python -I run.py stress --flow current|target       2025-10-10's unpriced pairs under each candidate trigger
    python -I run.py latency                            the latency classes under each candidate, and what reaches the fastest client
    python -I run.py recommend                          the proposed trigger and its alternative at their thresholds
    python -I run.py share --paths 1000 [--workers 2]   the honest share on the corrected income model at the 10 s curve
    python -I run.py report                             results/report.md and results/criterion.json

Each stage reads only committed data and earlier stages' results, and is deterministic.
"""

import argparse
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
RESULTS = HERE / "results"
DATA = HERE / "data"  # gitignored: the per-bin series later stages read


def compact(obj) -> str:
    """A large result as one line of JSON: `results/report.md` is where it is read."""
    return json.dumps(obj, separators=(",", ":")) + "\n"


# The income runs at the 10 s curve, as #697's `owner_income` runs them at D3's: tier 3's fee (the
# least any tier pays past the curve) with the least fee churn's hold moments allow.
SHARE_RUNS = ("current", "target")
SHARE_FLOORS = slice(6, 9)  # the retained grid's floors either side of $107M, as #697


def stage_share(args):
    """#134's retained NAV and the operator's honest share (20% of distributions) on #697's corrected
    income model, with the hold curve held at the full fee to 10 s (#614, 2026-11-13). A run already
    in `share.json` is kept."""
    import csv

    from trigger import inputs

    symbols, bars, flow, mix, open_hours, specs = inputs.cap_flow(target=False)
    from calib import fee, target
    from model import accounting, expansion, retained, simulate

    derived = inputs.CAP / "data" / "derived"
    g = inputs.genesis()
    with (derived / "benchmarks.csv").open() as fh:
        benchmarks = {r["benchmark"]: float(r["bps"]) for r in csv.DictReader(fh)}
    markets = simulate.load_markets(g, symbols, mix, bars, benchmarks)
    n = len(symbols)
    classes = expansion.asset_classes(derived)
    with (derived / "market_mix.csv").open() as fh:
        listed = {r["symbol"] for r in csv.DictReader(fh) if r["listed"] == "True"}
    with (derived / "papertrade_holding.csv").open() as fh:
        holding = list(csv.DictReader(fh))
    first = {s["symbol"]: s["spread_tiers"][0]["tenth_bps"] / 10 for s in specs}
    flows = {"current": flow, "target": target.target_flow(flow, holding)}
    grid = retained.floor_grid(retained.GRID_LOW, retained.GRID_HIGH)[SHARE_FLOORS]
    path = RESULTS / "share.json"
    out = json.loads(path.read_text()) if path.exists() else {"curve": inputs.CURVE, "spread_bps": first, "runs": {}}
    points = fee.curve_points(*inputs.CURVE)
    for name in SHARE_RUNS:
        if name in out["runs"]:
            continue
        hold_fee = accounting.HoldFee(points=points, base_bps=1000.0, discount=fee.LAUNCH_DISCOUNTS[inputs.HONEST_TIER])
        scenario = simulate.Scenario(seed=1, paths=args.paths, batch_paths=retained.batch_paths(n), churn_holds="bound",
                                     spread_bps=tuple(first[s] for s in symbols), fee_schedule=hold_fee)
        rows = retained.run_variant(scenario, markets, bars, flows[name], classes, listed, n, "baseline", args.paths, grid,
                                    retained.workers_for(n, args.workers))
        answer = retained.retained_nav(rows)
        row = answer["row"]
        out["runs"][name] = {"paths": args.paths, "retained_nav": answer["retained_nav"], "row": row,
                             "operator_share_per_day": row["distributed_mean"] / 90 * 0.20 if row else None}
        path.write_text(json.dumps(out, indent=1, default=float))
        print(name, answer["retained_nav"], out["runs"][name]["operator_share_per_day"], flush=True)


# The reading's windows (the issue asks one to sixty minutes; six hours and a day show the trend), the
# measures, and the honest fire rates (share of open hours with a firing) each candidate threshold is
# set at.
WINDOWS_MIN = (1, 5, 15, 30, 60, 360, 1440)
MEASURES = ("realised", "marked", "bad_debt")
FIRE_RATES = (0.05, 0.02, 0.01, 0.005, 0.001)
RAISED_WINDOWS_MIN = (60, 360, 1440)  # the windows whose time raised later stages read
TAIL_HOURS = 96  # the path's last hours are left out: positions still open at its end never settle
CRYPTO = ("BTC-USD", "ETH-USD", "SOL-USD")
SAVED = ("BTC-USD", "ETH-USD")  # per-bin series kept for the attack stage, with the house-wide sum
REFERENCE_NAV = 107_374_182.40  # #134's retained NAV, where the caps are measured
SEED = 698


def honest_summary(series, w: int, hours_open, strata: dict, day_bins: int, raise_level: bool) -> dict:
    """One reading's honest distribution and each candidate threshold's fire rate, the room it leaves
    an attacker a day, and with `raise_level` the time it holds the level raised, overall and by each
    crypto market's volatility stratum (`strata`, market -> each hour's stratum)."""
    import numpy as np

    from trigger import windows
    from trigger.paths import BINS_PER_HOUR, BIN_S

    reading = windows.trailing(series, w)
    hm = windows.hourly_max(reading, BINS_PER_HOUR)[hours_open]
    bins_open = np.repeat(hours_open, BINS_PER_HOUR)
    at_open = reading[: len(bins_open)][bins_open]
    out = {"mean": float(at_open.mean()), "reading_q": {str(q): float(np.quantile(at_open, q)) for q in (0.5, 0.9, 0.99, 0.999)},
           "hourly_max_q": {str(q): float(np.quantile(hm, q)) for q in (0.5, 0.9, 0.95, 0.99, 0.995, 0.999)}, "thresholds": []}
    cooldown = int(round(windows.COOLDOWN_S / BIN_S))
    days = len(series) // day_bins
    for rate in FIRE_RATES:
        t = float(np.quantile(hm, 1 - rate))
        if t <= 0 and series.min() >= 0:
            t = 1.0  # a reading that cannot fall below zero fires on any loss
        row = {"rate": rate, "threshold": t, "fire_share": float((hm >= t).mean())}
        if raise_level:
            up = windows.raised(reading, t, cooldown)[: len(bins_open)]
            row["raised_share"] = float(up[bins_open].mean())
            row["raised_by_stratum"] = {}
            for name, hour_strata in strata.items():
                st = np.repeat(hour_strata, BINS_PER_HOUR)[: len(up)]
                row["raised_by_stratum"][name] = [float(up[st == k].mean()) for k in range(5)]
        if day_bins % w == 0:
            room = windows.disjoint_room(series[: days * day_bins], w, t).reshape(days, -1).sum(axis=1)
            row["daily_room_mean"] = float(room.mean())
            row["daily_room_p95"] = float(np.quantile(room, 0.95))
        out["thresholds"].append(row)
    return out


def stage_honest(args):
    """Honest flow on every genesis market over the cap study's 102 weeks of hourly bars, settled
    position by position on 10 s paths: per market and house-wide, each measure's loss over each
    window, with candidate thresholds at stated honest fire rates."""
    from datetime import datetime

    import numpy as np

    from trigger import honest, inputs, paths, windows
    from trigger.paths import BINS_PER_HOUR

    target = args.flow == "target"
    symbols, bars, flow, mix, open_hours, specs = inputs.cap_flow(target=target)
    daily = inputs.steady_margin(flow)
    flows = inputs.market_flows(symbols, flow, mix, open_hours, specs, daily)
    fee_fn = inputs.fee_fn()
    hours = bars.shape[0]
    keep = (hours - TAIL_HOURS) * BINS_PER_HOUR
    week0 = datetime.fromisoformat(str(np.load(inputs.CAP / "data" / "derived" / "hourly_bars.npz")["week0"]))
    house = {m: np.zeros(keep) for m in MEASURES}
    out = {"flow": args.flow, "daily_margin": daily, "windows_min": WINDOWS_MIN, "fire_rates": FIRE_RATES, "week0": week0.isoformat(),
           "hours": hours - TAIL_HOURS, "markets": {}, "house": {}}
    saved, all_strata = {}, {}
    day_bins = 24 * BINS_PER_HOUR
    for i, sym in enumerate(symbols):
        rng = np.random.default_rng([SEED, i])
        path = paths.from_bars(bars[:, i], rng)
        ret = np.diff(path.x[::BINS_PER_HOUR], prepend=path.x[0])
        dev = honest.deviation(ret, flow.long_share_ar1, flow.long_share_slope, flow.long_share_resid_sd, rng)
        closes = honest.simulate(path, flows[sym], dev, fee_fn, rng)
        series = {"realised": closes.net[:keep], "marked": closes.marked(path.x)[:keep], "bad_debt": closes.bad[:keep]}
        hours_open = path.is_open[::BINS_PER_HOUR][: hours - TAIL_HOURS]
        vol = (bars[: hours - TAIL_HOURS, i, 1].astype(float) - bars[: hours - TAIL_HOURS, i, 2]) / 1e6
        strata = {sym: windows.strata(vol, hours_open)} if sym in CRYPTO else {}
        exposure = np.abs(np.cumsum(closes.exposure)[:keep])
        cap = REFERENCE_NAV * specs[i]["house_cap_bps"] / 1e4
        res = {"income_per_day": float(-series["realised"].sum() / (keep / day_bins)),
               "exposure_q": {str(q): float(np.quantile(exposure[np.repeat(hours_open, BINS_PER_HOUR)], q)) for q in (0.5, 0.99)},
               "cap_at_reference_nav": cap, "time_over_cap": float((exposure > cap).mean()), "measures": {}}
        for m, s in series.items():
            house[m] += s
            res["measures"][m] = {str(wm): honest_summary(s, wm * 6, hours_open, strata, day_bins, wm in RAISED_WINDOWS_MIN and m != "bad_debt")
                                  for wm in WINDOWS_MIN}
        all_strata.update(strata)
        out["markets"][sym] = res
        if sym in SAVED:
            saved.update({f"{sym}/{m}": s.astype(np.float32) for m, s in series.items()})
        print(sym, round(res["income_per_day"]), {m: round(res["measures"][m]["60"]["thresholds"][2]["threshold"]) for m in MEASURES}, flush=True)
        del closes, series, path
    all_open = np.ones(hours - TAIL_HOURS, dtype=bool)
    for m, s in house.items():
        out["house"][m] = {str(wm): honest_summary(s, wm * 6, all_open, all_strata, day_bins, wm in RAISED_WINDOWS_MIN and m != "bad_debt")
                           for wm in WINDOWS_MIN}
        saved[f"house/{m}"] = s.astype(np.float32)
    out["house"]["income_per_day"] = float(-house["realised"].sum() / (keep / day_bins))
    (RESULTS / f"honest_{args.flow}.json").write_text(compact(out))
    np.savez_compressed(DATA / f"honest_{args.flow}.npz", **saved)
    print("house", round(out["house"]["income_per_day"]), {m: round(out["house"][m]["60"]["thresholds"][2]["threshold"]) for m in MEASURES})


GRID_WINDOWS_MIN = (1, 5, 15, 30, 60)
GRID_PRE_HOURS = 96  # the synthetic history before a sampled hour, longer than all but 0.07% of holds


def grid_paths():
    """The rogue study's sampled crypto hours, each on 10 s bins after `GRID_PRE_HOURS` of synthetic
    history at the market's median hourly volatility, so positions enter the hour as they would on an
    ordinary day: (market, hour, stratum, its `Path`, the bin its hour starts at, its year weight in
    hours; zero for the stress day's)."""
    import numpy as np

    from trigger import inputs, paths
    from trigger.paths import BIN_S, BINS_PER_HOUR

    weights = inputs.grid_weights()
    bars = np.load(inputs.CAP / "data" / "derived" / "hourly_bars.npz")
    symbols = [str(s) for s in bars["symbols"]]
    out = []
    for market, hour, stratum, x, step_s, lead in inputs.grid_hours():
        b = bars["bars"][:, symbols.index(market)]
        open_ = b[:, 0] != paths.CLOSED
        median_sd = float(np.median((b[open_, 1].astype(float) - b[open_, 2]) / 1e6)) / np.sqrt(4 * np.log(2))
        rng = np.random.default_rng([SEED, hour, CRYPTO.index(market)])
        step = int(round(BIN_S / step_s))
        x_hour = x[lead : lead + BINS_PER_HOUR * step : step]
        walk = rng.standard_normal(GRID_PRE_HOURS * BINS_PER_HOUR) * median_sd / np.sqrt(BINS_PER_HOUR)
        pre = x_hour[0] - np.cumsum(walk[::-1])[::-1]
        weight = 0.0 if stratum == "stress" else weights[(market, stratum)]
        out.append((market, hour, stratum, paths.from_array(np.concatenate([pre, x_hour])), GRID_PRE_HOURS * BINS_PER_HOUR, weight))
    return out


def stage_grid(args):
    """Honest flow on the rogue study's sampled crypto hours (#549), on their recorded trades: per
    hour, each measure's highest reading over each window, with the hour's year weight. Bad debt
    needs these: on bridged hourly bars no price jumps past a closed-out position's balance."""
    import numpy as np

    from trigger import honest, inputs, windows
    from trigger.paths import BINS_PER_HOUR

    symbols, bars, flow, mix, open_hours, specs = inputs.cap_flow(target=args.flow == "target")
    flows = inputs.market_flows(symbols, flow, mix, open_hours, specs, inputs.steady_margin(flow))
    fee_fn = inputs.fee_fn()
    rows = []
    for market, hour, stratum, path, start, weight in grid_paths():
        rng = np.random.default_rng([SEED, hour, CRYPTO.index(market), 1])
        ret = np.diff(path.x[::BINS_PER_HOUR], prepend=path.x[0])
        dev = honest.deviation(ret, flow.long_share_ar1, flow.long_share_slope, flow.long_share_resid_sd, rng)
        closes = honest.simulate(path, flows[market], dev, fee_fn, rng)
        series = {"realised": closes.net, "marked": closes.marked(path.x), "bad_debt": closes.bad}
        row = {"market": market, "hour": hour, "stratum": stratum, "weight": weight, "max": {}}
        for m, s in series.items():
            row["max"][m] = {str(wm): float(windows.trailing(s, wm * 6)[start : start + BINS_PER_HOUR].max()) for wm in GRID_WINDOWS_MIN}
        row["bad_debt_hour"] = float(series["bad_debt"][start : start + BINS_PER_HOUR].sum())
        rows.append(row)
    (RESULTS / f"grid_{args.flow}.json").write_text(compact({"flow": args.flow, "pre_hours": GRID_PRE_HOURS, "rows": rows}))
    for m in CRYPTO:
        mine = [r for r in rows if r["market"] == m and r["weight"] > 0]
        w = np.array([r["weight"] for r in mine])
        any_bad = np.array([r["bad_debt_hour"] > 0 for r in mine])
        print(m, "hours with honest bad debt", round(float((w * any_bad).sum() / w.sum()), 4),
              "stress hours' bad debt", [round(r["bad_debt_hour"]) for r in rows if r["market"] == m and r["stratum"] == "stress"])


STRESS_DAY = "2025-10-10"
STRESS_MARKETS = ("BTC-USD", "ETH-USD")  # SOL-USD's 10% margin leaves its pairs nothing (#549)
STRESS_WINDOWS_MIN = (5, 15, 60, 360, 1440)
BAD_DEBT_THRESHOLDS = (1e3, 1e4, 2.5e4, 5e4, 1e5, 2.5e5)


def stress_background(flow_name: str, day0, days: int) -> dict:
    """Honest flow's loss per bin over `days` from `day0`, per measure, house-wide and on each stress
    market: the honest stage's series on bridged bars, with the stress markets' bad debt on the stress
    day's recorded hours from honest flow on those hours' trades, as the grid stage settles it."""
    from datetime import datetime, timezone

    import numpy as np

    from trigger import honest, inputs
    from trigger.paths import BINS_PER_HOUR

    z = np.load(DATA / f"honest_{flow_name}.npz")
    week0 = datetime.fromisoformat(json.loads((RESULTS / f"honest_{flow_name}.json").read_text())["week0"])
    a = int((day0 - week0).total_seconds() // 3600) * BINS_PER_HOUR
    n = days * 24 * BINS_PER_HOUR
    out = {f"{scope}/{m}": z[f"{scope}/{m}"][a : a + n].astype(np.float64) for scope in ("house",) + STRESS_MARKETS for m in MEASURES}
    symbols, bars, flow, mix, open_hours, specs = inputs.cap_flow(target=flow_name == "target")
    flows = inputs.market_flows(symbols, flow, mix, open_hours, specs, inputs.steady_margin(flow))
    fee_fn = inputs.fee_fn()
    for market, hour, stratum, path, start, _ in grid_paths():
        if stratum != "stress" or market not in STRESS_MARKETS:
            continue
        rng = np.random.default_rng([SEED, hour, CRYPTO.index(market), 1])
        dev = honest.deviation(np.diff(path.x[::BINS_PER_HOUR], prepend=path.x[0]), flow.long_share_ar1, flow.long_share_slope,
                               flow.long_share_resid_sd, rng)
        bad = honest.simulate(path, flows[market], dev, fee_fn, rng).bad[start : start + BINS_PER_HOUR]
        b = int((datetime.fromtimestamp(hour, timezone.utc) - day0).total_seconds() // 10)
        out["house/bad_debt"][b : b + BINS_PER_HOUR] += bad - out[f"{market}/bad_debt"][b : b + BINS_PER_HOUR]
        out[f"{market}/bad_debt"][b : b + BINS_PER_HOUR] = bad
    return out


def stress_run(hours: list, background: dict, measure: str, scope: str, w: int, threshold: float | None, run_raised: bool,
               day_from: int, fresh: bool = True, pre_opened: bool = True) -> dict:
    """The stress day's unpriced pairs under one trigger, bin by bin: the reading over the trailing `w`
    bins of honest loss in `scope` ("house" or a market, whose `hours` the caller passes) and of the
    spells' loss, the level it drives, and each spell the causal rule starts. `threshold` None is no
    loss trigger. With `run_raised` the operator still runs spells while the level is raised, at its
    target for level 2's costs; otherwise it waits for the level to fall. With `fresh` a spell's move
    counts from no earlier than the market's last repricing (`pairs.run_spell`); without it, as #697.
    With `pre_opened` each spell's pair was opened ahead at level 0; without it, before the spell at
    the level then, as #697. The level is re-read after every loss, as the engine reads it on every
    command."""
    import numpy as np

    from trigger import pairs, windows
    from trigger.paths import BIN_S

    honest = background[f"{scope}/{measure}"]
    n = len(honest)
    attack = np.zeros(n)
    reading, up, since, raised_bins = 0.0, False, -(10**9), 0
    cooldown = int(round(windows.COOLDOWN_S / BIN_S))

    def level(b):
        nonlocal up, since
        if threshold is None:
            return
        if reading >= threshold and not up:
            up, since = True, b
        elif up and reading < threshold and b - since >= cooldown:
            up, since = False, b

    by_bin = {}
    for h in hours:
        for i in h["fired"]:
            by_bin.setdefault(h["bin0"] + int((i - h["lead"]) * h["step_s"] // BIN_S), []).append((h, int(i)))
    busy = {}  # per recorded hour: the grid step its last spell priced again at
    spells = []
    for b in range(n):
        reading += honest[b] + attack[b]
        if b >= w:
            reading -= honest[b - w] + attack[b - w]
        level(b)
        raised_bins += int(up and b >= day_from)
        for h, i in by_bin.get(b, ()):
            m, key = h["market"], (h["market"], h["bin0"])
            last = busy.get(key, -1)
            if i <= last or (up and not run_raised):
                continue
            forced = 2 if up else None
            open_level = 0 if pre_opened else max(int(h["levels"][min(i // h["per_s"], len(h["levels"]) - 1)]), forced or 0)
            sp = pairs.run_spell(h["x"], i, h["step_s"], h["s1"], open_level, forced, last if fresh and last >= 0 else None)
            busy[key] = sp.stop
            stop_bin = h["bin0"] + int((sp.stop - h["lead"]) * h["step_s"] // BIN_S)
            # The reading sees the house's loss as it is marked, its bad debt, or what it realises
            # when the operator closes the losing leg and keeps the winning one open.
            loss = {"marked": sp.payoff, "bad_debt": sp.bad_debt, "realised": -pairs.MARGIN}[measure] * pairs.NOTIONAL
            if stop_bin < n:
                attack[stop_bin] += loss
                if stop_bin == b:  # priced again within this bin: the reading has already summed it
                    reading += loss
                    level(b)
            spells.append({"market": m, "bin": b, "payoff": sp.payoff, "raised": forced is not None})
    day = [sp for sp in spells if sp["bin"] >= day_from]
    return {"house_loss": sum(sp["payoff"] for sp in day) * pairs.NOTIONAL, "spells": len(day),
            "payoffs": [sp["payoff"] * pairs.NOTIONAL for sp in day],
            "spells_raised": sum(sp["raised"] for sp in day), "largest_spell": max((sp["payoff"] for sp in day), default=0.0) * pairs.NOTIONAL,
            "by_market": {m: sum(sp["payoff"] for sp in day if sp["market"] == m) * pairs.NOTIONAL for m in STRESS_MARKETS},
            "raised_minutes": raised_bins * BIN_S / 60, "run_raised": run_raised}


OPERATOR_KEEPS = 0.8  # of its own accounts' take: the house it shares in pays the rest (#549)
STRESS_FIRE_RATES = (0.5, 0.25, 0.1, 0.05, 0.02, 0.01, 0.005, 0.001)


def honest_threshold(series, w: int, rate: float | None, threshold: float | None = None) -> tuple[float, float, float, float]:
    """The threshold at which honest flow's reading over `w` bins fires in `rate` of hours (or the
    given `threshold`), the shares of hours it fires in and of time it holds the level raised, and
    the most the threshold less honest flow's loss over one window ever leaves an attacker."""
    import numpy as np

    from trigger import windows
    from trigger.paths import BIN_S, BINS_PER_HOUR

    reading = windows.trailing(series, w)
    hm = windows.hourly_max(reading, BINS_PER_HOUR)
    t = float(np.quantile(hm, 1 - rate)) if threshold is None else threshold
    up = windows.raised(reading, t, int(round(windows.COOLDOWN_S / BIN_S)))
    return t, float((hm >= t).mean()), float(up.mean()), t - float(reading.min())


def stress_best(hours, background, measure, scope, w, threshold, day_from) -> dict:
    """The operator's better policy while the level is raised, for pairs opened ahead (`pre_opened`)
    and for one pair reopened before each spell, as #549's operator does (`per_spell`)."""
    out = {}
    for name, pre in (("pre_opened", True), ("per_spell", False)):
        runs = [stress_run(hours, background, measure, scope, w, threshold, run, day_from, pre_opened=pre) for run in (False, True)]
        out[name] = {k: v for k, v in max(runs, key=lambda r: r["house_loss"]).items() if k != "payoffs"}
    return out


def merge(split: list) -> dict:
    """Per-market readings' runs, one per stress market, added up for each attacker."""
    return {name: {"house_loss": sum(r[name]["house_loss"] for r in split), "spells": sum(r[name]["spells"] for r in split),
                   "largest_spell": max(r[name]["largest_spell"] for r in split)} for name in split[0]}


def stress_day(flow_name: str):
    """The stress day's recorded hours on BTC-USD and ETH-USD, ready for `stress_run`; honest flow's
    loss over that day and the day before; and the bin the stress day starts at."""
    from datetime import datetime, timedelta, timezone

    from trigger import inputs, pairs
    from trigger.paths import BIN_S

    day0 = datetime.fromisoformat(STRESS_DAY).replace(tzinfo=timezone.utc) - timedelta(days=1)
    first = {s["symbol"]: s["spread_tiers"][0]["tenth_bps"] / 1e5 for s in inputs.genesis()["markets"]}
    hours = []
    for market, hour, stratum, x, step_s, lead in inputs.grid_hours():
        if stratum != "stress" or market not in STRESS_MARKETS:
            continue
        hours.append({"market": market, "x": x, "step_s": step_s, "lead": lead, "per_s": int(round(1.0 / step_s)), "s1": first[market],
                      "levels": pairs.move_levels(x, step_s), "bin0": int((datetime.fromtimestamp(hour, timezone.utc) - day0).total_seconds() // BIN_S),
                      "fired": pairs.fired_steps(x, lead, lead + int(round(3600 / step_s)), step_s)})
    return hours, stress_background(flow_name, day0, 2), int(timedelta(days=1).total_seconds() // BIN_S)


def stage_stress(args):
    """2025-10-10's unpriced pairs at 1% margin (#614, D4) under each candidate trigger, on the
    recorded trades of the day's four most volatile hours, against honest flow's loss over the same day
    and the day before (a window reaches back up to a day). Marked and realised readings are tried at
    thresholds set by honest flow's fire rate over the whole history, bad debt at fixed thresholds."""
    import numpy as np

    from trigger import pairs

    hours, background, day_from = stress_day(args.flow)
    z = np.load(DATA / f"honest_{args.flow}.npz")
    out = {"flow": args.flow, "notional": pairs.NOTIONAL, "fee": pairs.FEE, "margin": pairs.MARGIN, "untriggered": {}, "candidates": []}
    # Without a loss trigger, each way of counting: pairs opened ahead or per spell, moves from the last
    # repricing or as #549's rule counts them, and #697's own spreads (the level's minimum alone).
    for pre in (True, False):
        for fresh in (True, False):
            key = f"{'pre-opened' if pre else 'per spell'}, {'from the last repricing' if fresh else 'as #549'}"
            out["untriggered"][key] = stress_run(hours, background, "marked", "house", 6, None, True, day_from, fresh=fresh, pre_opened=pre)
    levels = pairs.LEVELS
    pairs.LEVELS = tuple((lo, 1.0) for lo, _ in levels)
    try:
        out["untriggered"]["#697's spreads, per spell, as #549"] = stress_run(hours, background, "marked", "house", 6, None, True, day_from,
                                                                             fresh=False, pre_opened=False)
    finally:
        pairs.LEVELS = levels
    # The levers outside the trigger, on pairs opened ahead: 2% margin on BTC-USD and ETH-USD (#614,
    # D4's alternative), and unpriced spells cut short (#623's proposal 5).
    for key, margin, spell_s in (("2% margin", 0.02, pairs.SPELL_S), ("spells of at most 60 s", pairs.MARGIN, 60.0),
                                 ("spells of at most 30 s", pairs.MARGIN, 30.0)):
        saved = (pairs.MARGIN, pairs.SPELL_S)
        pairs.MARGIN, pairs.SPELL_S = margin, spell_s
        try:
            out["untriggered"][f"pre-opened, from the last repricing, {key}"] = stress_run(hours, background, "marked", "house", 6, None, True,
                                                                                         day_from)
        finally:
            pairs.MARGIN, pairs.SPELL_S = saved
    for k, v in out["untriggered"].items():
        print("no trigger,", k, round(v["house_loss"]), v["spells"], flush=True)

    def best(scope_hours, measure, scope, w, t):
        return stress_best(scope_hours, background, measure, scope, w, t, day_from)

    for measure in ("marked", "realised", "bad_debt"):
        for wm in STRESS_WINDOWS_MIN:
            w = wm * 6
            grid = [(None, t) for t in BAD_DEBT_THRESHOLDS] if measure == "bad_debt" else [(rate, None) for rate in STRESS_FIRE_RATES]
            for rate, t in grid:
                row = {"measure": measure, "window_min": wm, "fire_rate": rate}
                if measure == "bad_debt":
                    house = best(hours, measure, "house", w, t)
                    split = [best([h for h in hours if h["market"] == m], measure, m, w, t) for m in STRESS_MARKETS]
                    # Honest bad debt on the bridged bars: gaps at reopenings; the crypto markets' jumps
                    # inside an hour are the grid stage's.
                    _, fire, raised, _ = honest_threshold(z["house/bad_debt"].astype(np.float64), w, None, t)
                    row.update(threshold=t, honest_fire_share={"house": fire}, honest_raised_share={"house": raised}, house=house,
                               market=merge(split))
                else:
                    th = {s: honest_threshold(z[f"{s}/{measure}"].astype(np.float64), w, rate) for s in ("house",) + STRESS_MARKETS}
                    house = best(hours, measure, "house", w, th["house"][0])
                    split = [best([h for h in hours if h["market"] == m], measure, m, w, th[m][0]) for m in STRESS_MARKETS]
                    row.update(threshold={s: v[0] for s, v in th.items()}, honest_fire_share={s: v[1] for s, v in th.items()},
                               honest_raised_share={s: v[2] for s, v in th.items()}, most_room={s: v[3] for s, v in th.items()}, house=house,
                               market=merge(split))
                out["candidates"].append(row)
                print(measure, wm, rate, t, {k: round(v["house_loss"]) for k, v in row["house"].items()},
                      {k: round(v["house_loss"]) for k, v in row["market"].items()}, flush=True)
    (RESULTS / f"stress_{args.flow}.json").write_text(compact(out))


# The recommended trigger and its faster alternative: (name, window in minutes, threshold in bps of the
# total exposure cap). Zero needs no scale; 22.5 bps of the total cap at #134's retained NAV is target
# flow's 60-minute threshold at 1% of hours.
RECOMMENDED = (("a day, zero", 1440, 0.0), ("60 minutes, 22.5 bps of the total cap", 60, 22.5))


def stage_recommend(args):
    """The recommended trigger and its alternative at their exact thresholds: the house-wide marked loss
    over the window, on both flows, with honest flow's fire rate, time raised and its periods, the room
    it leaves an attacker a day and in its worst window, the latency classes it leaves (as `latency`
    measures them), and 2025-10-10's pairs under it."""
    import numpy as np

    from trigger import inputs, windows
    from trigger.paths import BIN_S, BINS_PER_HOUR, CLOSED

    total_cap = REFERENCE_NAV * inputs.genesis()["params"]["house_total_cap_bps"] / 1e4
    latency = json.loads((RESULTS / "latency.json").read_text())
    bars = np.load(inputs.CAP / "data" / "derived" / "hourly_bars.npz")
    symbols = [str(x) for x in bars["symbols"]]
    out = {"total_cap_at_reference_nav": total_cap, "flows": {}}
    for flow_name in ("current", "target"):
        series = np.load(DATA / f"honest_{flow_name}.npz")["house/marked"].astype(np.float64)
        hours, background, day_from = stress_day(flow_name)
        res = {}
        for name, wm, bps in RECOMMENDED:
            w, t = wm * 6, total_cap * bps / 1e4
            reading = windows.trailing(series, w)
            up = windows.raised(reading, t, int(round(windows.COOLDOWN_S / BIN_S)))
            starts = np.flatnonzero(np.diff(up.astype(np.int8), prepend=0) == 1)
            hm = windows.hourly_max(reading, BINS_PER_HOUR)
            days = len(series) // (24 * BINS_PER_HOUR)
            room = windows.disjoint_room(series[: days * 24 * BINS_PER_HOUR], w, t).reshape(days, -1).sum(axis=1)
            # Each latency class sits out the raised time: crypto by volatility stratum, FX and indices overall.
            n_hours = len(up) // BINS_PER_HOUR
            takes = {}
            for lag, who in ((3.0, "operator"), (0.25, "fastest")):
                crypto = 0.0
                for m in CRYPTO:
                    b = bars["bars"][:n_hours, symbols.index(m)]
                    st = np.repeat(windows.strata((b[:, 1].astype(float) - b[:, 2]) / 1e6, b[:, 0] != CLOSED), BINS_PER_HOUR)
                    raised_s = [float(up[: len(st)][st == k].mean()) for k in range(5)]
                    kept = sum(sh * (1 - r) for sh, r in zip(latency["stratum_shares"][f"{lag}/{m}"], raised_s))
                    crypto += latency["crypto"][f"{lag}/{m}"]["per_day"] * kept
                fx = sum(r[f"{lag}s"] for r in latency["fx_house_loss"].values()) * (1 - float(up.mean()))
                keep = OPERATOR_KEEPS if who == "operator" else 1.0
                takes[who] = {"crypto": crypto * keep, "fx_indices": fx * keep}
            res[name] = {"window_min": wm, "threshold_bps_of_total_cap": bps, "threshold": t, "latency": takes,
                         "fire_share": float((hm >= t).mean()), "raised_share": float(up.mean()), "episodes_a_year": len(starts) / days * 365,
                         "episode_mean_hours": float(up.sum()) / max(len(starts), 1) * BIN_S / 3600,
                         "daily_room_mean": float(room.mean()), "daily_room_p95": float(np.quantile(room, 0.95)), "daily_room_max": float(room.max()),
                         "window_room_max": t - float(reading.min()),
                         "stress": stress_best(hours, background, "marked", "house", w, t, day_from)}
            print(flow_name, name, {k: round(v, 4) for k, v in res[name].items() if isinstance(v, float)},
                  {k: round(v["house_loss"]) for k, v in res[name]["stress"].items()}, flush=True)
        out["flows"][flow_name] = res
    (RESULTS / "recommended.json").write_text(json.dumps(out, indent=1))


def spread_study():
    """`research/spread-calibration/run.py` as a module of its own name, for its latency helpers."""
    import importlib.util

    from trigger import inputs

    spec = importlib.util.spec_from_file_location("spread_run", inputs.SPREAD / "run.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


LATENCY_WINDOWS_MIN = (60, 360, 1440)
EXTRA_SPREADS_BPS = tuple(round(0.5 * k, 1) for k in range(41))  # 0 to 20 bps in halves
FLAT_FEES = tuple(round(0.05 * k, 2) for k in range(2, 21))  # 10% to 100%


def stage_latency(args):
    """Latency arbitrage (the operator's 3 s view) and the fastest honest client (an honest node's
    0.25 s) at the 10 s curve, as #697 measures them, and what a trigger leaves them: each sits out
    the time the level is raised, so a market's take falls by the share of it earned in raised time,
    from honest flow's raised time in each volatility stratum. Also what would bring the fastest client
    to zero without the trigger: a wider spread on every fill, or a higher fee at its holds."""
    import numpy as np

    sr = spread_study()
    from calib import fee

    from trigger import inputs

    points = fee.curve_points(*inputs.CURVE)
    tier_fee = lambda hold, tier: fee.d7(hold, fee.BASE, fee.LAUNCH_DISCOUNTS[tier], points)  # noqa: E731
    markets = sr.crypto_markets()
    years = {(lag, m): sr.latency_market_year(lag, m) for lag in (0.25, 3.0) for m in CRYPTO}
    takes = sr.owner_latency_takes(tier_fee, years, markets)
    # Each strategy's year sums by stratum, weighted as `latency_market_year` weighs the hours.
    from model.paths import population

    pop = population()
    rows = sr.read_gz(sr.RESULTS / "latency_hours.csv.gz")
    counts = {}
    for r in rows:
        if r["index"] == "held" and r["stress"] == "genesis" and r["lag_s"] == "3.0" and r["hold_s"] == "1" and r["theta_bps"] == "1":
            counts[(r["market"], r["stratum"])] = counts.get((r["market"], r["stratum"]), 0) + 1
    strategies = {(lag, m): takes[f"{lag}s"]["markets"][m] for lag in (0.25, 3.0) for m in CRYPTO}
    by_stratum = {k: np.zeros(5) for k in strategies}
    for r in rows:
        if r["index"] != "held" or r["stress"] != "genesis":
            continue
        key = (float(r["lag_s"]), r["market"])
        st = strategies.get(key)
        if not st or st["per_day"] <= 0 or float(r["hold_s"]) != st["hold_s"] or float(r["theta_bps"]) != st["theta_bps"]:
            continue
        strata, year_hours = pop[r["market"]]
        w = strata[int(r["stratum"])] / counts[(r["market"], r["stratum"])] * 8760 / year_hours / max(float(r["covered"]), 1e-9)
        f = float(tier_fee(st["hold_s"], st["tier"]))
        by_stratum[key][int(r["stratum"])] += w * ((1 - f) * float(r["positive"]) - float(r["negative"]))
    shares = {f"{lag}/{m}": (v / v.sum()).tolist() if v.sum() > 0 else [0.0] * 5 for (lag, m), v in by_stratum.items()}
    fx = json.loads((sr.RESULTS / "owner_fx.json").read_text())["markets"]
    curve = "100% to 10 s"
    out = {"curve": inputs.CURVE, "crypto": {f"{k[0]}/{k[1]}": v for k, v in strategies.items()}, "stratum_shares": shares,
           "fx_house_loss": {m: r["curves"][curve] for m, r in fx.items()}, "triggers": []}
    for flow_name in ("current", "target"):
        honest = json.loads((RESULTS / f"honest_{flow_name}.json").read_text())
        for measure in ("marked", "realised"):
            for wm in LATENCY_WINDOWS_MIN:
                for k, rate in enumerate(FIRE_RATES):
                    for scope in ("house", "market"):
                        res = {"flow": flow_name, "measure": measure, "window_min": wm, "fire_rate": rate, "scope": scope}
                        for lag, who in ((3.0, "operator"), (0.25, "fastest")):
                            crypto, fxs = 0.0, 0.0
                            for m in CRYPTO:
                                src = honest["house"][measure] if scope == "house" else honest["markets"][m]["measures"][measure]
                                r_s = src[str(wm)]["thresholds"][k]["raised_by_stratum"][m]
                                kept = sum(s * (1 - r) for s, r in zip(shares[f"{lag}/{m}"], r_s))
                                crypto += strategies[(lag, m)]["per_day"] * kept
                            for m, r in fx.items():
                                src = honest["house"][measure] if scope == "house" else honest["markets"][m]["measures"][measure]
                                fxs += r["curves"][curve][f"{lag}s"] * (1 - src[str(wm)]["thresholds"][k]["raised_share"])
                            keep = OPERATOR_KEEPS if who == "operator" else 1.0
                            res[who] = {"crypto": crypto * keep, "fx_indices": fxs * keep}
                        out["triggers"].append(res)
    # What reaches the fastest client without a trigger: an extra cost on each of its fills (the
    # linear bound `carrot.best_responses` uses), or a floor under the fee at every hold.
    fastest = {}
    for m in CRYPTO:
        y = years[(0.25, m)]
        size = sr.SIZE

        def best(extra, floor):
            vals = [((1 - max(float(tier_fee(h, 0)), floor)) * pos - neg - (1 - max(float(tier_fee(h, 0)), floor)) * n * 2 * extra) * size / 365
                    for (h, _), (pos, neg, n) in y.items()]
            return max(max(vals), 0.0)

        fastest[m] = {"spread": {str(c): best(c / 1e4, 0.0) for c in EXTRA_SPREADS_BPS},
                      "fee_floor": {str(f): best(0.0, f) for f in FLAT_FEES}}
    out["fastest_without_trigger"] = fastest
    # What a fee floor costs the target client (holds of 15 minutes or more): the extra fee as a share
    # of its gross profit on the reference venue's holding profile (`calib.fee.honest_cost`).
    import csv

    with (inputs.CAP / "data" / "derived" / "papertrade_holding.csv").open() as fh:
        holding = list(csv.DictReader(fh))
    out["fee_floor_target_cost"] = {
        str(f): fee.honest_cost(holding, None, from_s=900.0, fee_fn=lambda h, f=f: np.maximum(tier_fee(h, 0), f))["extra_fee_share_of_profit"]
        for f in FLAT_FEES}
    (RESULTS / "latency.json").write_text(json.dumps(out, indent=1, default=float))
    print({k: round(v["per_day"]) for k, v in out["crypto"].items()})
    for row in out["triggers"]:
        if row["flow"] == "target" and row["fire_rate"] == 0.01:
            print(row["measure"], row["scope"], row["window_min"], {w: {k: round(v) for k, v in row[w].items()} for w in ("operator", "fastest")})


def stage_report(args):
    from trigger import report

    print(report.write(RESULTS)[:1500])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("stage")
    ap.add_argument("--paths", type=int, default=1000)
    ap.add_argument("--workers", type=int, default=3)
    ap.add_argument("--flow", default="current")
    args = ap.parse_args()
    RESULTS.mkdir(exist_ok=True)
    DATA.mkdir(exist_ok=True)
    globals()[f"stage_{args.stage}"](args)


if __name__ == "__main__":
    main()
