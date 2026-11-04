import numpy as np
import pytest

from model import hold, stress, unprice


def test_hold_kills_a_losing_trade_and_commits_a_winning_one():
    # Lag 2, hold 3. From step 4: the venues are at 1 against an engine index of 0, so it buys at 0.
    x = np.array([0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 2.0, 0.0, 0.0, 0.0, 0.0])
    out = hold.option(x, np.array([4]), lag=2, hold=3)
    # Committed at step 7, it can close at the best of 5..7, which is 2; a winner needs no kill.
    assert out["held"][0] == pytest.approx(0.9 * 2.0)
    assert out["without"][0] == pytest.approx(0.9 * 2.0)
    assert not out["kill"][0]
    # Closing at once instead takes the best of 2..4, which is 1.
    assert out["now"][0] == pytest.approx(0.9 * 1.0)
    falling = np.array([0.0, 0.0, 0.0, 0.0, 1.0, -1.0, -2.0, -3.0, -3.0, 0.0, 0.0, 0.0])
    out = hold.option(falling, np.array([4]), lag=2, hold=3)
    # The best of 5..7 is -1: the kill avoids that loss, which the trade without a kill keeps.
    assert out["kill"][0] and out["held"][0] == 0.0
    assert out["without"][0] == pytest.approx(-1.0)


def test_a_hold_within_the_lag_never_kills():
    rng = np.random.default_rng(2)
    x = np.cumsum(rng.normal(0, 1, 5000))
    out = hold.option(x, np.arange(100, 4800, 7), lag=60, hold=40)
    assert not out["kill"].any()


def test_pair_pays_only_past_the_margin_over_the_fee():
    m = 0.01
    # Below the close-out (0.5%): the loser closes by hand; the pair loses the fee on the winner.
    assert unprice.pair_payoff(np.array([0.004]), m)[0] == pytest.approx(0.9 * 0.004 - 0.004)
    # Past the margin the loser loses only its balance; 1.5% with the 2 bps spread a 75 bps reading
    # brings: 0.9 * (0.015 - 0.0002) - 0.01.
    assert unprice.pair_payoff(np.array([0.015]), m)[0] == pytest.approx(0.9 * 0.0148 - 0.01)
    # 2% reads 200 bps, so both closes pay level 2's 10 bps.
    assert unprice.pair_payoff(np.array([0.02]), m)[0] == pytest.approx(0.9 * 0.019 - 0.01)
    # Break-even sits just above m / 0.9 = 1.11%, the spread moving it a little.
    grid = np.linspace(0.005, 0.02, 15001)
    first = grid[np.argmax(unprice.pair_payoff(grid, m) > 0)]
    assert 0.0111 < first < 0.0114


def test_best_spell_finds_the_largest_move_inside_the_spell():
    x = np.zeros(400)
    x[200:220] = np.log(1.03)  # a 3% jump held for a second
    pay, start = unprice.best_spell(x, 100, 200, spell=40, margin=0.01)
    assert pay == pytest.approx(0.9 * (0.03 - 0.001) - 0.01, rel=1e-6)
    assert 160 <= start <= 200


def test_causal_spell_reprices_at_the_target():
    x = np.zeros(2000)
    x[1000:] = np.log(1.0 + np.linspace(0, 0.015, 1000))  # 1.5% over 50 s at 50 ms steps
    out = unprice.causal_spells(x, 900, 1000, 0.05, spell=1200, lag=60, margin=0.01, trigger=0.0005)
    # One spell reprices at the target; what is left of the ramp cannot pay another.
    assert out["paying"] == 1
    target = 0.01 / 0.9 + unprice.TARGET_OVER_BREAKEVEN
    assert out["payoff"] == pytest.approx(unprice.pair_payoff(np.array([target]), 0.01)[0], rel=0.05)


def test_stress_rises_at_once_and_falls_only_after_the_cooldown():
    step = 1.0
    x = np.zeros(1000)
    x[100:] = np.log(1.01)  # a 100 bps step: level 1 (75), not level 2 (200)
    lv = stress.levels(x, step)
    assert lv[99] == 0 and lv[100] == 1
    # The reading falls once the window has passed the step (63 s), but the level holds until 300 s.
    assert lv[399] == 1 and lv[400] == 0
    x[500:] = np.log(1.04)
    assert stress.levels(x, step)[500] == 2
    assert stress.min_spread(x, step)[500] == pytest.approx(10e-4)
