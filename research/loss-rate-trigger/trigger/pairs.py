"""Paired positions across unpriced spells (#549), under a loss-rate trigger.

#549's causal rule (`research/rogue-operator/model/unprice.py`): an operator holding a pair, long in
one account and short the same notional in another, unprices the market once the venues have moved
`TRIGGER` in the last 5 s, counting from the engine's index `LAG_S` earlier, and prices it again as
soon as the move pays its target or after `SPELL_S`. The losing leg is then closed out past its
balance, and the loss beyond the balance is bad debt; the winning leg closes for its gain less the
fee. Per unit of notional a side, with margin `m`, fee `f`, each fill's spread `s` and the opening
fills' cost `c`:

    payoff = (1 - f) * (J - s) - min(J + s, m) - c

#697 ran it at the calibrated first tiers, each fill paying its market's first tier `s1` (the pair
split across accounts within each one's first band), tier 3's 8% fee and $50M a side, opening a
pair before each spell at the level then. Here every fill pays the engine's spread at its stress
level, `min_spread + multiplier * s1` (`execution/mod.rs`), where #697 added the level's minimum
alone; the level is the fast-move trigger's (`stress.rs`, at genesis), or level 2 while the loss
trigger is raised. The operator may open its pairs ahead, at level 0, so a crash's raised levels
reach only the closes. At a raised level it re-derives the rule's target from that level's costs,
so a spell still runs when the venues move far enough.
"""

from dataclasses import dataclass

import numpy as np

TRIGGER = 0.004  # 40 bps in 5 s
TRIGGER_WINDOW_S = 5.0
SPELL_S = 300.0
LAG_S = 3.0
TARGET_OVER_BREAKEVEN = 0.003
MARGIN = 0.01  # BTC-USD and ETH-USD at 1% (#614, D4)
FEE = 0.08  # tier 3's fee past the curve, the least a pair's winning leg pays (#697)
NOTIONAL = 50e6  # a side (#549)
# Genesis stress levels: (minimum spread, size multiplier); fast-move triggers in bps over the window.
LEVELS = ((0.0, 1.0), (2e-4, 2.0), (10e-4, 4.0))
MOVE_BPS = (75, 200)
MOVE_WINDOW_S = 63.0
COOLDOWN_S = 300.0


def spread(level: int, s1: float) -> float:
    lo, mult = LEVELS[level]
    return lo + mult * s1


def move_levels(x: np.ndarray, step_s: float) -> np.ndarray:
    """The fast-move trigger's level each second, as `research/rogue-operator/model/stress.py`."""
    every = int(round(1.0 / step_s))
    p = x[::every]
    w = int(MOVE_WINDOW_S)
    pad = np.concatenate([np.full(w, p[0]), p])
    win = np.lib.stride_tricks.sliding_window_view(pad, w + 1)[: len(p)]
    reading = np.maximum(np.expm1(p - win.min(axis=1)), -np.expm1(p - win.max(axis=1))) * 1e4
    target = np.zeros(len(p), dtype=np.int64)
    for k, threshold in enumerate(MOVE_BPS, start=1):
        target[reading >= threshold] = k
    out = np.empty(len(p), dtype=np.int64)
    level, since = 0, -np.inf
    for i, t in enumerate(target):
        if t > level or (t < level and i - since >= COOLDOWN_S):
            level, since = int(t), i
        out[i] = level
    return out


@dataclass
class Spell:
    start: int  # grid step it unprices at
    stop: int  # grid step it prices again at
    move: float
    payoff: float  # per unit of notional a side
    bad_debt: float  # per unit of notional a side
    level: int  # the level its fills paid


def breakeven(open_level: int, close_level: int, s1: float, fee: float = FEE, margin: float | None = None) -> float:
    """The move a spell needs to pay when its pair opened at `open_level`'s spreads and closes at
    `close_level`'s, at `margin` (the module's `MARGIN` when not given)."""
    margin = MARGIN if margin is None else margin
    s = spread(close_level, s1)
    return (margin + 2 * spread(open_level, s1)) / (1 - fee) + s


def run_spell(x: np.ndarray, i: int, step_s: float, s1: float, open_level: int, forced: int | None, priced_at: int | None = None,
              fee: float = FEE, margin: float | None = None) -> Spell:
    """One spell from step `i`: the pair opens at `open_level`'s spreads, and the operator prices
    again at its target or after `SPELL_S`. `forced` is the loss trigger's level, which the closes
    pay at least; without it they pay the level the move itself reads as (#697). The move counts
    from the engine's index `LAG_S` earlier, as #549's rule has it, or, given `priced_at`, the step
    the market last priced again, from no earlier than then: a spell that starts within `LAG_S` of
    the last one's end cannot count a move the index already showed."""
    margin = MARGIN if margin is None else margin
    lag = int(round(LAG_S / step_s))
    base = x[i - lag if priced_at is None else max(i - lag, priced_at)]
    path = np.abs(np.expm1(x[i : i + int(round(SPELL_S / step_s)) + 1] - base))
    if forced is None:
        target = margin / (1 - fee) + TARGET_OVER_BREAKEVEN
    else:
        target = breakeven(open_level, forced, s1, fee, margin) + TARGET_OVER_BREAKEVEN
    hit = np.flatnonzero(path >= target)
    stop = int(hit[0]) if len(hit) else len(path) - 1
    move = float(path[stop])
    read = 2 if move * 1e4 >= MOVE_BPS[1] else 1 if move * 1e4 >= MOVE_BPS[0] else 0
    close_level = max(read, forced or 0)
    s_open, s_close = spread(open_level, s1), spread(close_level, s1)
    payoff = (1 - fee) * (move - s_close) - min(move + s_close, margin) - 2 * s_open
    bad = max(move + s_close + s_open - margin, 0.0)
    return Spell(i, i + stop, move, payoff, bad, max(open_level, close_level))


def fired_steps(x: np.ndarray, start: int, stop: int, step_s: float) -> np.ndarray:
    w = int(round(TRIGGER_WINDOW_S / step_s))
    return np.flatnonzero(np.abs(np.expm1(x[start:stop] - x[start - w : stop - w])) >= TRIGGER) + start
