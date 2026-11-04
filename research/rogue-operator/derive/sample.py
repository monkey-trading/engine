"""Stratifies the year's hours by volatility and draws the sample of hours whose trades are pulled.

Usage: python -I derive/sample.py <raw dir> <out sample.json>

Each market's hours from `fetch/history.py`'s `START` to `END` are ranked by their Parkinson
volatility, `ln(high / low) / sqrt(4 ln 2)`, from Coinbase's hourly candles, and split into the
strata `STRATA` (shares of the hours, quietest first). Each stratum contributes `PER_STRATUM`
hours drawn without replacement at seed `SEED`; the estimator in `model/stats.py` weighs each
stratum by its share of the year's hours, so the busiest hours are sampled densely without
overweighting them. The stress day's `STRESS_HOURS` most volatile hours are listed separately.
"""

import json
import math
import sys
from pathlib import Path

import numpy as np

SEED = 549
# (upper rank share, sampled hours): quietest half, then 50-80%, 80-95%, 95-99% and the top 1%.
STRATA = [(0.50, 10), (0.80, 10), (0.95, 10), (0.99, 10), (1.00, 12)]
STRESS_HOURS = 4
MARKETS = ["BTC-USD", "ETH-USD", "SOL-USD"]


def parkinson(high, low):
    return np.log(np.asarray(high) / np.asarray(low)) / math.sqrt(4 * math.log(2))


def stratify(times, vol, strata, rng):
    """Strata of `times` by `vol` rank: each stratum's bounds, its hours, and the sampled ones."""
    order = np.argsort(vol, kind="stable")
    n = len(order)
    out, lo = [], 0
    for upper, k in strata:
        hi = int(round(upper * n))
        members = order[lo:hi]
        pick = rng.choice(members, size=min(k, len(members)), replace=False)
        out.append({
            "rank_from": lo / n,
            "rank_to": hi / n,
            "hours": int(len(members)),
            "vol_from": float(vol[members].min()),
            "vol_to": float(vol[members].max()),
            "sampled": sorted(int(times[i]) for i in pick),
        })
        lo = hi
    return out


def main(raw: Path, out: Path):
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from fetch.history import END, START, STRESS_DAY

    rng = np.random.default_rng(SEED)
    sample = {"seed": SEED, "start": START.isoformat(), "end": END.isoformat(), "markets": {}, "hours": []}
    for market in MARKETS:
        rows = np.array(json.loads((raw / "candles" / f"{market}.json").read_text()))
        t = rows[:, 0].astype(np.int64)
        vol = parkinson(rows[:, 2], rows[:, 3])
        year = (t >= START.timestamp()) & (t < END.timestamp())
        strata = stratify(t[year], vol[year], STRATA, rng)
        day = (t >= STRESS_DAY.timestamp()) & (t < STRESS_DAY.timestamp() + 86400)
        stress = sorted(int(x) for x in t[day][np.argsort(vol[day])[::-1][:STRESS_HOURS]])
        sample["markets"][market] = {
            "year_hours": int(year.sum()),
            "missing_hours": int(round((END - START).total_seconds() / 3600)) - int(year.sum()),
            "strata": strata,
            "stress": stress,
            "stress_vol": [float(vol[t == h][0]) for h in stress],
        }
        for k, s in enumerate(strata):
            sample["hours"] += [{"market": market, "hour": h, "stratum": k} for h in s["sampled"]]
        sample["hours"] += [{"market": market, "hour": h, "stratum": "stress"} for h in stress]
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(sample, indent=1))
    print(f"{len(sample['hours'])} hours sampled")


if __name__ == "__main__":
    main(Path(sys.argv[1]), Path(sys.argv[2]))
