"""The year's estimates from the hourly measurements, the operator's economics, and the report.

Reads `results/hourly.csv.gz` and writes the tables `results/*.csv`, the machine-readable
`results/summary.json`, and `results/report.md`. Every dollar figure is a stratified estimate over
the sampled hours (`model/stats.py`) with its 95% bootstrap interval; the stress day's hours are
reported on their own and never enter a year's estimate.
"""

import csv
import gzip
import json
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

import numpy as np

from model import economics, stats
from model.flow import market_flows
from model.paths import population

MARKETS = ("BTC-USD", "ETH-USD", "SOL-USD")
# The operator's position: 69% of each market's cap, under the utilisation trigger's level 1 (70%).
# At launch the cap is its floor, the $10M a market #134 records as #111's outcome; at #134's
# retained NAV ($107M) it is the genesis share of NAV where that is larger.
UTILISATION_L1 = 0.69
FLOORS = {"BTC-USD": 10e6, "ETH-USD": 10e6, "SOL-USD": 10e6}
CAP_SHARE = {"BTC-USD": 0.30, "ETH-USD": 0.20, "SOL-USD": 0.10}
POSITIONS = {
    "launch": {m: UTILISATION_L1 * FLOORS[m] for m in MARKETS},
    "retained NAV": {m: UTILISATION_L1 * max(FLOORS[m], CAP_SHARE[m] * economics.RETAINED_FLOOR) for m in MARKETS},
}
PAIR_NOTIONAL = 50e6  # a side, the operator's paired positions: $1M of margin at 1%
PRODUCTION = {"lag_s": 3.0, "hold_s": 5.0, "extra_bps": 0.0, "decide_s": 0.25}

NUMERIC = {"lag_s", "extra_bps", "decide_s", "pnl", "gross", "spread", "fee", "positions", "units_traded", "win_share",
           "scale", "delay_s", "fills", "notional", "usd", "mean_bps", "sd_bps", "hold_s", "uses", "held", "without",
           "kill_share", "now", "spell_s", "payoff", "spells", "paying", "unpriced_s", "theta_bps", "ahead_s", "events",
           "sum_bps", "sum_sq_bps", "hour", "margin", "level1_share", "level2_share", "p99_bps"}


def load(path: Path) -> list[dict]:
    rows = []
    with gzip.open(path, "rt") as fh:
        for r in csv.DictReader(fh):
            out = {}
            for k, v in r.items():
                if v == "":
                    out[k] = None
                elif k in NUMERIC:
                    out[k] = float(v)
                elif v in ("True", "False"):
                    out[k] = v == "True"
                elif k == "stratum":
                    out[k] = v if v == "stress" else int(v)
                elif k == "trigger":
                    out[k] = float(v)
                else:
                    out[k] = v
            rows.append(out)
    return rows


def select(rows, **match):
    return [r for r in rows if all(r.get(k) == v for k, v in match.items())]


def by_stratum(rows, market, value):
    out = defaultdict(list)
    for r in rows:
        if r["market"] == market and r["stratum"] != "stress":
            out[r["stratum"]].append(value(r))
    return {k: np.array(v) for k, v in out.items()}


class Year:
    """Year estimates over the markets' strata."""

    def __init__(self):
        self.pop = population()

    def total(self, rows, value, markets=MARKETS, seed=0) -> stats.Estimate:
        parts = []
        for m in markets:
            vals = by_stratum(rows, m, lambda r: value(r, m))
            if vals:
                pop, year_hours = self.pop[m]
                parts.append((vals, pop, year_hours))
        return stats.combine(parts, seed)

    def ratio(self, rows, num, den, markets=MARKETS, seed=0) -> stats.Estimate:
        parts = []
        for m in markets:
            n = by_stratum(rows, m, lambda r: num(r, m))
            d = by_stratum(rows, m, lambda r: den(r, m))
            if n:
                pop, year_hours = self.pop[m]
                parts.append((n, d, pop, year_hours))
        return stats.ratio(parts, seed)


def money(x: float) -> str:
    a = abs(x)
    if a >= 1e9:
        s = f"${a / 1e9:.2f}B"
    elif a >= 1e6:
        s = f"${a / 1e6:.2f}M"
    elif a >= 1e3:
        s = f"${a / 1e3:.0f}k"
    else:
        s = f"${a:.0f}"
    return ("-" if x < 0 else "") + s


def day(e: stats.Estimate) -> str:
    return f"{money(e.day)} ({money(e.day_lo)} to {money(e.day_hi)})"


def year(e: stats.Estimate) -> str:
    return f"{money(e.year)} ({money(e.year_lo)} to {money(e.year_hi)})"


def interval(e: stats.Estimate, fmt="{:.2f}") -> str:
    return f"{fmt.format(e.year)} ({fmt.format(e.year_lo)} to {fmt.format(e.year_hi)})"


def est_dict(e: stats.Estimate) -> dict:
    return {"year": e.year, "year_lo": e.year_lo, "year_hi": e.year_hi, "day": e.day, "day_lo": e.day_lo, "day_hi": e.day_hi}


def write_csv(path: Path, rows: list[dict]):
    if not rows:
        path.write_text("")
        return
    with path.open("w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=list(rows[0]))
        w.writeheader()
        w.writerows(rows)


def latency_tables(rows, y: Year):
    lat = select(rows, attack="latency", decide_s=0.25, stress=True)
    table = []
    for launder in (True, False):
        for lag in sorted({r["lag_s"] for r in lat}):
            for extra in sorted({r["extra_bps"] for r in lat}):
                sel = select(lat, lag_s=lag, extra_bps=extra, launder=launder)
                per_unit = y.total(sel, lambda r, m: r["pnl"])
                usd = {name: y.total(sel, lambda r, m, p=pos: r["pnl"] * p[m]) for name, pos in POSITIONS.items()}
                table.append({"launder": launder, "lag_s": lag, "extra_bps": extra,
                              "bps_per_day_of_position": per_unit.day * 1e4,
                              **{f"usd_day_{k}": v.day for k, v in usd.items()},
                              **{f"usd_day_{k}_lo": v.day_lo for k, v in usd.items()},
                              **{f"usd_day_{k}_hi": v.day_hi for k, v in usd.items()},
                              "positions_per_day": y.total(sel, lambda r, m: r["positions"]).day})
    sens = []
    for decide in (0.1, 0.25, 1.0):
        sel = select(rows, attack="latency", decide_s=decide, stress=True, lag_s=3.0, extra_bps=0.0, launder=True)
        sens.append({"variant": f"decide every {decide:g} s", **est_dict(y.total(sel, lambda r, m: r["pnl"] * POSITIONS["launch"][m]))})
    sel = select(rows, attack="latency", decide_s=0.25, stress=False, lag_s=3.0, extra_bps=0.0, launder=True)
    sens.append({"variant": "no stress spreads", **est_dict(y.total(sel, lambda r, m: r["pnl"] * POSITIONS["launch"][m]))})
    return table, sens


def skim_tables(rows, y: Year, flows):
    sk = select(rows, attack="skim")
    table = []
    for kind in ("bound", "causal", "sequenced"):
        for lag in sorted({r["lag_s"] for r in sk}):
            for delay in sorted({r["delay_s"] for r in sk}):
                for scale in (1.0, 10.0):
                    sel = select(sk, kind=kind, lag_s=lag, delay_s=delay, scale=scale)
                    if not sel and kind != "sequenced" and scale == 10.0:
                        # Per fill the same at any flow: ten times the notional and the dollars.
                        sel = [dict(r, usd=r["usd"] * 10, notional=r["notional"] * 10) for r in select(sk, kind=kind, lag_s=lag, delay_s=delay, scale=1.0)]
                    if not sel:
                        continue
                    usd = y.total(sel, lambda r, m: r["usd"])
                    bps = y.ratio(sel, lambda r, m: r["usd"], lambda r, m: r["notional"])
                    detect = np.mean([(3.09 * r["sd_bps"] / r["mean_bps"]) ** 2 for r in sel if r["mean_bps"] > 0])
                    table.append({"kind": kind, "lag_s": lag, "delay_s": delay, "flow": scale,
                                  "bps_of_notional": bps.year * 1e4, "bps_lo": bps.year_lo * 1e4, "bps_hi": bps.year_hi * 1e4,
                                  "usd_day": usd.day, "usd_day_lo": usd.day_lo, "usd_day_hi": usd.day_hi,
                                  "fills_to_detect": float(detect)})
    return table


def hold_tables(rows, y: Year):
    hd = select(rows, attack="hold")
    table = []
    for lag in sorted({r["lag_s"] for r in hd}):
        for H in sorted({r["hold_s"] for r in hd}):
            sel = select(hd, lag_s=lag, hold_s=H)
            if not sel:
                continue
            # Means over the year's hours: totals over the year divided by its hours.
            held = y.total(sel, lambda r, m: r["held"])
            without = y.total(sel, lambda r, m: r["without"])
            kill = y.total(sel, lambda r, m: r["kill_share"])
            n = stats.HOURS_PER_YEAR * len(MARKETS)
            top = [r for r in sel if r["stratum"] == 4]
            row = {"lag_s": lag, "hold_s": H, "held_bps": held.year / n * 1e4, "held_bps_lo": held.year_lo / n * 1e4,
                   "held_bps_hi": held.year_hi / n * 1e4, "without_bps": without.year / n * 1e4,
                   "added_bps": (held.year - without.year) / n * 1e4,
                   "kill_share": kill.year / n,
                   "held_bps_top1pct": float(np.mean([r["held"] for r in top]) * 1e4),
                   "kill_share_top1pct": float(np.mean([r["kill_share"] for r in top]))}
            table.append(row)
    return table


def unprice_tables(rows, y: Year):
    up = select(rows, attack="unprice")
    table = []
    for kind in ("best", "causal"):
        for spell in sorted({r["spell_s"] for r in up}):
            triggers = [None] if kind == "best" else sorted({r["trigger"] for r in up if r["trigger"] is not None})
            for trig in triggers:
                for markets, margin in ((("BTC-USD", "ETH-USD"), 0.01), (("BTC-USD", "ETH-USD"), 0.02),
                                        (("BTC-USD", "ETH-USD"), 0.05), (("SOL-USD",), 0.10)):
                    sel = select(up, kind=kind, spell_s=spell, trigger=trig, margin=margin)
                    pay = y.total(sel, lambda r, m: r["payoff"] * PAIR_NOTIONAL, markets)
                    paying = y.total(sel, lambda r, m: r["paying"], markets)
                    secs = y.total(sel, lambda r, m: r["unpriced_s"], markets)
                    table.append({"kind": kind, "spell_s": spell, "trigger_bps_5s": None if trig is None else trig * 1e4,
                                  "markets": "+".join(markets), "margin": margin, "usd_year": pay.year, "usd_year_lo": pay.year_lo,
                                  "usd_year_hi": pay.year_hi, "paying_spells_year": paying.year, "unpriced_s_year": secs.year})
    return table


def drift_tables(rows, y: Year):
    dr = select(rows, attack="drift")
    table = []
    for theta in sorted({r["theta_bps"] for r in dr}):
        for ahead in sorted({r["ahead_s"] for r in dr}):
            sel = select(dr, theta_bps=theta, ahead_s=ahead)
            mean = y.ratio(sel, lambda r, m: r["sum_bps"], lambda r, m: r["events"])
            events = y.total(sel, lambda r, m: r["events"])
            table.append({"theta_bps": theta, "ahead_s": ahead, "continuation_bps": mean.year,
                          "lo": mean.year_lo, "hi": mean.year_hi, "seconds_per_year": events.year})
    return table


def stress_day(rows):
    out = []
    for r in rows:
        if r["stratum"] != "stress":
            continue
        key = None
        if r["attack"] == "latency" and r["decide_s"] == 0.25 and r["stress"] and r["lag_s"] == 3 and r["extra_bps"] in (0.0, 2.0):
            key = f"latency, extra {r['extra_bps']:g} bps, {'laundered' if r['launder'] else 'tier 2'}"
            value = r["pnl"] * POSITIONS["launch"][r["market"]]
        elif r["attack"] == "skim" and r["kind"] == "sequenced" and r["lag_s"] == 3 and r["delay_s"] == 0 and r["scale"] == 1.0:
            key, value = "skim, sequenced, launch flow", r["usd"]
        elif r["attack"] == "hold" and r["lag_s"] == 3 and r["hold_s"] == 5:
            key, value = "held sync, loss a kill avoids, per use per $1M", (r["held"] - r["without"]) * 1e6
        elif r["attack"] == "unprice" and r["spell_s"] == 60 and r["margin"] in (0.01, 0.1) and (r["kind"] == "best" or r["trigger"] == 0.002):
            key, value = f"unpriced pairs, 60 s, {r['kind']}", r["payoff"] * PAIR_NOTIONAL
        if key:
            hour = datetime.fromtimestamp(int(r["hour"]), timezone.utc).strftime("%H:00")
            out.append({"market": r["market"], "hour_utc": hour, "measure": key, "usd": value})
    return out


def held_per_day(rows, y: Year, market: str, kills: int, lag=3.0, hold_s=5.0) -> stats.Estimate:
    """What the held sync adds a day on one market at `kills` restarts a day, over closing the same
    positions at once: as many uses as make that many kills, at the year's mean added value per use
    and kill share."""
    sel = select(rows, attack="hold", lag_s=lag, hold_s=hold_s)
    held = y.total(sel, lambda r, m: (r["held"] - r["without"]) * POSITIONS["launch"][m], (market,))
    kill = y.total(sel, lambda r, m: r["kill_share"], (market,))
    uses_per_kill = 1 / max(kill.year / stats.HOURS_PER_YEAR, 1e-9)
    per_use = held.scaled(1 / stats.HOURS_PER_YEAR)
    # `per_use.year` is the mean dollars a use; a day of `kills` kills is that many uses per kill.
    per_day = per_use.scaled(kills * uses_per_kill)
    return stats.Estimate(per_day.year * 365, per_day.year_lo * 365, per_day.year_hi * 365)


def headline(rows, y: Year, calibration: dict | None):
    """Each attack at production values: the house's loss or gain a day, and the operator's take."""
    from model.flow import opening_notional_by_class

    out = []

    def add(attack, side, flow, est, note=""):
        keep = economics.own_keep(0.0) if side == "own" else economics.house_keep(0.0)
        out.append({"attack": attack, "who_pays": "house" if side == "own" else "clients", "flow": flow,
                    **est_dict(est), "operator_day": est.day * keep, "operator_day_lo": est.day_lo * keep,
                    "operator_day_hi": est.day_hi * keep, "note": note})

    lat = select(rows, attack="latency", decide_s=0.25, stress=True, lag_s=3.0, extra_bps=0.0)
    for launder, name in ((True, "latency arbitrage, score laundered"), (False, "latency arbitrage, top toxicity tier paid")):
        est = y.total(select(lat, launder=launder), lambda r, m: r["pnl"] * POSITIONS["launch"][m])
        for flow in ("launch", "10x"):
            add(name, "own", flow, est, "operator positions at 69% of the launch caps; independent of client flow")
    sk = select(rows, attack="skim", kind="sequenced", lag_s=3.0, delay_s=0.0)
    for scale, flow in ((1.0, "launch"), (10.0, "10x")):
        add("skimming client fills (3 s look-back, no delay)", "house", flow, y.total(select(sk, scale=scale), lambda r, m: r["usd"]))
    closes = select(rows, attack="skim", kind="causal", lag_s=3.0, delay_s=10.0, scale=1.0)
    est = y.total(closes, lambda r, m: 0.5 * r["usd"])
    for scale, flow in ((1.0, "launch"), (10.0, "10x")):
        add("censoring closes for 10 s (closes are half the fills)", "house", flow, est.scaled(scale))
    est = held_per_day(rows, y, "BTC-USD", 4)
    for flow in ("launch", "10x"):
        add("held sync, 4 restarts a day (BTC-USD), over closing at once", "own", flow, est)
    # The causal rule's best spell and trigger over the sample: chosen after the fact, so mildly
    # optimistic for the operator; a rule that loses is not run, so it counts as nothing.
    up = select(rows, attack="unprice", kind="causal", margin=0.01)
    candidates = []
    for spell in sorted({r["spell_s"] for r in up}):
        for trig in sorted({r["trigger"] for r in up}):
            sel = select(up, spell_s=spell, trigger=trig)
            candidates.append((y.total(sel, lambda r, m: r["payoff"] * PAIR_NOTIONAL, ("BTC-USD", "ETH-USD")), spell, trig))
    est, spell, trig = max(candidates, key=lambda c: c[0].year)
    if est.year <= 0:
        est = stats.Estimate(0.0, 0.0, 0.0)
    for flow in ("launch", "10x"):
        add(f"unpricing across moves, $50M pairs (spells to {spell:g} s, {trig * 1e4:g} bps trigger)", "own", flow, est,
            "the causal rule's best spell and trigger, BTC-USD and ETH-USD")
    if calibration:
        half = np.mean([v["half_gap_bps_mean"] for k, v in calibration["summary"].items() if k in ("BTC", "ETH")])
        for scale, flow in ((1.0, "launch"), (10.0, "10x")):
            notional = sum(f.fill_notional_per_day for f in market_flows(scale).values())
            per_year = half / 1e4 * notional * 365
            add("choosing the venue if min_sources = 1", "house", flow, stats.Estimate(per_year, per_year, per_year),
                "half the venues' gap on every crypto fill, live recording")
    for scale, flow in ((1.0, "launch"), (10.0, "10x")):
        rwa = opening_notional_by_class(scale)
        notional = 2 * (rwa["indices"] + rwa["commodities"] + rwa["shares"])
        per_year = 3e-4 * notional * 365
        add("altering Databento quotes, 3 bps a fill", "house", flow, stats.Estimate(per_year, per_year, per_year),
            "every index, commodity and share fill; below the level-1 divergence trigger")
    # The best combination at production values: its own latency arbitrage, the held sync's
    # addition and unpriced pairs, with skimming every client fill. Skimming moves the index only
    # forward within its window, so the arbitrage still sees every price on the path; censoring
    # adds nothing beyond the lag (the momentum table). Measured per hour together, so the
    # interval is the joint one; the held sync and the pairs add their own.
    keyed = defaultdict(dict)
    for r in select(rows, attack="latency", decide_s=0.25, stress=True, lag_s=3.0, extra_bps=0.0):
        keyed[(r["market"], r["hour"])][("lat", r["launder"])] = r["pnl"] * POSITIONS["launch"][r["market"]]
    for r in select(rows, attack="skim", kind="sequenced", lag_s=3.0, delay_s=0.0):
        keyed[(r["market"], r["hour"])][("skim", r["scale"])] = r["usd"]
    strata = {(r["market"], r["hour"]): r["stratum"] for r in rows}
    rows_combined = [{"market": m, "hour": hr, "stratum": strata[(m, hr)], **v} for (m, hr), v in keyed.items()]
    extras = [h for h in out if h["attack"].startswith(("held sync", "unpricing")) and h["flow"] == "launch"]
    for launder, name in ((True, "everything, score laundered"), (False, "everything, top toxicity tier paid")):
        for scale, flow in ((1.0, "launch"), (10.0, "10x")):
            keep_own, keep_house = economics.own_keep(0.0), economics.house_keep(0.0)
            est = y.total(rows_combined, lambda r, m: keep_own * r[("lat", launder)] + keep_house * r[("skim", scale)])
            take = stats.Estimate(est.year + keep_own * sum(h["year"] for h in extras),
                                  est.year_lo + keep_own * sum(h["year_lo"] for h in extras),
                                  est.year_hi + keep_own * sum(h["year_hi"] for h in extras))
            out.append({"attack": f"combined: {name}", "who_pays": "both", "flow": flow, **est_dict(take),
                        "operator_day": take.day, "operator_day_lo": take.day_lo, "operator_day_hi": take.day_hi,
                        "note": "the operator's take; the dollar columns are its take too"})
    return out


def economics_table(head):
    """The take per operating day at which extraction pays even if it is acted on the day it
    starts, under each removal route, at a seat worth one year of the operator's share."""
    dist = economics.honest_distributions_per_day()
    rows = []
    for scale, flow in ((1.0, "launch"), (10.0, "10x")):
        for removal in economics.removals():
            seat = economics.seat_value(dist * scale, removal.seat_share, 1.0)
            rows.append({"flow": flow, "removal": removal.name, "seat_share": removal.seat_share,
                         "operating_days": removal.operating_days,
                         "operator_income_per_day": economics.OPERATOR_SHARE * dist * scale * removal.seat_share,
                         "seat_value_one_year": seat,
                         "breakeven_take_per_operating_day": economics.breakeven_take_per_day(seat, removal)})
    return rows


SERIES = ("#2a78d6", "#eb6834", "#1baf7a", "#eda100")
INK, MUTED, GRID, SURFACE = "#0b0b0b", "#52514e", "#e4e3df", "#fcfcfb"


def plot(results: Path, tables: dict):
    """Two figures: latency arbitrage against an extra spread, and unpriced pairs against the
    spell length, each with the break-even take an operating day for comparison."""
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import matplotlib.ticker

    def frame(ax):
        ax.set_facecolor(SURFACE)
        for side in ("top", "right"):
            ax.spines[side].set_visible(False)
        for side in ("left", "bottom"):
            ax.spines[side].set_color(MUTED)
        ax.tick_params(colors=MUTED, labelsize=9)
        ax.grid(True, axis="y", color=GRID, linewidth=0.8)
        ax.set_axisbelow(True)

    econ = [r for r in tables["economics"] if r["flow"] == "launch"]
    lines = [(next(r for r in econ if r["removal"] == name)["breakeven_take_per_operating_day"], label)
             for name, label in (("current rules, past the minimum term", "break-even, current rules"),
                                 ("current rules, just elected", "break-even, just elected"),
                                 ("daily rotation, one day, 5% of the stake", "break-even, rotation at 5% of the stake"))]

    fig, ax = plt.subplots(figsize=(7.2, 4.2), facecolor=SURFACE)
    frame(ax)
    floor = 1e4  # below $10k a day the take is noise around zero
    for colour, lag in zip(SERIES, (0.25, 1.0, 3.0, 15.0)):
        pts = sorted((r["extra_bps"], economics.own_keep(0.0) * r["usd_day_launch"]) for r in tables["latency"]
                     if r["launder"] and r["lag_s"] == lag)
        xs, ys = zip(*[(x, y) for x, y in pts if y >= floor])
        ax.plot(xs, ys, color=colour, linewidth=2, marker="o", markersize=5, label=f"{lag:g} s lag")
        ax.annotate(f"{lag:g} s", (xs[-1], ys[-1]), xytext=(6, 0), textcoords="offset points", va="center", fontsize=9, color=INK)
    for (value, label), (x, dy, ha) in zip(lines, ((10, 4, "right"), (10, 4, "right"), (0, -11, "left"))):
        ax.axhline(value, color=MUTED, linewidth=1, linestyle=(0, (4, 3)))
        ax.annotate(label, (x, value), xytext=(0, dy), textcoords="offset points", fontsize=8, color=MUTED, ha=ha)
    ax.set_yscale("log")
    ax.set_ylim(floor, 2e8)
    ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: money(v)))
    ax.set_xlabel("Extra spread every fill pays (bps)", color=INK, fontsize=10)
    ax.set_ylabel("Operator's take, $ a day", color=INK, fontsize=10)
    ax.set_title("Latency arbitrage at launch positions, toxicity score laundered", color=INK, fontsize=11, loc="left")
    ax.legend(frameon=False, fontsize=9, loc="lower left")
    fig.tight_layout()
    fig.savefig(results / "latency_vs_spread.png", dpi=150)
    plt.close(fig)

    fig, ax = plt.subplots(figsize=(7.2, 4.2), facecolor=SURFACE)
    frame(ax)
    for colour, margin in zip(SERIES, (0.01, 0.02, 0.05)):
        pts = sorted((r["spell_s"], r["usd_year"] / 1e6) for r in tables["unprice"]
                     if r["kind"] == "best" and r["markets"] == "BTC-USD+ETH-USD" and r["margin"] == margin)
        xs, ys = zip(*pts)
        ax.plot(xs, ys, color=colour, linewidth=2, marker="o", markersize=5, label=f"{margin:.0%} margin")
        ax.annotate(f"{margin:.0%}", (xs[-1], ys[-1]), xytext=(6, 0), textcoords="offset points", va="center", fontsize=9, color=INK)
    ax.set_xscale("log")
    ax.xaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{v:g}"))
    ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"\\${v:g}M"))
    ax.set_xlabel("Longest unpriced spell (s)", color=INK, fontsize=10)
    ax.set_ylabel("A year from \\$50M of pairs", color=INK, fontsize=10)
    ax.set_title("Unpricing BTC-USD and ETH-USD across moves, best spell an hour", color=INK, fontsize=11, loc="left")
    ax.legend(frameon=False, fontsize=9, loc="upper left")
    fig.tight_layout()
    fig.savefig(results / "unprice_vs_spell.png", dpi=150)
    plt.close(fig)


def md_table(header: list[str], body: list[list[str]]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    lines += ["| " + " | ".join(r) + " |" for r in body]
    return "\n".join(lines)


def write_report(results: Path, tables: dict, summary: dict):
    t = tables
    md = ["# Rogue operator: results", "",
          "Generated by `run.py report` from `results/hourly.csv.gz`. Every year figure is a stratified "
          "estimate over the sampled hours with its 95% bootstrap interval in brackets; a day is a year "
          "over 365. The README explains the method and the limits.", ""]
    md += ["## Headline, production values", "",
           md_table(["Attack", "Pays", "Flow", "A day", "A year", "Operator's take a day"],
                    [[h["attack"], h["who_pays"], h["flow"], f"{money(h['day'])} ({money(h['day_lo'])} to {money(h['day_hi'])})",
                      f"{money(h['year'])} ({money(h['year_lo'])} to {money(h['year_hi'])})",
                      f"{money(h['operator_day'])}"] for h in t["headline"]]), ""]
    md += ["## Latency arbitrage", "",
           "Dollars a day at the launch positions (69% of the $10M launch caps: $6.9M a market), by the lag the host holds and an extra spread every fill would pay; laundered means "
           "the operator keeps its toxicity score under the first tier with free round trips.", ""]
    extras = sorted({r["extra_bps"] for r in t["latency"]})
    for launder in (True, False):
        body = []
        for lag in sorted({r["lag_s"] for r in t["latency"]}):
            cells = [f"{lag:g} s"]
            for e in extras:
                r = next(r for r in t["latency"] if r["launder"] == launder and r["lag_s"] == lag and r["extra_bps"] == e)
                cells.append(money(r["usd_day_launch"]))
            body.append(cells)
        md += [f"**{'Laundered' if launder else 'Top toxicity tier (3 bps) paid'}**", "",
               md_table(["Lag"] + [f"+{e:g} bps" for e in extras], body), ""]
    md += ["Sensitivity at a 3 s lag, laundered, no extra spread:", "",
           md_table(["Variant", "A day"], [[r["variant"], f"{money(r['day'])} ({money(r['day_lo'])} to {money(r['day_hi'])})"] for r in t["latency_sensitivity"]]), ""]
    md += ["## Skimming and censoring client fills", "",
           "The house's gain from choosing each client fill's moment in its window, in bps of the fill "
           "notional and dollars a day; `fills to detect` is how many fills a monitor comparing fills "
           "with venue prices needs to reject an honest node at 0.1% (one-sided).", ""]
    body = [[r["kind"], f"{r['lag_s']:g} s", f"{r['delay_s']:g} s", f"{r['flow']:g}x",
             f"{r['bps_of_notional']:.2f} ({r['bps_lo']:.2f} to {r['bps_hi']:.2f})",
             f"{money(r['usd_day'])} ({money(r['usd_day_lo'])} to {money(r['usd_day_hi'])})", f"{r['fills_to_detect']:.0f}"]
            for r in t["skim"] if r["flow"] == 1.0 or r["kind"] == "sequenced"]
    md += [md_table(["Measure", "Lag", "Delay", "Flow", "bps of notional", "A day", "Fills to detect"], body), ""]
    md += ["## Held sync", "", md_table(
        ["Lag", "Hold", "Value a use (bps)", "Without the kill (bps)", "Added by the kill (bps)", "Kill share", "Value a use, top 1% hours (bps)", "Added a day, BTC-USD, 4 restarts"],
        [[f"{r['lag_s']:g} s", f"{r['hold_s']:g} s", f"{r['held_bps']:.2f} ({r['held_bps_lo']:.2f} to {r['held_bps_hi']:.2f})",
          f"{r['without_bps']:.2f}", f"{r['added_bps']:.2f}", f"{r['kill_share']:.2f}", f"{r['held_bps_top1pct']:.2f}",
          money(r["btc_4_kills_day"]) if r["kill_share"] > 0.005 else "no kill needed"] for r in t["hold"]]), ""]
    md += ["## Unpricing across moves", "", "Dollars a year from $50M a side of pairs, by spell length.", "",
           md_table(["Rule", "Spell", "Trigger (bps in 5 s)", "Markets", "Margin", "A year", "Paying spells a year", "Unpriced time a year"],
                    [[r["kind"], f"{r['spell_s']:g} s", "-" if r["trigger_bps_5s"] is None else f"{r['trigger_bps_5s']:g}", r["markets"], f"{r['margin']:.0%}",
                      f"{money(r['usd_year'])} ({money(r['usd_year_lo'])} to {money(r['usd_year_hi'])})",
                      f"{r['paying_spells_year']:.0f}", f"{r['unpriced_s_year'] / 3600:.1f} h"] for r in t["unprice"]]), ""]
    md += ["## Momentum after a fast minute", "",
           "The mean move over the next seconds in the direction of a trailing minute that moved at least "
           "theta: positive is momentum, negative reversal. Censoring a request longer than the lag gains "
           "only through it.", "",
           md_table(["Theta", "Ahead", "Mean continuation (bps)", "Seconds a year"],
                    [[f"{r['theta_bps']:g} bps", f"{r['ahead_s']:g} s", f"{r['continuation_bps']:.2f} ({r['lo']:.2f} to {r['hi']:.2f})",
                      f"{r['seconds_per_year']:,.0f}"] for r in t["drift"]]), ""]
    md += ["## Venue divergence", "",
           "Share of seconds the venues' last trades sit 16 and 24 bps apart (the level-1 and level-2 "
           "divergence triggers on a 25 bps bound), by stratum. Kraken's last trade lags its book, so "
           "this overstates the divergence the engine's index sees.", "",
           md_table(["Stratum", "Hours", "Level 1", "Level 2", "p99 gap (bps)"],
                    [[str(r["stratum"]), str(r["hours"]), f"{r['level1']:.2%}", f"{r['level2']:.2%}", f"{r['p99']:.1f}"] for r in t["divergence"]]), ""]
    if t.get("calibration"):
        c = t["calibration"]
        keys = [("abs_move_bps_0.25s", "move over 0.25 s"), ("abs_move_bps_1s", "move over 1 s"),
                ("abs_move_bps_3s", "move over 3 s"), ("abs_move_bps_8s", "move over 8 s"),
                ("tv_250ms_bps", "variation at 250 ms"), ("lookback_3s_bps", "worst index of the last 3 s")]
        md += ["## The index against the proxy (live recording)", "",
               f"The engine's two-venue book-mid index over the Coinbase trade proxy, on {c['summary']['BTC']['hours']} whole "
               "recorded hours: below 1 the proxy overstates what the index does.", "",
               md_table(["Statistic"] + list(c["summary"]), [[label] + [f"{c['summary'][sym]['ratio_index_over_proxy'][k]:.2f}" for sym in c["summary"]] for k, label in keys]),
               "", md_table(["Symbol", "Mean half-gap between the venues (bps)", "Highest hourly p99 (bps)", "Time past 16 bps", "Time past 24 bps"],
                            [[sym, f"{v['half_gap_bps_mean']:.2f}", f"{v['half_gap_bps_p99']:.2f}", f"{v['divergence_time_share']['16']:.2%}",
                              f"{v['divergence_time_share']['24']:.2%}"] for sym, v in c["summary"].items()]), ""]
    md += ["## What the operator risks", "",
           "The seat is one year of the operator's 20% of #134's distributions, at its share of the days. "
           "An attack pays, even if it is acted on the day it starts, when the operator's take an operating "
           "day exceeds the break-even.", "", md_table(
        ["Flow", "Removal", "Operating days", "Seat, one year", "Break-even take an operating day"],
        [[r["flow"], r["removal"], f"{r['operating_days']:g}", money(r["seat_value_one_year"]),
          money(r["breakeven_take_per_operating_day"])] for r in t["economics"]]), ""]
    md += ["## The stress day, 2025-10-10", "", md_table(
        ["Market", "Hour (UTC)", "Measure", "Dollars"],
        [[r["market"], r["hour_utc"], r["measure"], money(r["usd"])] for r in sorted(t["stress_day"], key=lambda r: (r["measure"], r["market"], r["hour_utc"]))]), ""]
    (results / "report.md").write_text("\n".join(md) + "\n")


def main(hourly_path: Path, results: Path):
    rows = load(hourly_path)
    y = Year()
    flows = {s: market_flows(s) for s in (1.0, 10.0)}
    calib_path = results / "calibration.json"
    calibration = json.loads(calib_path.read_text()) if calib_path.exists() else None
    lat, lat_sens = latency_tables(rows, y)
    sk = skim_tables(rows, y, flows)
    hd = hold_tables(rows, y)
    for r in hd:
        r["btc_4_kills_day"] = held_per_day(rows, y, "BTC-USD", 4, r["lag_s"], r["hold_s"]).day
    up = unprice_tables(rows, y)
    dr = drift_tables(rows, y)
    sd = stress_day(rows)
    dv = []
    for k in (0, 1, 2, 3, 4, "stress"):
        sel = [r for r in select(rows, attack="divergence") if r["stratum"] == k]
        dv.append({"stratum": k, "hours": len(sel), "level1": float(np.mean([r["level1_share"] for r in sel])),
                   "level2": float(np.mean([r["level2_share"] for r in sel])), "p99": float(np.mean([r["p99_bps"] for r in sel]))})
    head = headline(rows, y, calibration)
    econ = economics_table(head)
    tables = {"latency": lat, "latency_sensitivity": lat_sens, "skim": sk, "hold": hd, "unprice": up, "drift": dr,
              "stress_day": sd, "divergence": dv, "headline": head, "economics": econ}
    for name, table in tables.items():
        write_csv(results / f"{name}.csv", table)
    tables["calibration"] = calibration
    summary = {"positions": POSITIONS, "pair_notional": PAIR_NOTIONAL,
               "flow": {str(s): {m: {"fill_notional_per_day": f.fill_notional_per_day, "fills_per_second": f.fills_per_second} for m, f in fl.items()} for s, fl in flows.items()},
               "honest_distributions_per_day": economics.honest_distributions_per_day()}
    (results / "summary.json").write_text(json.dumps(summary, indent=1))
    write_report(results, tables, summary)
    plot(results, tables)
    print(f"wrote tables, figures and report.md to {results}")
