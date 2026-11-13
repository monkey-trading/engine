"""The target client's flow: the #111 flow with the holds under 15 minutes largely gone (#614).

The owner's target client holds 15 to 30 minutes or longer; the reference venue's short holds are
flow that a zero fee pays for, which the holding-time fee is meant to make uneconomic. The cap
study's flow keeps everything opened and closed within an hour in one class per kind and leverage
bin (`hold_h == 0`). This splits each such class by the holding-time profile's notional shares
(holds log-uniform inside each bucket): the part held 15 to 60 minutes stays, with that range's
mean hold and mean square-root hold, and the part held under 15 minutes keeps `KEEP_SHORT` of its
margin with the class's own holds. Classes held an hour or more are unchanged. The split is by
notional, applied to margin shares, which treats the within-hour holds as independent of leverage.
"""

from dataclasses import replace

import numpy as np

TARGET_FROM_S = 900.0
KEEP_SHORT = 0.10


def within_hour_split(holding_rows: list[dict]) -> float:
    """The share of the within-hour notional held `TARGET_FROM_S` or more."""
    short = target = 0.0
    for r in holding_rows:
        lo, hi = max(float(r["hold_from_s"]), 0.5), float(r["hold_to_s"])
        if lo >= 3600:
            continue
        hi = min(hi, 3600.0)
        share = float(r["notional_share"])
        cut = min(max(TARGET_FROM_S, lo), hi)
        part = np.log(hi / cut) / np.log(hi / lo)
        target += share * part
        short += share * (1 - part)
    return target / (short + target)


def target_flow(flow, holding_rows: list[dict], keep_short: float = KEEP_SHORT):
    from model.flow import FlowClass

    held = within_hour_split(holding_rows)
    lo, hi = TARGET_FROM_S / 3600, 1.0
    mean_h = (hi - lo) / np.log(hi / lo)
    mean_sqrt_h = 2 * (np.sqrt(hi) - np.sqrt(lo)) / np.log(hi / lo)
    classes = []
    for c in flow.classes:
        if c.hold_h != 0:
            classes.append(c)
            continue
        classes.append(replace(c, margin_share=c.margin_share * (1 - held) * keep_short))
        classes.append(FlowClass(c.kind, 0, c.bucket, c.margin_share * held, float(mean_h), float(mean_sqrt_h), c.long_share))
    return replace(flow, classes=classes)
