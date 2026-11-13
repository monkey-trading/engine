"""Ceiling arithmetic, blocking, and the leak-free split."""

import importlib.util
import math
import unittest
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parents[1]


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


C = load("ceilings")
PARAMS = {"stress_levels": [{"min_spread_bps": 0, "size_spread_multiplier_bps": 10_000},
                           {"min_spread_bps": 2, "size_spread_multiplier_bps": 20_000},
                           {"min_spread_bps": 10, "size_spread_multiplier_bps": 40_000}],
          "toxicity": {"tiers": [{"extra_spread_bps": 1}, {"extra_spread_bps": 3}]}}


def item(p99=100.0, upper=110.0, validation_p99=90.0, hours=2_000, divergence=10, age=1_000, margin=50,
         symbol="X", asset_class="fx", first_tenth_bps=0):
    interval = None if upper is None else {"p975_bps": upper}
    return {"market": {"id": 1, "symbol": symbol, "asset_class": asset_class,
                       "price_rules": {"max_age_ms": age, "max_divergence_bps": divergence},
                       "margin_tiers": [{"bps": margin}],
                       "spread_tiers": [{"up_to": "1000000", "tenth_bps": first_tenth_bps},
                                        {"tenth_bps": 2 * first_tenth_bps}]},
            "calibration": {"active_hours": hours, "abs_hourly_p99_bps": p99,
                            "hourly_p99_week_block_bootstrap": interval},
            "validation": {"active_hours": 500, "abs_hourly_p99_bps": validation_p99, "abs_hourly_max_bps": 300.0}}


class CeilingTests(unittest.TestCase):
    def test_launch_charge_is_the_top_stress_minimum_plus_top_toxicity(self):
        self.assertEqual(C.launch_close_charge_bps(PARAMS), 13)
        self.assertEqual(C.launch_close_charge_bps(
            {"stress_levels": [{"min_spread_bps": 4}], "toxicity": {"tiers": []}}), 4)
        # From V54 the top add-on is the score's top tier, and the earlier tiers are gone.
        scored = {"stress_levels": [{"min_spread_bps": 10}], "toxicity": {"score_tiers": [{"extra_spread_bps": 5}]}}
        self.assertEqual(C.launch_close_charge_bps(scored), 15)

    def test_floor_counts_the_first_band_at_the_widest_multiplier_in_tenths(self):
        # 13 bps of stress minimum and add-on, plus 0.7 bp at 4x, 2.8 bps: 15.8 bps, which a 16-bp ceiling covers.
        market = {"spread_tiers": [{"up_to": "5000000", "tenth_bps": 7}, {"tenth_bps": 14}]}
        self.assertEqual(C.launch_close_charge_tenth_bps(PARAMS, market), 158)
        self.assertEqual((C.floor_bps(158), C.floor_bps(150), C.floor_bps(151)), (16, 15, 16))
        # A band written in whole bps charges ten tenths a bp.
        self.assertEqual(C.launch_close_charge_tenth_bps(PARAMS, {"spread_tiers": [{"bps": 2}]}), 210)
        # The first band at the multiplier rounds up to a whole tenth: 0.3 bp at 1.5x is 0.45 bp, counted 0.5.
        widest = {"stress_levels": [{"min_spread_bps": 10, "size_spread_multiplier_bps": 15_000}],
                  "toxicity": {"tiers": []}}
        self.assertEqual(C.launch_close_charge_tenth_bps(widest, {"spread_tiers": [{"tenth_bps": 3}]}), 105)

    def test_each_market_takes_its_own_floor(self):
        row = C.propose(item(upper=20.0, validation_p99=20.0, first_tenth_bps=7), PARAMS)
        self.assertEqual((row["launch_close_charge_tenth_bps"], row["floor_bps"]), (158, 16))
        self.assertEqual((row["proposed_ceiling_bps"], row["ceiling_binding_constraint"]), (16, "launch charge"))
        self.assertGreaterEqual(row["proposed_ceiling_bps"] * 10, row["launch_close_charge_tenth_bps"])
        wide = C.propose(item(upper=20.0, validation_p99=20.0, first_tenth_bps=30), PARAMS)
        self.assertEqual((wide["floor_bps"], wide["proposed_ceiling_bps"]), (25, 25))
        # A need above the floor rounds up to the step, not to the floor.
        above = C.propose(item(upper=600.0, validation_p99=100.0, first_tenth_bps=7), PARAMS)
        self.assertEqual((above["proposed_ceiling_bps"], above["ceiling_binding_constraint"]),
                         (25, "reference-risk need"))

    def test_horizon_scale_is_square_root_of_time(self):
        self.assertAlmostEqual(C.horizon_scale(3_600_000), 1)
        self.assertAlmostEqual(C.horizon_scale(900_000), .5)
        with self.assertRaises(ValueError):
            C.horizon_scale(0)

    def test_round_up_uses_step_and_never_goes_below_the_floor(self):
        self.assertEqual(C.round_up(5.7, 13), 13)
        self.assertEqual(C.round_up(13.0, 13), 13)
        self.assertEqual(C.round_up(13.01, 13), 15)
        self.assertEqual(C.round_up(30.0, 13), 30)
        self.assertEqual(C.round_up(30.2, 13), 35)

    def test_need_adds_divergence_to_the_scaled_hourly_tail(self):
        self.assertAlmostEqual(C.need_bps(10, 360, 1_000), 11 + 360 / 60)

    def test_proposal_traces_to_its_inputs(self):
        row = C.propose(item(upper=600.0, validation_p99=100.0), PARAMS)
        self.assertAlmostEqual(row["need_bps"], 21.0)
        self.assertEqual((row["calibration_only_ceiling_bps"], row["proposed_ceiling_bps"]), (25, 25))
        self.assertEqual(row["ceiling_binding_constraint"], "reference-risk need")
        self.assertAlmostEqual(row["ceiling_percent_of_min_margin"], 50.0)
        self.assertFalse(row["within_burden_limit"])

    def test_calm_market_takes_the_launch_charge(self):
        row = C.propose(item(upper=20.0, validation_p99=20.0), PARAMS)
        self.assertEqual(row["proposed_ceiling_bps"], 13)
        self.assertEqual(row["ceiling_binding_constraint"], "launch charge")

    def test_floor_bound_ceiling_can_fail_validation(self):
        # A 240-bp validation hourly p99 is 4 bps over one second at a 1s quote age; with a 10-bp divergence
        # limit (11 counted, whole bps truncating) the validation need is 15 bps, above the 13-bp floor the
        # calibration ceiling sits at.
        row = C.propose(item(upper=20.0, validation_p99=240.0), PARAMS)
        self.assertEqual(row["calibration_only_ceiling_bps"], 13)
        self.assertAlmostEqual(row["validation_need_bps"], 15.0)
        self.assertFalse(row["validation_covered_by_calibration_ceiling"])
        self.assertEqual(row["proposed_ceiling_bps"], 15)

    def test_validation_tail_never_lowers_and_may_raise_the_proposal(self):
        low = C.propose(item(upper=600.0, validation_p99=0.0), PARAMS)
        high = C.propose(item(upper=600.0, validation_p99=1_200.0), PARAMS)
        self.assertEqual(low["proposed_ceiling_bps"], low["calibration_only_ceiling_bps"])
        self.assertGreater(high["proposed_ceiling_bps"], high["calibration_only_ceiling_bps"])
        self.assertFalse(high["validation_covered_by_calibration_ceiling"])

    def test_calibration_ceiling_does_not_depend_on_validation(self):
        first = C.propose(item(validation_p99=10.0), PARAMS)["calibration_only_ceiling_bps"]
        second = C.propose(item(validation_p99=5_000.0), PARAMS)["calibration_only_ceiling_bps"]
        self.assertEqual(first, second)

    def test_missing_interval_or_thin_sample_is_blocked_not_defaulted(self):
        for blocked in (item(upper=None), item(hours=C.MIN_CALIBRATION_HOURS - 1)):
            row = C.propose(blocked, PARAMS)
            self.assertEqual((row["status"], row["proposed_ceiling_bps"]), ("blocked_pending_evidence", None))

    def test_proxy_instruments_are_low_confidence(self):
        row = C.propose(item(symbol="DE40", asset_class="index"), PARAMS)
        self.assertEqual((row["status"], row["confidence"]), ("proposed_low_confidence", "low"))
        self.assertTrue(any(flag.startswith("proxy") for flag in row["evidence_flags"]))

    def test_exceedance_counts_active_hours_only(self):
        closed = np.iinfo(np.int32).min
        rows = np.array([[0, 0, 0, 0], [0, 0, 0, 600_000], [closed] * 4], dtype=np.int32)
        # An hourly move of 6% (618 bps) scales to 10.3 bps over one second; the divergence limit of 10
        # admits sources just under 11 bps apart, so the need counts 11.
        self.assertEqual(C.exceedance(rows, 1e7, 10, 1_000, 15), .5)
        self.assertEqual(C.exceedance(rows, 1e7, 10, 1_000, 21), .5)
        self.assertEqual(C.exceedance(rows, 1e7, 10, 1_000, 22), 0)
        self.assertIsNone(C.exceedance(np.array([[closed] * 4], dtype=np.int32), 1e7, 10, 1_000, 1))
        self.assertTrue(math.isfinite(C.exceedance(rows, 1e7, 10, 1_000, 0)))


if __name__ == "__main__":
    unittest.main()
