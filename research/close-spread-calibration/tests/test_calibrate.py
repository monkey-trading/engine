"""The ceilings CSV export: the column schema comes from every row, so a blocked market does not break it."""

import csv
import importlib.util
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CALIBRATE = load("calibrate")

COLUMNS = ["market_id", "symbol", "status", "proposed_ceiling_bps", "need_bps"]
PROPOSED = {"market_id": 1, "symbol": "X", "status": "proposed", "proposed_ceiling_bps": 15,
            "need_bps": 14.0}
BLOCKED = {"market_id": 2, "symbol": "Y", "status": "blocked_pending_evidence",
           "proposed_ceiling_bps": None}


class WriteCsvTests(unittest.TestCase):
    def check(self, rows):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "out.csv"
            CALIBRATE.write_csv(path, rows)
            with path.open(newline="") as handle:
                read = list(csv.DictReader(handle))
        self.assertEqual(list(read[0]), COLUMNS)
        blocked = read[rows.index(BLOCKED)]
        self.assertEqual((blocked["symbol"], blocked["status"], blocked["need_bps"]),
                         ("Y", "blocked_pending_evidence", ""))

    def test_blocked_market_first(self):
        self.check([BLOCKED, PROPOSED])

    def test_blocked_market_later(self):
        self.check([PROPOSED, BLOCKED])


if __name__ == "__main__":
    unittest.main()
