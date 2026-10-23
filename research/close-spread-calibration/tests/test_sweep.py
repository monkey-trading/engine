"""Conservation and margin-cost checks for explicitly conditional gap scenarios."""

import importlib.util
import unittest
from decimal import Decimal as D
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("sweep", Path(__file__).resolve().parents[1] / "sweep.py")
SWEEP = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SWEEP)


class SweepTests(unittest.TestCase):
    def scenario(self, **changes):
        args = dict(notional=D(10_000), margin_bps=100, move=D("0.05"), cap_bps=0,
                    minimum_bps=13, toxicity_bps=0, closeout_level_bps=5000, profit_fee_bps=0, capital=D(0))
        return SWEEP.paired_gap(**(args | changes))

    def test_opposite_positions_can_create_unfunded_house_loss_after_gap(self):
        result = self.scenario()
        self.assertEqual(result["bad_debt_usd"], D(400))
        self.assertEqual(result["house_cash_shortfall_usd"], D(400))

    def test_cap_reduces_exit_cost_and_collectable_shortfall_offset(self):
        result = self.scenario(cap_bps=10)
        self.assertEqual(result["largest_account_cost_usd"], D("10.5"))
        self.assertEqual(result["bad_debt_usd"], D("410.5"))
        self.assertEqual(result["house_cash_shortfall_usd"], D("389.5"))
        self.assertEqual(result["largest_cost_percent_initial_margin"], D("10.5"))

    def test_gap_liquidation_waives_toxicity_only_on_losing_leg(self):
        result = self.scenario(cap_bps=100, minimum_bps=10, toxicity_bps=3)
        self.assertEqual(result["liquidations"], D(1))
        self.assertEqual(result["total_close_cost_usd"], D("24.15"))
        self.assertEqual(result["largest_account_cost_usd"], D("13.65"))

    def test_capital_offsets_shortfall_and_fee_applies_only_to_positive_realised_profit(self):
        result = self.scenario(capital=D(500), profit_fee_bps=1000)
        self.assertEqual(result["profit_fees_usd"], D(50))
        self.assertEqual(result["house_cash_after_usd"], D(150))
        self.assertEqual(result["house_cash_shortfall_usd"], D(0))

    def test_cap_above_requested_charge_has_no_effect_and_negative_prices_are_refused(self):
        self.assertEqual(self.scenario(cap_bps=13), self.scenario(cap_bps=100))
        with self.assertRaisesRegex(ValueError, "invalid"):
            self.scenario(move=D(-1))

    def test_margin_is_progressive_instead_of_last_band_rate_on_whole_position(self):
        tiers = [{"up_to": "5000", "bps": 100}, {"bps": 200}]
        self.assertEqual(SWEEP.margin_cost(D(10_000), tiers), D(150))
        self.assertEqual(self.scenario(margin_tiers=tiers)["initial_margin_per_account_usd"], D(150))
