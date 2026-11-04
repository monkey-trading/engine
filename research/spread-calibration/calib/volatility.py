"""Each launch market's expected move over the price-lag window.

Two sources:
- the crypto markets' sampled hours of venue trades (`research/rogue-operator`), on which the mean
  absolute move over `h` seconds is measured directly, hour by hour, and weighted to the year by
  the sample's strata;
- every market's two years of hourly bars (`research/cap-calibration`), whose Parkinson volatility
  per open hour, `ln(high / low) / sqrt(4 ln 2)`, is scaled to `h` seconds as a Brownian path would
  scale, `sigma * sqrt(h / 3600) * sqrt(2 / pi)`, and then by `RHO[h]`, the ratio the crypto hours
  measure between their short-horizon moves and that scaling (below 1: over seconds, prices move
  less than an hour's range implies). The ratio is crypto's; applying it to FX, indices, commodities
  and shares, whose short-horizon behaviour this study does not observe, is an assumption.
"""

import json
from pathlib import Path

import numpy as np

from calib.common import STUDIES

HORIZONS_S = (0.25, 1.0, 3.0)
CLOSED = np.iinfo(np.int32).min
QUANTILES = (50, 90, 99)


def brownian(sigma_hour, horizon_s: float):
    """The mean absolute move over `horizon_s` of a Brownian path with hourly volatility `sigma_hour`."""
    return np.asarray(sigma_hour) * np.sqrt(horizon_s / 3600) * np.sqrt(2 / np.pi)


def weighted_percentile(values: np.ndarray, weights: np.ndarray, q: float) -> float:
    order = np.argsort(values)
    v, w = np.asarray(values)[order], np.asarray(weights)[order]
    cum = np.cumsum(w) / w.sum()
    return float(np.interp(q / 100, cum, v))


def crypto_hours(rogue: Path = STUDIES["rogue"]) -> list[dict]:
    """Per sampled hour of the year (stress hours left out): its market, stratum weight, Parkinson
    volatility over the hour, and mean absolute move at each horizon."""
    derived = rogue / "data" / "derived"
    z = np.load(derived / "grids.npz")
    meta = [m for m in json.loads((derived / "grids.json").read_text()) if m["venue"] == "coinbase"]
    sample = json.loads((derived / "sample.json").read_text())
    step_s = float(z["step_ms"]) / 1000
    lead = int(round(float(z["lead_s"]) / step_s))
    n = int(round(3600 / step_s))
    x_all = np.log(np.cumsum(z["coinbase"].astype(np.int64), axis=1) / 100.0)
    counts = {}
    for m in meta:
        if m["stratum"] != "stress":
            counts[(m["market"], m["stratum"])] = counts.get((m["market"], m["stratum"]), 0) + 1
    out = []
    for i, m in enumerate(meta):
        if m["stratum"] == "stress":
            continue
        x = x_all[i, lead : lead + n]
        strata = sample["markets"][m["market"]]["strata"]
        weight = strata[m["stratum"]]["hours"] / counts[(m["market"], m["stratum"])]
        row = {"market": m["market"], "hour": m["hour"], "weight": weight,
               "sigma_hour": float((x.max() - x.min()) / np.sqrt(4 * np.log(2)))}
        for h in HORIZONS_S:
            k = int(round(h / step_s))
            row[f"move_{h}"] = float(np.abs(x[k:] - x[:-k]).mean())
        out.append(row)
    return out


def rho(hours: list[dict]) -> dict[float, float]:
    """The crypto hours' median ratio of measured to Brownian-scaled moves, per horizon."""
    return {h: float(np.median([r[f"move_{h}"] / brownian(r["sigma_hour"], h) for r in hours if r["sigma_hour"] > 0]))
            for h in HORIZONS_S}


def hourly_sigma(cap: Path = STUDIES["cap"]) -> dict[str, np.ndarray]:
    """Each market's Parkinson volatility in every open hour with a range, from the hourly bars."""
    z = np.load(cap / "data" / "derived" / "hourly_bars.npz")
    bars, symbols = z["bars"], [str(s) for s in z["symbols"]]
    out = {}
    for i, s in enumerate(symbols):
        b = bars[:, i, :]
        is_open = b[:, 0] != CLOSED
        sigma = (b[is_open, 1] - b[is_open, 2]) / 1e6 / np.sqrt(4 * np.log(2))
        out[s] = sigma[sigma > 0]
    return out


def lag_moves(cap: Path = STUDIES["cap"], rogue: Path = STUDIES["rogue"]) -> dict:
    """Per market and horizon, the mean absolute move in the median, p90 and p99 open hour, in
    bps: measured for crypto, scaled from hourly bars for the rest, with both given for crypto."""
    hours = crypto_hours(rogue)
    ratios = rho(hours)
    sigma = hourly_sigma(cap)
    out = {"rho": {str(k): v for k, v in ratios.items()}, "markets": {}}
    for market, s in sigma.items():
        row = {}
        for h in HORIZONS_S:
            for q in QUANTILES:
                row[f"scaled_{h}_p{q}"] = float(ratios[h] * brownian(np.percentile(s, q), h) * 1e4)
        sel = [r for r in hours if r["market"] == market]
        if sel:
            w = np.array([r["weight"] for r in sel])
            for h in HORIZONS_S:
                v = np.array([r[f"move_{h}"] for r in sel])
                for q in QUANTILES:
                    row[f"measured_{h}_p{q}"] = weighted_percentile(v, w, q) * 1e4
        out["markets"][market] = row
    return out
