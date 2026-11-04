"""The automatic stress level's fast-move trigger, as `crates/core/src/stress.rs` applies it.

The reading is the largest move to the current mid from the lowest or highest mid of the trailing
`stress_move_window_ms` (60 s at genesis), whose buckets reach back up to a tenth of the window
further; `WINDOW_S` takes the window plus half a bucket. The automatic level rises at once to the
highest level whose trigger the reading meets, and steps down only once the cooldown (300 s) has
passed since it last changed. The other two triggers are left out: utilisation, because the
modelled operator keeps the house's exposure under the level-1 share of its cap, and divergence,
because the historical sample has Coinbase's trades but not Kraken's book (the live recording's
divergence stays far below the trigger; `results/calibration.json`).
"""

import numpy as np

from model.paths import window_max, window_min

WINDOW_S = 63.0
COOLDOWN_S = 300.0
# Genesis triggers (config/production-genesis.toml): move bps for levels 1 and 2.
MOVE_BPS = (75, 200)
# Each level's minimum spread in bps; the size spreads' first tier is 0, so a multiplier adds none.
MIN_SPREAD_BPS = (0, 2, 10)


def move_reading_bps(x: np.ndarray, step_s: float) -> np.ndarray:
    """Each step's reading: the largest move to it from the trailing window's low or high, in bps.
    Steps before a whole window has passed read the window they have."""
    w = int(round(WINDOW_S / step_s))
    pad = np.concatenate([np.full(w, x[0]), x])
    low = window_min(pad, w)[: len(x)]
    high = window_max(pad, w)[: len(x)]
    return np.maximum(np.expm1(x - low), -np.expm1(x - high)) * 1e4


def levels(x: np.ndarray, step_s: float, move_bps=MOVE_BPS, cooldown_s: float = COOLDOWN_S) -> np.ndarray:
    """The automatic level at each step: up at once, down only a cooldown after its last change."""
    reading = move_reading_bps(x, step_s)
    target = np.zeros(len(x), dtype=np.int64)
    for k, threshold in enumerate(move_bps, start=1):
        target[reading >= threshold] = k
    out = np.empty(len(x), dtype=np.int64)
    level, since = 0, -np.inf
    cooldown = cooldown_s / step_s
    for i, t in enumerate(target):
        if t > level or (t < level and i - since >= cooldown):
            level, since = int(t), i
        out[i] = level
    return out


def min_spread(x: np.ndarray, step_s: float) -> np.ndarray:
    """Each step's minimum spread, as a fraction of the mid."""
    return np.asarray(MIN_SPREAD_BPS, dtype=float)[levels(x, step_s)] / 1e4
