"""Skimming client fills and censoring closes: the house's gain from choosing when each fill prices.

The host holds the index up to `lag` behind the venues and decides when each client request
reaches the sequencer, up to `delay` after it arrived. A client order that arrives at `a` can
therefore fill at the index of any moment from `a - lag` to `a + delay`, and the operator, who
sees the venues live, picks the moment worst for the client: the highest index in that window for
a buy and the lowest for a sell. The house is the counterparty, so the client's loss against the
index at its arrival is the house's gain, and the operator's share of distributions (20% from
V47) makes it the operator's too.

Three measures of what that is worth, per fill, in bps of the fill's notional, against the index
at arrival (what the fill gets from an honest node, give or take its delivery latency):
- `bound`: every order gets the worst index in its own window. An upper bound: the index is one
  path, and moving it forward for one order moves it for the next.
- `sequenced`: orders keep their arrival order and the index only moves forward, so each order's
  moment is at or after the one before it (`schedule`, an exact dynamic programme over the
  moments). The operator could do better by reordering clients, so this is achievable, not the
  most.
- `causal`: for a delay longer than the lag, the operator cannot know the window's worst moment
  until it has passed; it waits the whole delay and takes the worst index of the last `lag`.
  With `delay = 0` it is `bound` for that order. A martingale index gains nothing from waiting
  beyond the lag; momentum would.
"""

import numpy as np

from model.paths import window_max, window_min


def arrivals(rng, rate_per_s: float, start: int, length: int, step_s: float):
    """Poisson arrival steps in `[start, start + length)` at `rate_per_s`, and their sides (+1 a buy)."""
    n = rng.poisson(rate_per_s * length * step_s)
    at = np.sort(rng.integers(start, start + length, size=n))
    side = np.where(rng.random(n) < 0.5, 1.0, -1.0)
    return at, side


def bound(x: np.ndarray, at: np.ndarray, side: np.ndarray, lag: int, delay: int) -> np.ndarray:
    """Each order's gain in log terms if it alone chose its moment in `[a - lag, a + delay]`."""
    w = lag + delay
    hi = window_max(x, w)[at - lag]
    lo = window_min(x, w)[at - lag]
    return np.where(side > 0, hi - x[at], x[at] - lo)


def causal(x: np.ndarray, at: np.ndarray, side: np.ndarray, lag: int, delay: int) -> np.ndarray:
    """Each order's gain when executed at `a + delay` at the worst index of the `lag` before it."""
    end = at + delay
    hi = window_max(x, lag)[end - lag]
    lo = window_min(x, lag)[end - lag]
    return np.where(side > 0, hi - x[at], x[at] - lo)


def schedule(x: np.ndarray, at: np.ndarray, side: np.ndarray, size: np.ndarray, lag: int, delay: int) -> np.ndarray:
    """Each order's gain under the best monotone schedule: order `i` fills at moment `u_i` with
    `a_i - lag <= u_i <= a_i + delay` and `u_i >= u_{i-1}`, maximising the size-weighted total."""
    n = len(at)
    if n == 0:
        return np.zeros(0)
    w = lag + delay + 1
    slot = np.arange(w)
    # best[j]: the best total up to the previous order with its moment at window slot j.
    prev_lo = None
    best = None
    choice = np.empty((n, w), dtype=np.int64)  # back-pointer: the previous order's slot
    for i in range(n):
        lo = at[i] - lag
        val = side[i] * size[i] * (x[lo : lo + w] - x[at[i]])
        if best is None:
            carry = np.zeros(w)
            choice[i] = -1
        else:
            shift = lo - prev_lo
            # The previous order's best at or before each of this order's moments, and where.
            run = np.maximum.accumulate(best)
            running = np.maximum.accumulate(np.where(best >= run, slot, 0))
            idx = slot + shift
            inside = idx < w
            carry = np.where(inside, run[np.minimum(idx, w - 1)], run[-1])
            arg = np.where(inside, running[np.minimum(idx, w - 1)], running[-1])
            choice[i] = arg
        best = val + carry
        prev_lo = lo
    # Trace the schedule back.
    slots = np.empty(n, dtype=np.int64)
    slots[-1] = int(np.argmax(best))
    for i in range(n - 1, 0, -1):
        slots[i - 1] = choice[i, slots[i]]
    moments = at - lag + slots
    return side * (x[moments] - x[at])
