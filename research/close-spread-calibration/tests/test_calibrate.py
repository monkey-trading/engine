"""The calibrate helpers: the ceilings CSV schema and the gap comparison with blocked markets."""

import csv
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CALIBRATE = load("calibrate")
REPORT = load("report")

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


def effects_row(symbol, band, case, shortfall="100", cost_bps=13):
    return {"symbol": symbol, "margin_band": band, "case": case,
            "house_cash_shortfall_usd_zero_capital": shortfall, "close_cost_bps": cost_bps}


class GapComparisonTests(unittest.TestCase):
    def test_blocked_eur_usd_names_another_market(self):
        rows = [effects_row("GBP-USD", 0, "at_ceiling"), effects_row("GBP-USD", 0, "no_cap_operator_attack"),
                effects_row("USD-JPY", 0, "at_ceiling"), effects_row("USD-JPY", 0, "no_cap_operator_attack")]
        result = CALIBRATE.gap_comparison(rows)
        self.assertEqual(result["market"], "GBP-USD")
        self.assertEqual(result["trader_cost_ratio"], 1)

    def test_empty_effects_skip_with_a_reason(self):
        self.assertEqual(CALIBRATE.gap_comparison([]),
                         {"market": None, "skipped": "no market has effects rows"})


class BlockedFxReportTests(unittest.TestCase):
    def check_report(self, gap, needle):
        data = json.loads((HERE / "results/ceilings.json").read_text())
        for row in data["markets"]:
            if row["asset_class"] == "fx":
                row.update(proposed_ceiling_bps=None, status="blocked_pending_evidence")
        data["gap_comparison"] = gap
        with tempfile.TemporaryDirectory() as directory, patch.object(REPORT, "RESULTS", Path(directory)):
            REPORT.write_report(data)
            report = (Path(directory) / "report.md").read_text()
        self.assertIn(needle, report)

    def test_report_handles_a_blocked_fx_class(self):
        # No FX market is live, so `effects` would carry no EUR-USD rows and the comparison names another market.
        rows = [effects_row("AAPL", 0, "at_ceiling"), effects_row("AAPL", 0, "no_cap_operator_attack")]
        self.check_report(CALIBRATE.gap_comparison(rows), "in a 5% gap on AAPL, ")

    def test_report_states_the_reason_when_the_gap_comparison_skips(self):
        self.check_report({"market": None, "skipped": "no market has effects rows"},
                          "the gap comparison is skipped (no market has effects rows).")


if __name__ == "__main__":
    unittest.main()
