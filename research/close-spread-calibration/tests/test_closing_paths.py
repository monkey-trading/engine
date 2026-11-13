"""Reversal blending, at V52, and rounding evidence, at V17, agree with the price model."""

import importlib.util
import json
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("closing_paths", HERE / "closing_paths.py")
P = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(P)
FIXTURES = json.loads((HERE / "fixtures/engine-v17.json").read_text())
FLAT = {"spread_tiers": [{"tenth_bps": 0}]}
STEEP = {"spread_tiers": [{"tenth_bps": 1_000}]}
# 1 bp to $100,000 of notional, 100 bps above.
BANDED = {"spread_tiers": [{"up_to": "100000", "tenth_bps": 10}, {"tenth_bps": 1_000}]}
TOP = P.LEVELS[2]


class ClosingPathTests(unittest.TestCase):
    def test_bands_read_in_tenths_and_whole_bps_read_as_ten_tenths(self):
        self.assertEqual(P.tiers({"spread_tiers": [{"up_to": "1", "bps": 2}, {"tenth_bps": 7}]}),
                         [(P.EXEC.raw("1"), 20), (None, 7)])

    def test_pure_close_pays_its_closing_slice_through_the_bands(self):
        # Closing $100,000 at 100 bps and 4x is $4,000 over 1,000 units: 400 bps, on top of the 13.
        self.assertAlmostEqual(P.reversal(STEEP, TOP, 100_000, 0.0), 13 + 400)
        self.assertAlmostEqual(P.reversal(FLAT, TOP, 100_000, 2.0), 13)

    def test_both_slices_pay_before_the_multiplier_and_blend_over_the_whole_fill(self):
        # Close 1,000 units and open 1,000: 100 bps on $200,000 at 4x is $8,000, or $4 a unit over 2,000 units,
        # which is 400 bps of the $100 index.
        self.assertAlmostEqual(P.reversal(STEEP, TOP, 100_000, 1.0), 13 + 400, places=2)

    def test_blended_spread_never_falls_with_the_opening_share_through_rising_bands(self):
        values = [P.reversal(BANDED, P.LEVELS[0], 100_000, fraction) for fraction in P.OPEN_FRACTIONS]
        self.assertEqual(values, sorted(values))
        self.assertLess(values[0], values[-1])

    def test_rows_flag_only_ceiling_breaches(self):
        item = {"market": {"symbol": "T", "asset_class": "equity", "spread_tiers": BANDED["spread_tiers"]}}
        rows = P.reversal_rows([item], {"T": 20})
        for row in rows:
            self.assertEqual(row["exceeds_ceiling"], row["blended_reversal_bps"] > 20)
            self.assertEqual(row["pure_close_clamped"], row["pure_close_bps"] > 20)
        for flag in ("exceeds_ceiling", "pure_close_clamped"):
            self.assertTrue(any(row[flag] for row in rows))
            self.assertTrue(any(not row[flag] for row in rows))

    def test_rounding_rows_agree_with_engine_vectors(self):
        by_name = {vector["name"]: vector for vector in FIXTURES}
        for name, sign in (("precision-floor-buy", 1), ("precision-floor-sell", -1), ("raw-unit-sell", -1)):
            vector = by_name[name]
            mid = P.EXEC.raw(vector["mid"])
            row = P.rounding_rows(charge_bps=vector["minimum_bps"], mids=(mid,))[0]
            engine_adjustment = (vector["expected_price_raw"] - mid) * sign
            # The engine's sell stops a unit short of the index; the row is the rounded-up charge.
            self.assertEqual(min(row["rounded_up_raw"], mid - 1) if sign < 0 else row["rounded_up_raw"],
                             engine_adjustment, name)
        dust = P.rounding_rows(charge_bps=1, mids=(1,))[0]
        self.assertEqual((dust["rounded_up_raw"], dust["floor_bound_raw"], dust["floor_makes_close_free"]), (1, 0, True))

    def test_levels_come_from_the_pinned_parameters(self):
        params = {"stress_levels": [{"min_spread_bps": 0, "size_spread_multiplier_bps": 10_000},
                                    {"min_spread_bps": 3, "size_spread_multiplier_bps": 20_000},
                                    {"min_spread_bps": 7, "size_spread_multiplier_bps": 30_000}],
                  "toxicity": {"tiers": [{"extra_spread_bps": 1}, {"extra_spread_bps": 4}]}}
        levels, toxicity = P.levels_from(params)
        self.assertEqual((levels[2], toxicity), (("top", 7, 30_000), 4))
        with self.assertRaises(ValueError):
            P.levels_from(params | {"stress_levels": params["stress_levels"][:2]})
        item = {"market": {"symbol": "T", "asset_class": "equity", "spread_tiers": FLAT["spread_tiers"]}}
        rows = P.reversal_rows([item], {"T": 11}, levels, toxicity)
        self.assertEqual({row["pure_close_bps"] for row in rows if row["stress_level"] == "top"}, {11})

    def test_pure_close_is_never_more_than_one_atom_over_the_exact_bound(self):
        self.assertLessEqual(P.pure_close_excess_atoms(2_000), 1)
        self.assertEqual(P.pure_close_excess_atoms(2_000), P.pure_close_excess_atoms(2_000))


if __name__ == "__main__":
    unittest.main()
