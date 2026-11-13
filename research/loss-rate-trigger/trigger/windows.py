"""A loss reading over a trailing window, and the stress level it drives.

The reading at a bin is the loss summed over the trailing `w` bins, that bin included. The level
follows `crates/core/src/stress.rs`: it rises at once when the reading meets the threshold and steps
down only once the cooldown has passed since it rose and the reading is below the threshold again.
"""

import numpy as np

COOLDOWN_S = 300.0  # `stress_cooldown_ms` at genesis


def trailing(series: np.ndarray, w: int) -> np.ndarray:
    """The sum of `series` over each bin's trailing `w` bins (fewer at the start)."""
    c = np.concatenate([[0.0], np.cumsum(series, dtype=np.float64)])
    idx = np.arange(1, len(series) + 1)
    return c[idx] - c[np.maximum(idx - w, 0)]


def hourly_max(reading: np.ndarray, bins_per_hour: int) -> np.ndarray:
    """The highest reading within each whole hour."""
    hours = len(reading) // bins_per_hour
    return reading[: hours * bins_per_hour].reshape(hours, bins_per_hour).max(axis=1)


def raised(reading: np.ndarray, threshold: float, cooldown_bins: int) -> np.ndarray:
    """Whether the level is raised at each bin: from the first bin the reading meets `threshold`
    until the first bin at least `cooldown_bins` later at which it no longer does."""
    n = len(reading)
    above = reading >= threshold
    idx = np.arange(n)
    next_above = np.minimum.accumulate(np.where(above, idx, n)[::-1])[::-1]
    next_below = np.minimum.accumulate(np.where(~above, idx, n)[::-1])[::-1]
    out = np.zeros(n, dtype=bool)
    pos = 0
    while pos < n:
        a = int(next_above[pos])
        if a >= n:
            break
        b = n if a + cooldown_bins >= n else int(next_below[a + cooldown_bins])
        out[a:b] = True
        pos = b
    return out


def disjoint_room(honest: np.ndarray, w: int, threshold: float) -> np.ndarray:
    """Per disjoint window of `w` bins, the most an attacker adds before the reading meets
    `threshold`: `max(0, threshold - honest loss in the window)`. Summed over a day's windows it
    bounds the attacker's day from above (the reading must stay under the threshold over these
    windows as over any other), less the overshoot of the command that crosses it."""
    k = len(honest) // w
    per = honest[: k * w].reshape(k, w).sum(axis=1)
    return np.maximum(threshold - per, 0.0)


def strata(vol: np.ndarray, is_open: np.ndarray, uppers=(0.50, 0.80, 0.95, 0.99, 1.00)) -> np.ndarray:
    """Each hour's volatility stratum by its rank among the open hours (`research/rogue-operator`'s
    `derive/sample.py` shares: the quietest half, then 50-80%, 80-95%, 95-99% and the top 1%); -1 when
    closed."""
    out = np.full(len(vol), -1, dtype=np.int64)
    open_idx = np.flatnonzero(is_open)
    order = open_idx[np.argsort(vol[open_idx], kind="stable")]
    lo = 0
    for k, upper in enumerate(uppers):
        hi = int(round(upper * len(order)))
        out[order[lo:hi]] = k
        lo = hi
    return out
