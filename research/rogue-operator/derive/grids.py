"""Builds each sampled hour's price grid from its trades: the derived dataset the models read.

Usage: python -I derive/grids.py <raw dir> <sample.json> <out grids.npz>

For each sampled hour and venue, the last trade price at or before each `STEP_MS` step from
`LEAD_S` before the hour to `TAIL_S` after it, in integer cents (every launch crypto pair trades
in cents on both venues), forward filled, and back filled before the first trade. Coinbase's
grid is the index proxy (`model/paths.py`); Kraken trades a tenth as often and is kept for the
divergence check only. The grids are delta-encoded per row, so the long runs of unchanged prices
compress well. A row records the longest stretch without a trade and the trade count, and the
hour is flagged when a stretch passes `MAX_GAP_S`.
"""

import gzip
import hashlib
import json
import sys
from pathlib import Path

import numpy as np

STEP_MS = 50
LEAD_S = 180
TAIL_S = 120
MAX_GAP_S = 60
STEPS = (LEAD_S + 3600 + TAIL_S) * 1000 // STEP_MS


def read_trades(path: Path):
    ms, price = [], []
    with gzip.open(path, "rt") as fh:
        for line in fh:
            row = json.loads(line)
            if len(row) != 3:
                raise ValueError(f"{path}: malformed row {row!r}")
            ms.append(int(row[0]))
            price.append(float(row[1]))
    return np.asarray(ms, dtype=np.int64), np.asarray(price, dtype=float)


def grid(ms: np.ndarray, price: np.ndarray, t0_ms: int):
    """Cents at each step from `t0_ms`, the longest gap between trades in seconds, and the count."""
    if len(ms) == 0:
        raise ValueError("no trades")
    order = np.argsort(ms, kind="stable")
    ms, price = ms[order], price[order]
    cents = np.rint(price * 100).astype(np.int64)
    steps = t0_ms + STEP_MS * np.arange(STEPS)
    idx = np.searchsorted(ms, steps, side="right") - 1
    out = cents[np.maximum(idx, 0)]
    edges = np.concatenate([[t0_ms], ms, [t0_ms + STEPS * STEP_MS]])
    return out, float(np.diff(edges).max() / 1000), len(ms)


def main(raw: Path, sample_path: Path, out: Path):
    from datetime import datetime, timezone

    sample = json.loads(sample_path.read_text())
    arrays, meta = {}, []
    for venue in ("coinbase", "kraken"):
        rows = []
        for entry in sample["hours"]:
            market, hour = entry["market"], int(entry["hour"])
            name = datetime.fromtimestamp(hour, timezone.utc).strftime("%Y%m%dT%H")
            path = raw / "trades" / venue / market / f"{name}.jsonl.gz"
            if not path.exists():
                raise FileNotFoundError(f"{path}: run fetch/history.py trades first")
            ms, price = read_trades(path)
            g, gap, count = grid(ms, price, (hour - LEAD_S) * 1000)
            rows.append(np.concatenate([[g[0]], np.diff(g)]).astype(np.int32))
            meta.append({
                "venue": venue, "market": market, "hour": hour, "stratum": entry["stratum"],
                "trades": count, "max_gap_s": gap, "flagged": gap > MAX_GAP_S,
                "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            })
        arrays[venue] = np.stack(rows)
    out.parent.mkdir(parents=True, exist_ok=True)
    np.savez_compressed(out, coinbase=arrays["coinbase"], kraken=arrays["kraken"], step_ms=STEP_MS, lead_s=LEAD_S, tail_s=TAIL_S)
    out.with_suffix(".json").write_text(json.dumps(meta, indent=1))
    flagged = [m for m in meta if m["flagged"]]
    print(f"{len(meta)} grids, {len(flagged)} flagged: " + ", ".join(f"{m['venue']} {m['market']} {m['hour']} {m['max_gap_s']:.0f}s" for m in flagged))


if __name__ == "__main__":
    main(Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]))
