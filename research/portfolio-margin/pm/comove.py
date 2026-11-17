"""Co-movement of every pair of launch markets: one hour, one day, and across each kind of closure.

For each pair and each scale: the number of observations, the body correlation (each series clipped
at its 1st and 99th percentiles, so a handful of jumps does not set it) and the tail dependence, the
chance that one is in its own 5% tail (10% for the closures, which are fewer) on the same side given
the other is, whose value under independence is the tail share itself.

- one hour: the cap study's hourly returns, the hours both markets are open;
- one day: close-to-close returns over the dates both have a close, from 2016;
- a closure: the gaps over closures of one kind, a pair's matched by the hour the markets last traded
  before it (the week for weekends, the day for the overnight closures, the reopening for breaks).

`lags` reads whether a member of a candidate group follows another: the correlation of one's return
with the other's, one to three hours (two days) later, against the same-time one.
"""

import numpy as np

from pm import daily
from pm.hourly import Hourly
from pm.stats import coexceed, corr, winsor

MIN_OBS = 30
KEY = {"weekend": lambda last, reopen: last // 168, "closure": lambda last, reopen: last // 24, "break": lambda last, reopen: reopen}


def closure_table(hr: Hourly) -> dict[tuple[str, str], dict[int, float]]:
    out = {}
    for s in hr.symbols:
        for kind, last, reopen, gap in hr.closure_gaps(s):
            out.setdefault((s, kind), {}).setdefault(KEY[kind](last, reopen), gap)
    return out


def pair_stats(a: np.ndarray, b: np.ndarray, p: float) -> dict:
    n = len(a)
    if n < MIN_OBS:
        return {"n": n, "body": float("nan"), "tail": float("nan")}
    return {"n": n, "body": corr(winsor(a), winsor(b)), "tail": coexceed(a, b, p)}


def pairs(hr: Hourly) -> list[dict]:
    dates, dsyms, dx = daily.load()
    dcol = {s: i for i, s in enumerate(dsyms)}
    clos = closure_table(hr)
    rows = []
    syms = hr.symbols
    for i, a in enumerate(syms):
        for b in syms[i + 1 :]:
            row = {"a": a, "b": b}
            ra, rb = hr.hour_ret[:, hr.index[a]], hr.hour_ret[:, hr.index[b]]
            ok = np.isfinite(ra) & np.isfinite(rb)
            for k, v in pair_stats(ra[ok], rb[ok], 0.05).items():
                row[f"hour_{k}"] = v
            d = daily.returns(dx, [dcol[a], dcol[b]])
            for k, v in pair_stats(d[:, 0], d[:, 1], 0.05).items():
                row[f"day_{k}"] = v
            for kind in ("weekend", "closure", "break"):
                ga, gb = clos.get((a, kind), {}), clos.get((b, kind), {})
                keys = sorted(set(ga) & set(gb))
                for k, v in pair_stats(np.array([ga[x] for x in keys]), np.array([gb[x] for x in keys]), 0.10).items():
                    row[f"{kind}_{k}"] = v
            rows.append(row)
    return rows


def lags(hr: Hourly, groups: dict[str, list[str]]) -> list[dict]:
    dates, dsyms, dx = daily.load()
    dcol = {s: i for i, s in enumerate(dsyms)}
    rows = []
    for g, members in groups.items():
        for a in members:
            for b in members:
                if a >= b:
                    continue
                ra, rb = hr.hour_ret[:, hr.index[a]], hr.hour_ret[:, hr.index[b]]
                row = {"group": g, "a": a, "b": b}
                for k in range(-3, 4):
                    x, y = (ra[: len(ra) - k], rb[k:]) if k >= 0 else (ra[-k:], rb[: len(rb) + k])
                    ok = np.isfinite(x) & np.isfinite(y)
                    row[f"hour_lag{k:+d}"] = corr(x[ok], y[ok]) if ok.sum() > MIN_OBS else float("nan")
                d = daily.returns(dx, [dcol[a], dcol[b]])
                for k in range(-2, 3):
                    x, y = (d[: len(d) - k, 0], d[k:, 1]) if k >= 0 else (d[-k:, 0], d[: len(d) + k, 1])
                    row[f"day_lag{k:+d}"] = corr(x, y)
                rows.append(row)
    return rows


def group_summary(pairs: list[dict], groups: dict[str, list[str]]) -> list[dict]:
    """The median of each scale's body correlation and tail dependence over a group's pairs, and over
    the pairs across groups of one asset class apart (the group's pairs against every other pair)."""
    rows = []
    where = {m: g for g, ms in groups.items() for m in ms}
    for g, members in groups.items():
        inside = [p for p in pairs if where.get(p["a"]) == g and where.get(p["b"]) == g]
        outside = [p for p in pairs if (where.get(p["a"]) == g) != (where.get(p["b"]) == g)]
        for label, sel in (("within", inside), ("to_others", outside)):
            row = {"group": g, "pairs": label, "count": len(sel)}
            for k in ("hour", "day", "weekend", "closure", "break"):
                for m in ("body", "tail"):
                    v = [p[f"{k}_{m}"] for p in sel if np.isfinite(p.get(f"{k}_{m}", float("nan")))]
                    row[f"{k}_{m}_median"] = float(np.median(v)) if v else float("nan")
                    row[f"{k}_{m}_min"] = float(np.min(v)) if v else float("nan")
            rows.append(row)
    return rows
