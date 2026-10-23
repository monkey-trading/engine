"""Hand calculations distinguish close charges from market moves and opening impact."""

import importlib.util
import json
import unittest
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("execution", Path(__file__).resolve().parents[1] / "execution.py")
EXEC = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXEC)
raw = EXEC.raw


class ExecutionTests(unittest.TestCase):
    def test_units_refuse_nonfinite_and_sub_atom_inputs(self):
        self.assertEqual(raw("0.000000001"), 1)
        for value in ("NaN", "Infinity", "0.0000000001"):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "nine decimal"):
                raw(value)

    def price(self, **changes):
        arguments = dict(mid=raw("100"), current=raw("10"), quantity=raw("5"), buy=False,
                         tiers=[(None, 100)], minimum_bps=10, toxicity_bps=3)
        return EXEC.adjustment(**(arguments | changes))

    def test_pure_close_has_no_size_spread_even_with_expensive_tiers(self):
        self.assertEqual(self.price()["price_raw"], raw("99.87"))
        self.assertEqual(self.price()["reported_spread_bps"], 13)

    def test_liquidation_waives_toxicity(self):
        self.assertEqual(self.price(liquidation=True)["price_raw"], raw("99.9"))

    def test_reversal_applies_size_only_to_new_side_then_divides_by_entire_fill(self):
        # Sell 15 from long 10: five new units * $100 * 1% / 15, rounded upwards.
        result = self.price(quantity=raw("15"), minimum_bps=0, toxicity_bps=0)
        self.assertEqual(result["adjustment_raw"], 333_333_334)

    def test_reduce_only_clamps_quantity_before_size_calculation(self):
        result = self.price(quantity=raw("15"), reduce_only=True)
        self.assertEqual(result["quantity_raw"], raw("10"))
        self.assertEqual(result["adjustment_raw"], raw("0.13"))
        with self.assertRaisesRegex(ValueError, "does not reduce"):
            self.price(reduce_only=True, buy=True)

    def test_sub_basis_point_price_rounding_is_not_visible_in_integer_bps(self):
        result = self.price(mid=333, minimum_bps=1, toxicity_bps=0)
        self.assertEqual(result["adjustment_raw"], 1)
        self.assertEqual(result["reported_spread_bps"], 30)
        self.assertGreater(result["adjustment_raw"] * 10_000, 333 * result["reported_spread_bps"])

    def test_precision_floor_sell_and_buy_and_unfillable_charge(self):
        self.assertEqual(self.price(mid=1, minimum_bps=1, toxicity_bps=0)["price_raw"], 1)
        self.assertEqual(self.price(mid=1, current=-raw("10"), buy=True,
                                   minimum_bps=1, toxicity_bps=0)["price_raw"], 2)
        with self.assertRaisesRegex(ValueError, "reaches the price"):
            self.price(minimum_bps=10_000)

    def test_eur_and_jpy_conversion_direction_and_rounded_inverse(self):
        self.assertEqual(EXEC.convert(raw("100"), "EUR", raw("1.25"), True, True), raw("125"))
        self.assertEqual(EXEC.convert(raw("100"), "JPY", raw("125"), True, True), raw("0.8"))
        self.assertEqual(EXEC.convert(1, "EUR", raw("3"), False, True), 1)
        with self.assertRaisesRegex(ValueError, "positive FX"):
            EXEC.convert(raw("100"), "JPY", None, True, True)

    def test_staircase_rounds_each_band_before_summing(self):
        self.assertEqual(EXEC.staircase([(1, 1), (None, 1)], 0, 2, True), 2)
        self.assertEqual(EXEC.staircase([(1, 1), (None, 1)], 0, 2, False), 0)

    def test_model_matches_shared_engine_apply_vectors(self):
        path = Path(__file__).resolve().parents[1] / "fixtures/engine-v17.json"
        for vector in json.loads(path.read_text()):
            with self.subTest(name=vector["name"]):
                arguments = {key: value for key, value in vector.items()
                             if key not in ("name", "opening_mid", "deposit") and not key.startswith("expected_")}
                for key in ("mid", "current", "quantity"):
                    arguments[key] = raw(arguments[key])
                arguments["fx"] = None if arguments["fx"] is None else raw(arguments["fx"])
                arguments["tiers"] = [(None if high is None else raw(high), bps)
                                      for high, bps in arguments["tiers"]]
                result = EXEC.adjustment(**arguments)
                self.assertEqual(result["price_raw"], vector["expected_price_raw"])
                self.assertEqual(result["quantity_raw"], vector["expected_quantity_raw"])
                self.assertEqual(result["reported_spread_bps"], vector["expected_spread_bps"])


if __name__ == "__main__":
    unittest.main()
