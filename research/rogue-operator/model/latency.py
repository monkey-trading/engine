"""Latency arbitrage: the operator trading its own accounts against the index it holds back.

The host can hold every venue's stream back by up to `max_delay` and set the clock so the held
quotes still look fresh, so the engine's index can run up to `max_delay + max_age_ms` (3 s in
production) behind the venues while the host sees them live (SECURITY.md, #510). At each decision
the operator therefore knows the engine's next `lag` seconds of index exactly, and trades a
position of `-1`, `0` or `+1` units of its size against it.

The policy is a receding-horizon dynamic programme: at each decision it solves for the best path
of positions over the index it knows, given the spread each fill pays, and takes the first step.
With no spread the best step is simply the sign of the next move, so the operator collects the
index's whole variation at its decision interval and the lag beyond one interval adds nothing;
a spread makes the lag matter, because only moves larger than the round trip pay.

Costs, per unit traded, as a share of the mid: the stress level's minimum spread
(`model/stress.py`), an optional extra spread (the parameter sweep), and, for an account that does
not launder its toxicity score, the top tier's 3 bps. The size spread is zero below the first tier,
which an operator stays under with one account per $2M (BTC-USD) of position. The profit fee takes
10% of each position's positive realised PnL, spreads included; losses pay none.
"""

import numpy as np

from model import stress as stress_model

STATES = np.array([-1.0, 0.0, 1.0])
TRADE = np.abs(STATES[:, None] - STATES[None, :])  # units traded from state i to state j
TOXIC_TIER_BPS = 3
PROFIT_FEE = 0.10


def policy(r: np.ndarray, c: np.ndarray, horizon: int) -> np.ndarray:
    """Positions, one per return, of the receding-horizon policy that knows `horizon` returns
    ahead: `r[k] .. r[k + horizon - 1]` at decision `k`, each fill paying `c[k]` per unit."""
    k = len(r)
    if horizon < 1:
        return np.zeros(k)
    rp = np.concatenate([r, np.zeros(horizon)])
    cp = np.concatenate([c, np.full(horizon, c[-1] if len(c) else 0.0)])
    value = np.zeros((k, 3))
    for j in range(horizon - 1, 0, -1):
        rj, cj = rp[j : j + k], cp[j : j + k]
        cand = value[:, None, :] + STATES[None, None, :] * rj[:, None, None] - cj[:, None, None] * TRADE[None, :, :]
        value = cand.max(axis=2)
    cand = value[:, None, :] + STATES[None, None, :] * r[:, None, None] - c[:, None, None] * TRADE[None, :, :]
    action = cand.argmax(axis=2)  # (k, from-state) -> to-state
    state = 1  # flat
    out = np.empty(k)
    for i in range(k):
        state = action[i, state]
        out[i] = STATES[state]
    return out


def settle(pos: np.ndarray, r: np.ndarray, c: np.ndarray, fee: float = PROFIT_FEE) -> dict:
    """Realised PnL per unit of size: each position's moves less the spreads it paid opening and
    closing, less the profit fee on its positive results."""
    prev = np.concatenate([[0.0], pos[:-1]])
    nxt = np.concatenate([pos[1:], [0.0]])
    gross = float((pos * r).sum())
    opened = (pos != 0) & (pos != prev)
    starts = np.flatnonzero(opened)
    ends = np.flatnonzero((pos != 0) & (nxt != pos)) + 1
    step = pos * r - c * np.abs(pos) * opened
    closing = (pos != 0) & (nxt != pos)
    step -= np.where(closing, np.abs(pos) * np.concatenate([c[1:], [c[-1]]]), 0.0)
    csum = np.concatenate([[0.0], np.cumsum(step)])
    results = csum[ends] - csum[starts]
    fees = fee * results[results > 0].sum()
    spread = gross - float(step.sum())
    return {
        "pnl": float(results.sum() - fees),
        "gross": gross,
        "spread": spread,
        "fee": float(fees),
        "positions": int(len(starts)),
        "units_traded": float(np.abs(np.diff(np.concatenate([[0.0], pos, [0.0]]))).sum()),
        "win_share": float((results > 0).mean()) if len(results) else 0.0,
    }


def arbitrage(x: np.ndarray, step_s: float, lead: int, decide_s: float, lag_s: float, extra_bps: float = 0.0,
              launder: bool = True, stress: bool = True) -> dict:
    """The operator's PnL per unit of size over the hour `x[lead : lead + 1 h]`."""
    every = int(round(decide_s / step_s))
    e = x[::every]
    r = np.diff(e)
    c = np.full(len(r), extra_bps / 1e4)
    if stress:
        c += stress_model.min_spread(e, decide_s)[:-1]
    if not launder:
        c += TOXIC_TIER_BPS / 1e4
    pos = policy(r, c, int(np.floor(lag_s / decide_s + 1e-9)))
    lo = lead // every
    hi = lo + int(round(3600 / decide_s))
    return settle(pos[lo:hi], r[lo:hi], c[lo:hi])
