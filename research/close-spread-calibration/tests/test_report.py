"""The report builds figures when a market is blocked, and both figures leave it out."""

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("report", HERE / "report.py")
REPORT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REPORT)


class ReportTests(unittest.TestCase):
    def test_blocked_market_is_left_out_of_both_figures(self):
        data = json.loads((HERE / "results/ceilings.json").read_text())
        statistics = json.loads((HERE / "results/market_statistics.json").read_text())
        blocked_symbol = data["markets"][0]["symbol"]
        data["markets"][0].update(proposed_ceiling_bps=None, status="blocked_pending_evidence")
        with tempfile.TemporaryDirectory() as directory, patch.object(REPORT, "RESULTS", Path(directory)):
            ceiling_labels, tail_labels = REPORT.plots(data, statistics)
            self.assertTrue((Path(directory) / "ceilings_vs_need.png").exists())
            self.assertTrue((Path(directory) / "hourly_tails.png").exists())
        for labels in (ceiling_labels, tail_labels):
            self.assertNotIn(blocked_symbol, labels)
        self.assertIn(data["markets"][1]["symbol"], ceiling_labels)
        self.assertIn(data["markets"][1]["symbol"], tail_labels)


if __name__ == "__main__":
    unittest.main()
