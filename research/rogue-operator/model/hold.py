"""The free option on a held sync: deciding after the market has moved whether a trade happened.

Replies and stream entries go out only once their batch is durable, so a host that stalls the
batch's sync holds a trade nobody outside has seen. It can let the sync finish, or kill the node,
and the restarted log never had the trade (SECURITY.md, #541). The node raises a Critical alert
and stops stamping once a batch is held past 5 s, so a hold of up to `hold` seconds is silent;
every client sees replies and new stream entries stall for its length, and a kill is a restart.

At a moment `t` the operator opens a position at the engine's index, which it holds `lag` behind
the venues, in the direction the venues have already moved: `d = sign(x[t] - x[t - lag])`, at
`x[t - lag]`. It then holds the sync for `hold`. Committed, the position closes at the best index
it can then show, the most favourable of `x[t + hold - lag .. t + hold]`; if that loses, it kills
the node instead and the trade never happened. Without the option to kill, the same trade keeps
its loss, so what the hold adds is the loss it avoids: the mean of `max(-committed, 0)`. A hold
no longer than the lag never needs a kill, since the close's window still reaches back to the
moment the trade opened in the money. The profit fee takes 10% of a positive result.
"""

import numpy as np

from model.paths import window_max, window_min

PROFIT_FEE = 0.10


def option(x: np.ndarray, starts: np.ndarray, lag: int, hold: int) -> dict:
    """Per start, in log terms per unit of size: with the hold, without it, and whether it kills."""
    entry = x[starts - lag]
    d = np.where(x[starts] >= entry, 1.0, -1.0)
    hi_now, lo_now = window_max(x, lag)[starts - lag], window_min(x, lag)[starts - lag]
    end = starts + hold
    hi_end, lo_end = window_max(x, lag)[end - lag], window_min(x, lag)[end - lag]
    committed = np.where(d > 0, hi_end - entry, entry - lo_end)
    kill = committed < 0
    won = (1 - PROFIT_FEE) * np.maximum(committed, 0.0)
    return {
        "held": won,
        "without": won + np.minimum(committed, 0.0),
        "now": (1 - PROFIT_FEE) * np.where(d > 0, hi_now - entry, entry - lo_now),
        "kill": kill,
    }
