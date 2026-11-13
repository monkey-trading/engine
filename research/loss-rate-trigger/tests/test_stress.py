import numpy as np
import pytest

import run
from trigger import pairs

STEP = 0.05
LEAD = int(180 / STEP)


def one_jump_hour(jump=0.025, at_s=100.0):
    """A recorded hour, on BTC-USD's grid, still but for one jump: the market's first tier at 2 bps
    and no fast-move level, starting at the timeline's first bin."""
    x = np.zeros(LEAD + int(3720 / STEP))
    x[LEAD + int(at_s / STEP) :] = np.log1p(jump)
    return {"market": "BTC-USD", "x": x, "step_s": STEP, "lead": LEAD, "per_s": 20, "s1": 2e-4, "bin0": 0,
            "levels": np.zeros(len(x) // 20, dtype=np.int64), "fired": pairs.fired_steps(x, LEAD, LEAD + int(3600 / STEP), STEP)}


def background(bins=720):
    return {f"house/{m}": np.zeros(bins) for m in ("marked", "bad_debt", "realised")}


def test_a_spell_counts_its_move_from_the_last_repricing_not_from_before_it():
    h = one_jump_hour()
    fresh = run.stress_run([h], background(), "marked", "house", 6, None, True, 0)
    as_697 = run.stress_run([h], background(), "marked", "house", 6, None, True, 0, fresh=False)
    first = (1 - pairs.FEE) * (0.025 - pairs.spread(2, 2e-4)) - pairs.MARGIN - 2 * 2e-4
    # The jump pays once; the next spell, from the repriced index, finds no move and loses its costs.
    assert fresh["spells"] == 2 and fresh["largest_spell"] == pytest.approx(first * pairs.NOTIONAL)
    assert fresh["house_loss"] < first * pairs.NOTIONAL
    # Counted from 3 s before each start, every spell within 3 s of the jump takes it again.
    assert as_697["spells"] > 50 and as_697["house_loss"] > 30 * fresh["house_loss"]


def test_bad_debt_raises_the_level_at_once_and_the_operator_waits_out_the_window():
    h = one_jump_hour()
    got = run.stress_run([h], background(), "bad_debt", "house", 360, 1.0, False, 0)
    assert got["spells"] == 1 and got["spells_raised"] == 0
    # The level holds while the jump's bad debt is in the 60-minute window, then falls.
    assert got["raised_minutes"] == pytest.approx(60, abs=0.2)


def test_a_raised_level_makes_a_spell_pay_level_two_and_target_its_breakeven():
    h = one_jump_hour(jump=0.025)
    got = run.stress_run([h], background(), "bad_debt", "house", 360, 1.0, True, 0)
    assert got["spells_raised"] >= 1
    assert got["house_loss"] < run.stress_run([h], background(), "bad_debt", "house", 360, None, True, 0)["house_loss"]
