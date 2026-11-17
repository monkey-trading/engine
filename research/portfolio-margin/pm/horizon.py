"""How far a market moves over the horizon, from the 50 ms grids where they exist.

`research/rogue-operator/data/derived/grids.npz` holds Coinbase's last trade of sampled hours of
BTC-USD, ETH-USD and SOL-USD on a 50 ms grid, stratified by volatility, with each stratum's
population in `sample.json`. Each hour carries the weight of its stratum's population over the
hours sampled in it; the stress hours are left out. For a horizon `w`, the move is the absolute
log change over every window of `w` seconds (a quarter-window stride) in the analysed hour.
"""

import json

import numpy as np

from pm.common import ROGUE
from pm.stats import wquantile

HORIZONS = (1, 5, 10, 30, 60, 300, 3600)


def load():
    z = np.load(ROGUE / "grids.npz")
    meta = [m for m in json.loads((ROGUE / "grids.json").read_text()) if m["venue"] == "coinbase"]
    sample = json.loads((ROGUE / "sample.json").read_text())
    step = float(z["step_ms"]) / 1000
    lead = int(round(float(z["lead_s"]) / step))
    x = np.log(np.cumsum(z["coinbase"].astype(np.int64), axis=1) / 100.0)
    count = {}
    for m in meta:
        count[(m["market"], m["stratum"])] = count.get((m["market"], m["stratum"]), 0) + 1
    rows = []
    for i, m in enumerate(meta):
        if m["stratum"] == "stress":
            continue
        pop = sample["markets"][m["market"]]["strata"][int(m["stratum"])]["hours"]
        rows.append((m["market"], x[i, lead : lead + int(round(3600 / step))], pop / count[(m["market"], m["stratum"])]))
    return rows, step


def moves(rows, step: float, market: str, seconds: float) -> tuple[np.ndarray, np.ndarray]:
    """(|moves|, weights) of every window of `seconds` over `market`'s sampled hours."""
    w0 = int(round(seconds / step))
    stride = max(w0 // 4, 1)
    vals, wts = [], []
    for mk, x, weight in rows:
        if mk != market:
            continue
        w = min(w0, len(x) - 1)
        d =np.abs(x[w::stride] - x[:-w:stride][: len(x[w::stride])])
        vals.append(d)
        wts.append(np.full(len(d), weight / len(d)))
    return np.concatenate(vals), np.concatenate(wts)


def table(qs=(0.99, 0.995, 0.999)) -> list[dict]:
    rows, step = load()
    out = []
    for market in sorted({r[0] for r in rows}):
        for q in qs:
            ref = None
            for h in HORIZONS[::-1]:
                v, w = moves(rows, step, market, h)
                qh = wquantile(v, w, q)
                ref = qh if ref is None else ref
                out.append({"market": market, "q": q, "seconds": h, "move": qh, "ratio_to_hour": qh / ref,
                            "sqrt_time_ratio": (h / 3600) ** 0.5, "windows": len(v)})
    return out


def ratio(rows_table: list[dict], seconds: float, q: float) -> float:
    """The larger, over the grid markets, of the horizon's move relative to the hour's, and of
    square-root time: the factor that takes a market's one-hour quantile to the horizon."""
    r = [x["ratio_to_hour"] for x in rows_table if x["seconds"] == seconds and x["q"] == q]
    return max(max(r), (seconds / 3600) ** 0.5)
