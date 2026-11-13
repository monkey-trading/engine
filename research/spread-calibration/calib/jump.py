"""A jump trigger: the stress level a sub-second move raises, beside the genesis fast-move trigger.

The genesis triggers read a move over a minute (75 and 200 bps), which a latency trader's few
seconds never reach. This variant also raises the market to level 2 (a 10 bps minimum spread)
whenever its index has moved `JUMP_BPS` within a second, held by the same 300 s cooldown, so the
fill that closes a position opened on a jump the trader saw coming pays the level-2 spread, as does
everyone's in a fast market.
"""

import numpy as np

JUMP_BPS = 20
COOLDOWN_S = 300.0
MIN_SPREAD_BPS = (0, 2, 10)


def with_jump(genesis_levels, jump_bps: float = JUMP_BPS):
    """A `min_spread(x, step_s)` that takes the higher of the genesis level and the jump trigger's."""

    def min_spread(x: np.ndarray, step_s: float) -> np.ndarray:
        base = genesis_levels(x, step_s)
        k = max(1, int(round(1.0 / step_s)))
        jump = np.zeros(len(x), dtype=bool)
        jump[k:] = np.abs(np.expm1(x[k:] - x[:-k])) * 1e4 >= jump_bps
        cooldown = COOLDOWN_S / step_s
        out = np.empty(len(x), dtype=np.int64)
        held_until = -np.inf
        for i in range(len(x)):
            if jump[i]:
                held_until = i + cooldown
            out[i] = max(base[i], 2 if i < held_until else 0)
        return np.asarray(MIN_SPREAD_BPS, dtype=float)[out] / 1e4

    return min_spread
