"""How much of the hourly bars' conservatism is each bar being one jump, measured on crypto's 50 ms grids read every 60 s.

The account simulator reads one mark an hour, so a move inside the hour reaches it as a single jump
from the hour's open to its close, and a position is closed only after it. The grids (the rogue-operator
study's 50 ms Coinbase last trades, stratum-weighted) give the same hours at any cadence. For a position
opened at the start of a sampled hour with equity `e` (a share of its notional) the close-out triggers
when its loss reaches `(1 - level) e`; it is past its balance if the loss at the close is `e` or more.
Read once at the hour's end (the simulator's cadence) against every `step` seconds, per market:

- the share of hours (both directions) in which it ends past its balance;
- the mean excess loss past the balance, per notional.

`z` is `e` over the market's hourly standard deviation of the same hours, so a market without a grid reads
the ratio at the `z` its own margin has: `z_of_bands` below gives each grouped market's first band.
"""

import numpy as np

from pm import horizon
from pm.context import LEVEL


def outcome(x: np.ndarray, e: float, step: int, level: float) -> tuple[float, float]:
    """(past-balance share, excess), averaged over both directions, for one hour's log price `x` read every `step` samples."""
    idx = np.arange(step, len(x), step)
    if len(idx) == 0 or idx[-1] != len(x) - 1:
        idx = np.append(idx, len(x) - 1)
    d = np.expm1(x[idx] - x[0])
    share = excess = 0.0
    for loss in (-d, d):  # a long and a short
        hit = np.flatnonzero(loss >= (1 - level) * e)
        final = loss[hit[0]] if len(hit) else loss[-1]
        share += float(final >= e) / 2
        excess += max(float(final) - e, 0.0) / 2
    return share, excess


def table(es=(0.0025, 0.005, 0.01, 0.02, 0.05), steps=(1, 10, 60, 300), level: float = LEVEL) -> list[dict]:
    rows, hs = horizon.load()
    out = []
    for market in sorted({r[0] for r in rows}):
        sel = [r for r in rows if r[0] == market]
        sd = float(np.sqrt(np.average([(x[-1] - x[0]) ** 2 for _, x, _ in sel], weights=[w for _, _, w in sel])))
        for e in es:
            base = None
            for step_s in (3600, *steps[::-1]):
                step = int(round(step_s / hs))
                rate = ex = tot = 0.0
                for _, x, w in sel:
                    r, m = outcome(x, e, min(step, len(x) - 1), level)
                    rate, ex, tot = rate + w * r, ex + w * m, tot + w
                row = {"market": market, "margin": e, "z": e / sd, "check_seconds": step_s, "past_balance_share": rate / tot, "mean_excess": ex / tot}
                if step_s == 3600:
                    base = row
                row["share_over_hourly"] = row["past_balance_share"] / base["past_balance_share"] if base["past_balance_share"] else float("nan")
                row["excess_over_hourly"] = row["mean_excess"] / base["mean_excess"] if base["mean_excess"] else float("nan")
                out.append(row)
    return out


def z_of_bands(ctx) -> list[dict]:
    """Each launch market's first band over its hourly standard deviation (the cap study's bars)."""
    rows = []
    for s, spec in ctx.specs.items():
        r = ctx.hr.hour_ret[:, ctx.hr.index[s]]
        r = r[np.isfinite(r)]
        rows.append({"market": s, "class": spec.asset_class, "first_band": spec.bands[0][1], "hour_sd": float(r.std()), "z": spec.bands[0][1] / float(r.std())})
    return rows
