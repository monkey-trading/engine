"""Result files: CSV rows and JSON, written the same way every run."""

import csv
import json
from pathlib import Path

from pm.common import RESULTS


def write_rows(name: str, rows: list[dict]) -> Path:
    path = RESULTS / name
    path.parent.mkdir(parents=True, exist_ok=True)
    cols = list(dict.fromkeys(k for r in rows for k in r))
    with path.open("w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=cols, lineterminator="\n")
        w.writeheader()
        for r in rows:
            w.writerow({k: (f"{v:.6g}" if isinstance(v, float) else v) for k, v in r.items()})
    return path


def write_json(name: str, obj) -> Path:
    path = RESULTS / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, indent=1, sort_keys=True) + "\n")
    return path


def read_json(name: str):
    return json.loads((RESULTS / name).read_text())
