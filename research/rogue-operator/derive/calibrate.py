"""Compares the engine's two-venue book-mid index with the Coinbase trade proxy on the live recording.

Usage: python -I derive/calibrate.py <live dir> <out calibration.json>

For each whole UTC hour `fetch/record_books.py` recorded on both venues, and each symbol, the
50 ms grids of Coinbase's last trade (the proxy the historical sample uses), Coinbase's and
Kraken's book mids, and the index, their midpoint, all by venue stamp. Reported per hour and
symbol, for the index and the proxy:
- the mean absolute move over each horizon in `HORIZONS_S`;
- the total variation at 250 ms, what latency arbitrage at no spread collects;
- the mean worst index a 3 s look-back offers an order against its arrival, what skimming takes;
and, for the index alone, the half gap between the venues' mids in bps (what choosing one venue
moves the index by) and the share of time the venues diverge past the level-1 and level-2
divergence triggers (16 and 24 bps on a 25 bps bound).
"""

import gzip
import json
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np

STEP_MS = 50
HORIZONS_S = (0.25, 0.5, 1, 2, 3, 5, 8)
TRIGGERS_BPS = (16, 24)


def read(path: Path):
    books, trades = defaultdict(list), defaultdict(list)
    with gzip.open(path, "rt") as fh:
        for line in fh:
            parts = line.split()
            try:
                if parts[0] == "B" and len(parts) == 6:
                    bid, ask = float(parts[4]), float(parts[5])
                    if 0 < bid < ask:
                        books[parts[1]].append((int(parts[3]), (bid + ask) / 2))
                elif parts[0] == "T" and len(parts) == 7:
                    price = float(parts[4])
                    if price > 0:
                        trades[parts[1]].append((int(parts[3]), price))
            except ValueError:
                continue
    return books, trades


def on_grid(rows, t0: int, steps: int):
    if not rows:
        return None
    rows = sorted(rows)
    ms = np.array([r[0] for r in rows])
    v = np.array([r[1] for r in rows])
    # Back filled before the first quote, which `main` requires within the hour's first minute.
    idx = np.maximum(np.searchsorted(ms, t0 + STEP_MS * np.arange(steps), side="right") - 1, 0)
    return np.log(v[idx])


def stats(x: np.ndarray) -> dict:
    out = {}
    for h in HORIZONS_S:
        k = int(round(h * 1000 / STEP_MS))
        out[f"abs_move_bps_{h}s"] = float(np.abs(x[k:] - x[:-k]).mean() * 1e4)
    out["tv_250ms_bps"] = float(np.abs(np.diff(x[:: 250 // STEP_MS])).sum() * 1e4)
    w = 3000 // STEP_MS
    from numpy.lib.stride_tricks import sliding_window_view

    win = sliding_window_view(x, w + 1)
    out["lookback_3s_bps"] = float(((win.max(axis=1) - win[:, -1]) + (win[:, -1] - win.min(axis=1))).mean() / 2 * 1e4)
    return out


def main(live: Path, out: Path):
    hours = defaultdict(dict)
    for path in sorted(live.glob("*.txt.gz")):
        venue, stamp = path.name.split(".")[0].split("-")
        hours[stamp][venue] = path
    rows = []
    for stamp, files in sorted(hours.items()):
        if set(files) != {"coinbase", "kraken"}:
            continue
        cb_books, cb_trades = read(files["coinbase"])
        kr_books, _ = read(files["kraken"])
        from datetime import datetime, timezone

        t0 = int(datetime.strptime(stamp, "%Y%m%d%H").replace(tzinfo=timezone.utc).timestamp() * 1000)
        steps = 3600 * 1000 // STEP_MS
        for sym in sorted(set(cb_books) & set(kr_books)):
            cb_mid = on_grid(cb_books[sym], t0, steps)
            kr_mid = on_grid(kr_books[sym], t0, steps)
            proxy = on_grid(cb_trades[sym], t0, steps)
            if cb_mid is None or kr_mid is None or proxy is None:
                continue
            # A recording that started or stopped inside the hour leaves its edge flat: keep the hour
            # only if both venues quote within its first and last minute.
            first = min(r[0] for r in cb_books[sym]), min(r[0] for r in kr_books[sym])
            last = max(r[0] for r in cb_books[sym]), max(r[0] for r in kr_books[sym])
            if max(first) > t0 + 60_000 or min(last) < t0 + 3_540_000:
                continue
            index = np.log((np.exp(cb_mid) + np.exp(kr_mid)) / 2)
            gap = np.abs(np.exp(cb_mid) - np.exp(kr_mid)) / np.exp(index) * 1e4
            row = {"hour": stamp, "symbol": sym, "index": stats(index), "proxy": stats(proxy)}
            half = gap / 2
            row["half_gap_bps"] = {"mean": float(half.mean()), **{q: float(np.percentile(half, p)) for q, p in (("p50", 50), ("p90", 90), ("p99", 99), ("max", 100))}}
            row["divergence_time_share"] = {str(t): float((gap >= t).mean()) for t in TRIGGERS_BPS}
            rows.append(row)
    summary = {}
    for sym in sorted({r["symbol"] for r in rows}):
        sel = [r for r in rows if r["symbol"] == sym]
        summary[sym] = {
            "hours": len(sel),
            "ratio_index_over_proxy": {k: float(np.mean([r["index"][k] for r in sel]) / np.mean([r["proxy"][k] for r in sel])) for k in sel[0]["index"]},
            "half_gap_bps_mean": float(np.mean([r["half_gap_bps"]["mean"] for r in sel])),
            "half_gap_bps_p99": float(np.max([r["half_gap_bps"]["p99"] for r in sel])),
            "divergence_time_share": {t: float(np.mean([r["divergence_time_share"][t] for r in sel])) for t in sel[0]["divergence_time_share"]},
        }
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({"hours": rows, "summary": summary}, indent=1))
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main(Path(sys.argv[1]), Path(sys.argv[2]))
