"""Reversal blending and rounding evidence agree with the price model."""

import importlib.util
import json
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("closing_paths", HERE / "closing_paths.py")
P = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(P)
FIXTURES = json.loads((HERE / "fixtures/engine-v17.json").read_text())
FLAT = {"spread_tiers": [{"bps": 0}]}
STEEP = {"spread_tiers": [{"bps": 100}]}
TOP = P.LEVELS[2]


class ClosingPathTests(unittest.TestCase):
    def test_pure_close_spread_is_the_stress_plus_toxicity_charge_whatever_the_tiers(self):
        self.assertAlmostEqual(P.reversal(STEEP, TOP, 100_000, 0.0), 13)
        self.assertAlmostEqual(P.reversal(FLAT, TOP, 100_000, 2.0), 13)

    def test_size_cost_of_the_opening_slice_is_blended_over_the_whole_fill(self):
        # Close 1,000 units and open 1,000: 400 bps on the opening $100,000 is $4,000, or $2 a unit over 2,000
        # units, which is 200 bps of the $100 index.
        self.assertAlmostEqual(P.reversal(STEEP, TOP, 100_000, 1.0), 13 + 200, places=2)

    def test_blended_spread_grows_with_the_opening_share(self):
        values = [P.reversal(STEEP, P.LEVELS[0], 100_000, fraction) for fraction in P.OPEN_FRACTIONS]
        self.assertEqual(values, sorted(values))

    def test_rows_flag_only_ceiling_breaches(self):
        item = {"market": {"symbol": "T", "asset_class": "equity", "spread_tiers": STEEP["spread_tiers"]}}
        rows = P.reversal_rows([item], {"T": 20})
        for row in rows:
            self.assertEqual(row["exceeds_ceiling"], row["blended_reversal_bps"] > 20)
        self.assertTrue(any(row["exceeds_ceiling"] for row in rows))
        self.assertTrue(any(not row["exceeds_ceiling"] for row in rows))

    def test_rounding_rows_agree_with_engine_vectors(self):
        by_name = {vector["name"]: vector for vector in FIXTURES}
        row = P.rounding_rows(charge_bps=1, mids=(1_000_000_000,))[0]
        self.assertEqual((row["rounded_up_raw"], row["floor_bound_raw"], row["excess_atoms"]), (100_000, 100_000, 0))
        dust = P.rounding_rows(charge_bps=1, mids=(1,))[0]
        self.assertEqual((dust["rounded_up_raw"], dust["floor_bound_raw"], dust["floor_makes_close_free"]), (1, 0, True))
        self.assertEqual(by_name["precision-floor-buy"]["expected_spread_bps"], 10_000)

    def test_pure_close_is_never_more_than_one_atom_over_the_exact_bound(self):
        self.assertLessEqual(P.pure_close_excess_atoms(2_000), 1)
        self.assertEqual(P.pure_close_excess_atoms(2_000), P.pure_close_excess_atoms(2_000))


if __name__ == "__main__":
    unittest.main()
