"""Tests for historical-data units, closed hours, and fail-closed pinning."""

import importlib.util
import tempfile
import tomllib
import unittest
from contextlib import nullcontext
from unittest.mock import patch
from pathlib import Path

import numpy as np

SPEC = importlib.util.spec_from_file_location("calibration", Path(__file__).resolve().parents[1] / "run.py")
MODEL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODEL)


class InputTests(unittest.TestCase):
    def test_hourly_tail_is_the_in_bar_move_and_excludes_gap_and_closed_sentinel(self):
        closed = np.iinfo(np.int32).min
        rows = np.array([[1000, 0, 0, 2000], [closed, closed, closed, closed]], dtype=np.int32)
        result = MODEL.summarize(rows, 1e6)
        self.assertEqual(result["active_hours"], 1)
        self.assertAlmostEqual(result["abs_hourly_p99_bps"], np.expm1(0.002) * 10_000)
        self.assertAlmostEqual(result["abs_gap_max_bps"], np.expm1(0.001) * 10_000)

    def test_no_trades_produces_missing_statistics(self):
        result = MODEL.summarize(np.empty((0, 4), dtype=np.int32), 1e6)
        self.assertEqual(result["active_hours"], 0)
        self.assertIsNone(result["abs_hourly_p99_bps"])

    def test_week_resampling_preserves_observed_tail_range_and_is_seeded(self):
        rows = np.zeros((168 * 3, 4), dtype=np.int32)
        rows[168:, 3] = 10_000
        first = MODEL.uncertainty(rows, 1e6)
        self.assertEqual(first, MODEL.uncertainty(rows, 1e6))
        self.assertEqual(first["p025_bps"], 0)
        self.assertAlmostEqual(first["p975_bps"], np.expm1(0.01) * 10_000)
        gap_only = np.zeros((168 * 3, 4), dtype=np.int32)
        gap_only[:, 0] = 50_000
        self.assertEqual(MODEL.uncertainty(gap_only, 1e6)["p975_bps"], 0)
        self.assertIsNone(MODEL.uncertainty(rows[:167], 1e6))

    def test_changed_input_refuses_calibration(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "data"
            path.write_bytes(b"different input")
            with self.assertRaisesRegex(ValueError, "input changed"):
                MODEL.verify(path, "0" * 64)

    def test_pinned_inventory_has_every_launch_market_and_a_chronological_split(self):
        result = MODEL.build()
        self.assertEqual(len(result["markets"]), 37)
        self.assertEqual(result["calibration_weeks"] + result["validation_weeks"], 102)
        self.assertLess(result["week0_utc"], result["validation_start_utc"])
        self.assertLess(result["validation_start_utc"], result["end_exclusive_utc"])

    def test_inventory_digest_ignores_calendars_but_not_price_rules(self):
        with tempfile.TemporaryDirectory() as directory:
            text = (MODEL.REPO / "config/production-genesis.toml").read_text()
            path = Path(directory) / "genesis.toml"
            path.write_text(text + "\n# a comment\n")
            self.assertEqual(MODEL.inventory_digest(path), MODEL.INVENTORY_SHA256)
            path.write_text(text.replace("max_divergence_bps = 25", "max_divergence_bps = 26", 1))
            self.assertNotEqual(MODEL.inventory_digest(path), MODEL.INVENTORY_SHA256)

    def test_future_holdout_moves_do_not_change_calibration_statistics(self):
        genesis = tomllib.loads((MODEL.REPO / "config/production-genesis.toml").read_text())["genesis"]
        bars = np.zeros((102 * 168, 37, 4), dtype=np.int32)
        bars[MODEL.CALIBRATION_WEEKS * 168:, :, 3] = 10_000
        archive = {"symbols": np.array([m["symbol"] for m in genesis["markets"]]), "bars": bars,
                   "unit": np.array(1e6), "week0": np.array("2024-10-28T00:00:00+00:00")}
        with patch.object(MODEL.np, "load", return_value=nullcontext(archive)):
            result = MODEL.build()
        for market in result["markets"]:
            self.assertEqual(market["calibration"]["abs_hourly_p99_bps"], 0)
            self.assertAlmostEqual(market["validation"]["abs_hourly_p99_bps"], np.expm1(.01) * 10_000)


if __name__ == "__main__":
    unittest.main()
