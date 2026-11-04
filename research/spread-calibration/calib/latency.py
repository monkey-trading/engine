"""Latency arbitrage at a spread and a holding-time fee: what each hold length is worth.

A trader who sees the venues `lag` seconds before the engine prices knows the engine's next
`lag` seconds. #549 found that at a zero spread it collects the index's whole variation; with a
spread it trades only moves larger than the round trip, and with a profit fee that falls with
holding time it must also choose how long to hold. This module measures, per sampled hour, a
simple strategy: when the venues have moved at least `theta` from the engine's index, open a
position at that index plus the spread in the venues' direction; hold it `hold` seconds; close at
the best index the last `lag` seconds before then offer, less the spread. Positions do not overlap.

What it knows is the move up to the moment it opens (and the window it closes in); what happens
in between is noise to it. The recorded path also carries the sample's own drift after large moves,
which is not the trader's edge, so each trade counts twice, with that later move as recorded and
mirrored: a martingale's outcome. For each (lag, hold, theta) the hour gives the sums of the
positive and negative results per unit of size, so any fee schedule can be applied afterwards: a
schedule's take is `(1 - fee(hold)) * positive - negative`.
"""

import numpy as np

DECIDE_S = 0.25
LAGS_S = (0.25, 1.0, 3.0)
HOLDS_S = (1, 3, 5, 10, 20, 30, 60, 120, 300, 600, 900, 1800)
THETAS_BPS = (1, 2, 3, 5, 8, 12, 20, 40)


def trades(t: np.ndarray, eligible: np.ndarray, hold: int) -> np.ndarray:
    """Indices into `t` of the trades a strategy takes: each eligible moment once the previous
    position has closed."""
    idx = np.flatnonzero(eligible)
    taken, free_at = [], -1
    k = 0
    while k < len(idx):
        i = idx[k]
        if t[i] >= free_at:
            taken.append(i)
            free_at = t[i] + hold
            k = int(np.searchsorted(t[idx], free_at, side="left"))
        else:
            k += 1
    return np.asarray(taken, dtype=np.int64)


def hour_rows(h, spread: float, window_max, window_min, min_spread=None) -> list[dict]:
    """Every (lag, hold, theta) on one hour: sums of positive and negative results per unit of
    size, mirrored, and the trade count. `spread` is the tier's, a fraction of the mid; with
    `min_spread` (the stress model's, from a path at 1 s steps) each fill also pays the stress
    level's minimum at its moment, as the engine adds it."""
    rows = []
    span = h.analysis
    t = np.arange(span.start, span.stop, h.steps(DECIDE_S))
    every = h.steps(1.0)
    stress = np.zeros(len(h.x)) if min_spread is None else np.repeat(min_spread(h.x[::every], 1.0), every)[: len(h.x)]
    for lag_s in LAGS_S:
        lag = h.steps(lag_s)
        known = h.x[t] - h.x[t - lag]
        d = np.where(known >= 0, 1.0, -1.0)
        entry = h.x[t - lag]
        # The engine's stress level when a fill is sequenced reads the index it then shows.
        open_cost = spread + stress[t - lag]
        hi_w, lo_w = window_max(h.x, lag), window_min(h.x, lag)
        for hold_s in HOLDS_S:
            hold = h.steps(hold_s)
            fits = t + hold < len(h.x)
            end = np.minimum(t + hold - lag, len(hi_w) - 1)
            close = np.where(d > 0, hi_w[end], lo_w[end])
            close_cost = spread + stress[end]
            # Known when it opens (the venues' move, less both fills' costs) and when it closes (the
            # best of its last window); between them, the move it could not see, which is mirrored.
            seen_from = np.maximum(end, t)
            known_part = d * known - open_cost - close_cost + d * (close - h.x[seen_from])
            unseen = d * (h.x[seen_from] - h.x[t])
            for theta in THETAS_BPS:
                taken = trades(t, (d * known >= theta * 1e-4) & fits, hold)
                a = known_part[taken] + unseen[taken]
                b = known_part[taken] - unseen[taken]
                both = np.concatenate([a, b])
                rows.append({"market": h.market, "hour": h.hour, "stratum": h.stratum, "lag_s": lag_s, "hold_s": hold_s,
                             "theta_bps": theta, "positive": float(np.clip(both, 0, None).sum() / 2),
                             "negative": float(np.clip(-both, 0, None).sum() / 2), "trades": len(taken),
                             "covered": float(fits.mean())})
    return rows
