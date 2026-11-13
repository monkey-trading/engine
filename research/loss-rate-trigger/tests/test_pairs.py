import numpy as np
import pytest

from trigger import pairs

STEP = 0.05


def test_each_level_charges_its_minimum_and_multiplies_the_first_tier():
    assert pairs.spread(0, 2e-4) == pytest.approx(2e-4)
    assert pairs.spread(1, 2e-4) == pytest.approx(2e-4 + 4e-4)
    assert pairs.spread(2, 2e-4) == pytest.approx(10e-4 + 8e-4)


def path_moving_to(move: float, at_s: float, length_s: float = 400.0) -> np.ndarray:
    x = np.zeros(int(length_s / STEP))
    x[int(at_s / STEP) :] = np.log1p(move)
    return x


def test_a_spell_prices_again_at_its_target_and_pays_the_formula():
    s1 = 2e-4
    x = path_moving_to(0.025, 100.0)
    i = int(10.0 / STEP)
    sp = pairs.run_spell(x, i, STEP, s1, open_level=0, forced=None)
    assert sp.stop == int(100.0 / STEP)
    assert sp.move == pytest.approx(0.025)
    s_close = pairs.spread(2, s1)  # a 250 bps move reads as level 2 when the market prices again
    want = (1 - pairs.FEE) * (0.025 - s_close) - pairs.MARGIN - 2 * s1
    assert sp.payoff == pytest.approx(want)
    assert sp.bad_debt == pytest.approx(0.025 + s_close + s1 - pairs.MARGIN)


def test_at_level_two_the_operator_needs_the_breakeven_move_and_the_spell_pays_nothing_below_it():
    s1 = 2e-4
    need = pairs.breakeven(0, 2, s1)
    exact = (1 - pairs.FEE) * (need - pairs.spread(2, s1)) - pairs.MARGIN - 2 * pairs.spread(0, s1)
    assert exact == pytest.approx(0.0, abs=1e-12)
    # A move just short of level 2's breakeven never reaches the target: the spell runs its length and loses.
    x = path_moving_to(need - 0.001, 50.0)
    sp = pairs.run_spell(x, int(10.0 / STEP), STEP, s1, open_level=0, forced=2)
    assert sp.stop == len(x) - 1 or sp.stop == int(10.0 / STEP) + int(pairs.SPELL_S / STEP)
    assert sp.payoff < 0


def test_the_fast_move_trigger_rises_on_a_move_and_cools_down():
    x = np.zeros(int(1000 / STEP))
    x[int(100 / STEP) :] = np.log1p(0.01)  # 100 bps: level 1
    lv = pairs.move_levels(x, STEP)
    assert lv[99] == 0 and lv[100] == 1
    assert lv[100 + 299] == 1 and lv[100 + 300] == 0


def test_fired_steps_are_moves_of_forty_bps_in_five_seconds():
    x = np.zeros(int(100 / STEP))
    x[int(50 / STEP) :] = np.log1p(0.005)
    fired = pairs.fired_steps(x, int(10 / STEP), len(x), STEP)
    assert fired[0] == int(50 / STEP) and fired[-1] == int(55 / STEP) - 1
