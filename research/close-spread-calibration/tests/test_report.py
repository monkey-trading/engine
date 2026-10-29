"""The report builds figures when a market is blocked."""

import copy
import importlib.util
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
        import json
        data = json.loads((HERE / "results/ceilings.json").read_text())
        statistics = copy.deepcopy(json.loads((HERE / "results/market_statistics.json").read_text()))
        data["markets"][0].update(proposed_ceiling_bps=None, status="blocked_pending_evidence")
        statistics["markets"][0]["calibration"].pop("hourly_p99_week_block_bootstrap")
        with tempfile.TemporaryDirectory() as directory, patch.object(REPORT, "RESULTS", Path(directory)):
            REPORT.plots(data, statistics)
            self.assertTrue((Path(directory) / "ceilings_vs_need.png").exists())
            self.assertTrue((Path(directory) / "hourly_tails.png").exists())


if __name__ == "__main__":
    unittest.main()
