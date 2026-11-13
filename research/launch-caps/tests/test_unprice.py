"""The unpriced-pair measure's launch rules: each fill's spread at its stress level, the level the
engine sets at the repricing, and the pair's payoff on hand-built paths."""

import numpy as np
import pytest

from caps import gaps, unprice
from caps.common import genesis

TERMS = unprice.stress_terms(genesis()["params"])
S1 = 2e-4  # BTC-USD's first band
FEE = 0.09  # tier 2's: the 10% base less its 10% discount


def test_a_stress_level_multiplies_the_first_band_and_adds_its_minimum():
    assert TERMS["move_bps"] == (75, 200) and TERMS["window_s"] == 60 and TERMS["cooldown_s"] == 300
    assert float(unprice.level_spread(0, S1, TERMS)) == pytest.approx(2e-4)
    assert float(unprice.level_spread(1, S1, TERMS)) == pytest.approx(2e-4 + 2 * 2e-4)
    assert float(unprice.level_spread(2, S1, TERMS)) == pytest.approx(10e-4 + 4 * 2e-4)


@pytest.mark.parametrize("move, unpriced_s, level0, since0_s, level", [
    (0.025, 3.0, 0, 1000.0, 2),  # a short spell: the window reads the 250 bps move, and the level rises at once
    (0.008, 30.0, 2, 400.0, 1),  # the move reads 80 bps; level 2 changed 400 s ago, past its cooldown, so it falls to 1
    (0.008, 30.0, 2, 100.0, 2),  # ... but 130 s after it changed, it holds
    (0.025, 120.0, 2, 50.0, 2),  # past the window the repricing reads no move; 170 s after the change the level holds
    (0.025, 300.0, 2, 0.0, 0),  # unpriced past the cooldown: the level falls to 0
])
def test_the_close_pays_the_level_the_engine_sets_at_the_repricing(move, unpriced_s, level0, since0_s, level):
    assert unprice.close_level(move, unpriced_s, level0, since0_s, TERMS) == level


def jump_path(rise: float, over_s: int, first: float = 0.0) -> np.ndarray:
    """A one-second log path, flat to 100 s, then up `first` at once and on to `rise` linearly over
    `over_s` seconds."""
    x = np.zeros(800)
    x[100:100 + over_s] = np.log1p(first) + (np.log1p(rise) - np.log1p(first)) * np.arange(0, over_s) / max(over_s - 1, 1)
    x[100 + over_s:] = np.log1p(rise)
    return x


def run(x, level0: int, since0: float, opens: str) -> dict:
    level, since = np.full(800, level0), np.full(800, since0)
    goal = unprice.MARGIN / (1 - FEE) + 0.003
    return unprice.causal_spells(x, 100, 1, 1.0, 300, 3, 5, unprice.TRIGGER, goal, FEE, S1, TERMS, level, since, opens)


def test_a_jump_past_the_balance_pays_the_pair_at_the_moves_level():
    # A 2.5% jump in one second: unpriced at once and priced again 3 s later, past the goal. Both
    # legs close at level 2; opened at the trigger's level 0, each fill on average pays half of each.
    out = run(jump_path(0.025, 1, first=0.025), 0, 1000.0, "trigger")
    s_close = float(unprice.level_spread(2, S1, TERMS))
    winner, _ = gaps.leg_results(0.025, S1, s_close, unprice.MARGIN, FEE, 0.0)
    loser, bad = gaps.leg_results(-0.025, S1, s_close, unprice.MARGIN, FEE, 0.0)
    assert (out["spells"], out["closing_levels"]) == (1, [0, 0, 1])
    assert out["payoff"] == pytest.approx(float(winner + loser), rel=1e-6)
    assert winner == pytest.approx((0.025 - S1 - s_close) * (1 - FEE))
    assert loser == pytest.approx(-(unprice.MARGIN + S1)) and bad > 0


def test_a_spell_past_the_window_and_the_cooldown_closes_at_level_0():
    # The venues jump 0.5% (the trigger), then the move takes about 80 s more to reach the goal: the
    # repricing reads no move, and a level 2 set 250 s before the spell has passed its cooldown, so
    # the closes pay the first band alone; set 50 s before, it holds. A standing pair opened at
    # level 0 pays less than one opened at level 2.
    x = jump_path(0.05, 400, first=0.005)
    fresh = run(x, 2, 250.0, "trigger")
    held = run(x, 2, 50.0, "trigger")
    standing = run(x, 2, 250.0, "standing")
    assert 60 < fresh["unpriced_s"] < 250
    assert fresh["closing_levels"] == [1, 0, 0] and held["closing_levels"] == [0, 0, 1]
    assert fresh["payoff"] > held["payoff"]
    assert standing["payoff"] > fresh["payoff"]


def test_a_spell_counts_its_move_from_no_earlier_than_the_last_repricing():
    # The venues jump 2.5% at 100 s and the rule fires at 100 s and 101 s. The first spell prices
    # again at once, past the goal; the second starts from that repricing, so its move is nothing
    # and its pair pays only its four fills' spreads, where counting from 3 s before it would have
    # paid the jump twice (`marks.rs`, `store_quotes`).
    x = jump_path(0.025, 1, first=0.025)
    level, since = np.zeros(800, dtype=int), np.full(800, 1000.0)
    goal = unprice.MARGIN / (1 - FEE) + 0.003
    one = unprice.causal_spells(x, 100, 1, 1.0, 300, 3, 5, unprice.TRIGGER, goal, FEE, S1, TERMS, level, since)
    two = unprice.causal_spells(x, 100, 2, 1.0, 300, 3, 5, unprice.TRIGGER, goal, FEE, S1, TERMS, level, since)
    assert two["spells"] == 2
    assert two["payoff"] == pytest.approx(one["payoff"] - 4 * S1, rel=1e-9)


def test_closes_at_the_moves_level_ignore_the_window():
    # A move that reaches the goal (about 1.4%) 80 s after the trigger: the engine reads no move at
    # the repricing and the level, set 250 s before, falls to 0; #734's convention closes at the
    # level the 1.4% move reads as, 1.
    x = jump_path(0.05, 400, first=0.005)
    goal = unprice.MARGIN / (1 - FEE) + 0.003
    level, since = np.full(800, 2), np.full(800, 250.0)
    engine = unprice.causal_spells(x, 100, 1, 1.0, 300, 3, 5, unprice.TRIGGER, goal, FEE, S1, TERMS, level, since, "trigger", "engine")
    move = unprice.causal_spells(x, 100, 1, 1.0, 300, 3, 5, unprice.TRIGGER, goal, FEE, S1, TERMS, level, since, "trigger", "move")
    assert engine["closing_levels"] == [1, 0, 0] and move["closing_levels"] == [0, 1, 0]
    assert engine["payoff"] > move["payoff"]
