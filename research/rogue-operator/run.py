"""Runs the rogue-operator study: every attack on every sampled hour, then the year's estimates.

Usage (from a virtualenv with requirements.txt, in this directory):
    python -I run.py simulate [--workers 3]   every sampled hour -> results/hourly.csv.gz
    python -I run.py report                   results/hourly.csv.gz -> results/*.csv, *.json, report.md
    python -I run.py all [--workers 3]

`simulate` reads `data/derived/grids.npz` (`derive/grids.py`) and needs nothing fetched; it is
deterministic: every hour's client arrivals are seeded by the hour. `report` also reads
`results/calibration.json` (`derive/calibrate.py`, from the live recording) when present.
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

from model import hourly  # noqa: E402
from model.flow import market_flows  # noqa: E402
from model.paths import load_hours  # noqa: E402

RESULTS = HERE / "results"
HOURLY = RESULTS / "hourly.csv.gz"
FLOW_SCALES = (1.0, 10.0)


def simulate(workers: int):
    hours = load_hours()
    flows = {s: market_flows(s) for s in FLOW_SCALES}
    jobs = [(h, {s: flows[s][h.market] for s in FLOW_SCALES}) for h in hours]
    rows = []
    with Pool(workers) as pool:
        for i, out in enumerate(pool.imap_unordered(hourly.all_rows, jobs, chunksize=1), 1):
            rows += out
            if i % 10 == 0:
                print(f"{i}/{len(jobs)} hours", flush=True)
    keys = sorted({k for r in rows for k in r})
    RESULTS.mkdir(exist_ok=True)
    rows.sort(key=lambda r: (r["attack"], r["market"], r["hour"], json.dumps({k: r.get(k) for k in keys}, default=str)))
    # A zero modification time in the gzip header, so a rerun writes the same bytes.
    with HOURLY.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0) as gz, io.TextIOWrapper(gz, newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=keys)
        w.writeheader()
        w.writerows(rows)
    print(f"{len(rows)} rows -> {HOURLY}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", choices=["simulate", "report", "all"])
    parser.add_argument("--workers", type=int, default=3)
    args = parser.parse_args()
    if args.stage in ("simulate", "all"):
        simulate(args.workers)
    if args.stage in ("report", "all"):
        from model import report

        report.main(HOURLY, RESULTS)


if __name__ == "__main__":
    main()
