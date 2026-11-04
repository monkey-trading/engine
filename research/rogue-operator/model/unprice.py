"""Unpricing a market across a move, so the operator's paired positions skip their close-out.

A production market needs both its venues fresh, so a host that holds one venue's stream past
`max_delay` leaves the market with no price: nothing fills, and no close-out runs, until the host
lets the stream through. Nothing bounds how long that lasts. An operator holding a pair (long in
one account, short the same notional in another, which leaves the house's net exposure, and so
its caps, untouched) unprices the market as a fast move starts and prices it again once the move
has run. The losing leg is then closed out at the new index, past its balance, and the loss beyond
the balance is the house's (negative balance protection); the winning leg closes for its gain less
the profit fee. Per unit of notional a side, with margin rate `m` (1% for BTC-USD and ETH-USD
below $2M, 10% for SOL-USD) and the move `J` at the repricing:

    pair = 0.9 * (J - s) - min(J + s, m)

where `s` is the stress level's minimum spread both closes pay once the move reads as fast
(2 bps at a 75 bps reading, 10 bps at 200). It pays once `J` passes about `m / 0.9`: 1.1% at
100x. Opening the pair pays the stress minimum on both legs at the market's level then
(`model/stress.py`), which in a crash makes each new pair cost. The same holds for any farmer whose market goes unpriced across a move, as a quiet venue can leave
it; the operator chooses when.

Measures per hour, per unit of notional a side, for an unpriced spell of at most `spell` seconds:
- `best`: the hour's single best spell, its start and length chosen in hindsight;
- `causal`: the operator unprices once the venues have moved `trigger` in the last 5 s, counting
  the move from the engine's last index `lag` earlier, and prices again as soon as the move pays
  `TARGET` or at the spell's end, whatever it then is.
"""

import numpy as np

from model.paths import window_max, window_min

PROFIT_FEE = 0.10
TARGET_OVER_BREAKEVEN = 0.003
TRIGGER_WINDOW_S = 5.0


def spread_at(move: np.ndarray) -> np.ndarray:
    move_bps = move * 1e4
    return np.where(move_bps >= 200, 10e-4, np.where(move_bps >= 75, 2e-4, 0.0))


def pair_payoff(move: np.ndarray, margin: float) -> np.ndarray:
    move = np.abs(np.asarray(move, dtype=float))
    s = spread_at(move)
    return (1 - PROFIT_FEE) * (move - s) - np.minimum(move + s, margin)


def best_spell(x: np.ndarray, start: int, length: int, spell: int, margin: float, open_cost: np.ndarray | None = None) -> tuple[float, int]:
    """The hour's best single spell's payoff and its start, in hindsight, less `open_cost[i]`, what
    opening the pair just before a spell at `i` pays."""
    # Spells must end inside the grid, so the last starts of the hour fall away for a long spell.
    hi = window_max(x, spell)[start : start + length]
    lo = window_min(x, spell)[start : start + length]
    base = x[start : start + len(hi)]
    move = np.maximum(np.expm1(hi - base), -np.expm1(lo - base))
    pay = pair_payoff(move, margin)
    if open_cost is not None:
        pay = pay - open_cost[start : start + len(hi)]
    i = int(np.argmax(pay))
    return float(max(pay[i], 0.0)), start + i


def causal_spells(x: np.ndarray, start: int, length: int, step_s: float, spell: int, lag: int, margin: float, trigger: float,
                  open_cost: np.ndarray | None = None) -> dict:
    """Every spell the trigger rule starts in the hour, one after another; each opens a new pair
    first, paying `open_cost` at that moment, since the last spell's losing leg was closed out."""
    w = int(round(TRIGGER_WINDOW_S / step_s))
    target = margin / (1 - PROFIT_FEE) + TARGET_OVER_BREAKEVEN
    end = start + length
    fired = np.flatnonzero(np.abs(np.expm1(x[start:end] - x[start - w : end - w])) >= trigger) + start
    payoffs, seconds = [], 0.0
    k = 0
    while k < len(fired):
        i = int(fired[k])
        base = x[i - lag]
        path = np.abs(np.expm1(x[i : i + spell + 1] - base))
        hit = np.flatnonzero(path >= target)
        stop = int(hit[0]) if len(hit) else len(path) - 1
        cost = 0.0 if open_cost is None else float(open_cost[i])
        payoffs.append(float(pair_payoff(path[stop], margin)) - cost)
        seconds += (stop + lag) * step_s
        k = int(np.searchsorted(fired, i + stop + 1))
    return {"payoff": float(np.sum(payoffs)), "spells": len(payoffs), "unpriced_s": seconds,
            "paying": int(np.sum(np.array(payoffs) > 0)) if payoffs else 0}
