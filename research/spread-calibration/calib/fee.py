"""The profit fee that falls with holding time: the schedule, how a hold is measured, and its cost.

**The schedule** is a fee for each hold length, falling to the standard 10% (`BASE`). It is
calibrated from the latency strategy's year totals (`calib/latency.py`): at each hold length, the
lowest fee at which no threshold `theta` takes more than a budget a day,

    fee >= 1 - (negative + budget * 365 / size) / positive,

for an honest node's lead (0.25 s) with a budget near zero (`FAST_BUDGET` in `run.py`, $10,000 a
day), so a fast trader at the best price makes almost nothing, and for #510's 3 s bound with the
launch criterion's budget, the operator's honest share a day. The schedule is the larger of the two at each hold, made to fall with the hold, rounded up
to whole 5%, and interpolated linearly in the logarithm of the hold between the measured lengths.

**A hold** is measured as #614 proposes, on the position, not on lots:
- opening sets the position's open time to now;
- adding moves it to the size-weighted average of the old open time and now, as the entry price
  is averaged, so adding cannot reset the clock or age a fresh lot (`HoldClock.add`);
- a partial close realises its profit at the hold since the open time and leaves the open time as
  it is (`HoldClock.close`);
- a flip closes the whole position at its hold and opens the remainder now;
- a close-out is a close: its profit, if any, pays the fee at its hold; a loss pays none.
An aged position traded around in small slices keeps an old open time, since each add moves it
only by the slice's share: with a slice a tenth of the position closed and added back every 10 s,
each slice's profit pays at a hold of 100 s (`gamed_age`), and the fee there is what such a trader
pays, on a tenth of the size.
"""

from dataclasses import dataclass

import numpy as np

BASE = 0.10
STEP = 0.05
# The owner's schedule (#614, decision D3: 100% to 1 s, 10% at 15 minutes) as the engine's table
# (the plan's "Engine design notes"): (hold in seconds, fee share), linear between points with the
# fee rounded up to the bp, never below the log-linear curve and at most 0.81 points above it.
D3_POINTS = ((1.0, 1.0), (2.0, 0.9083), (4.0, 0.8166), (7.5, 0.7335), (15.0, 0.6418), (30.0, 0.55), (60.0, 0.4583),
             (120.0, 0.3666), (240.0, 0.2749), (450.0, 0.1918), (900.0, 0.10))
# The volume tiers' discounts on the base at launch (#614, decision D7; F7): tiers 2 and 3 pay 9% and
# 8% past the curve.
LAUNCH_DISCOUNTS = (0.0, 0.0, 0.10, 0.20)


def curve_points(held_to_s: float, until_s: float, points: int = 11) -> tuple:
    """A hold curve as a table: the full fee to `held_to_s`, falling linearly in the logarithm of the
    hold to `BASE` at `until_s`, at `points` log-spaced holds. Linear between them, the table sits
    on or above the log-linear curve."""
    holds = np.exp(np.linspace(np.log(held_to_s), np.log(until_s), points))
    holds[0], holds[-1] = held_to_s, until_s
    fees = 1 - (1 - BASE) * np.log(holds / held_to_s) / np.log(until_s / held_to_s)
    return tuple((float(h), float(np.ceil(f * 1e4 - 1e-9) / 1e4)) for h, f in zip(holds, fees))


def d7(hold_s, base: float = BASE, discount: float = 0.0, points: tuple = D3_POINTS) -> np.ndarray:
    """The fee share at `hold_s` under #614 decision D7's function, `max(base x (1 - discount),
    C(hold))`. `C` is the engine's table `points`: the first fee at or below the first hold, linear
    between points below the last, rounded up to the bp, and zero at and past the last hold, where
    the discounted base alone applies."""
    hold = np.asarray(hold_s, dtype=float)
    holds = np.array([h for h, _ in points])
    fees = np.array([f for _, f in points]) * 1e4
    curve = np.where(hold >= holds[-1], 0.0, np.ceil(np.interp(hold, holds, fees, left=fees[0]) - 1e-9))
    return np.maximum(curve / 1e4, base * (1 - discount))


def required(year: dict, size: float, budget_per_day: float) -> dict[float, float]:
    """Per hold, the lowest fee at which no theta's year take passes the budget. `year` maps
    (hold, theta) to (positive, negative) year sums per unit of size."""
    out = {}
    for hold in sorted({k[0] for k in year}):
        need = BASE
        for (h, _), (pos, neg) in year.items():
            if h != hold or pos <= 0:
                continue
            need = max(need, 1 - (neg + budget_per_day * 365 / size) / pos)
        out[hold] = min(1.0, need)
    return out


def envelope(*requirements: dict[float, float]) -> dict[float, float]:
    """The larger requirement at each hold, falling with the hold, rounded up to whole 5%."""
    holds = sorted(set().union(*requirements))
    raw = [max(r.get(h, BASE) for r in requirements) for h in holds]
    falling = np.maximum.accumulate(raw[::-1])[::-1]
    return {h: float(min(1.0, np.ceil(f / STEP - 1e-9) * STEP)) for h, f in zip(holds, falling)}


def fee_at(schedule: dict[float, float], hold_s) -> np.ndarray:
    """The schedule's fee at `hold_s`, linear in the logarithm of the hold between its points,
    the first point's fee below them and `BASE` past the hold where it reaches it."""
    holds = np.array(sorted(schedule), dtype=float)
    fees = np.array([schedule[h] for h in holds])
    x = np.log(np.maximum(np.asarray(hold_s, dtype=float), 1e-3))
    return np.interp(x, np.log(holds), fees, left=fees[0], right=BASE)


def schedule_take(year: dict, size: float, schedule: dict[float, float]) -> dict[float, float]:
    """The best theta's take a day at each hold under `schedule`."""
    out = {}
    for hold in sorted({k[0] for k in year}):
        f = float(fee_at(schedule, hold))
        out[hold] = max(((1 - f) * pos - neg) * size / 365 for (h, _), (pos, neg) in year.items() if h == hold)
    return out


def honest_cost(holding_rows: list[dict], schedule: dict[float, float] | None, points: int = 64, from_s: float = 0.0,
                to_s: float = float("inf"), fee_fn=None) -> dict:
    """What the schedule costs ordinary flow, from the #111 flow's holding-time profile: each
    bucket's share of notional, with holds log-uniform inside it (the first bucket from half a
    second, the open last bucket to four times its start), each hold's gross profit taken as
    proportional to notional times the square root of the hold (a zero-edge position's expected
    gain). Gives the share of honest profit that pays more than 10%, and the extra fee as a share of
    honest gross profit, over the holds in `[from_s, to_s)`: the target client's holds are 15 minutes
    or more (#614), and the shorter holds are the flow the schedule is meant to make uneconomic.
    `fee_fn(holds)`, when given, is the fee in place of `schedule`'s."""
    profit, over, extra = 0.0, 0.0, 0.0
    for r in holding_rows:
        lo0, hi0 = max(float(r["hold_from_s"]), 0.5), float(r["hold_to_s"])
        hi0 = hi0 if np.isfinite(hi0) else lo0 * 4
        lo, hi = max(lo0, from_s), min(hi0, to_s)
        if hi <= lo:
            continue
        # The bucket's notional, log-uniform over its holds, in proportion to the part kept.
        r = dict(r, notional_share=float(r["notional_share"]) * np.log(hi / lo) / np.log(hi0 / lo0))
        holds = np.exp(np.linspace(np.log(lo), np.log(hi), points))
        weight = float(r["notional_share"]) * np.sqrt(holds) / points
        f = fee_fn(holds) if fee_fn else fee_at(schedule, holds)
        profit += weight.sum()
        over += weight[f > BASE + 1e-9].sum()
        extra += (weight * (f - BASE)).sum()
    return {"share_of_profit_over_base": float(over / profit), "extra_fee_share_of_profit": float(extra / profit)}


def family(peak: float, until_s: float, from_s: float = 1.0) -> dict[float, float]:
    """A schedule at `peak` up to `from_s`, falling linearly in the logarithm of the hold to `BASE`
    at `until_s`; `peak` at `BASE` is the flat standard fee."""
    if peak <= BASE or until_s <= from_s:
        return {from_s: BASE}
    return {from_s: peak, until_s: BASE}


@dataclass
class HoldClock:
    """A position's quantity and open time, in seconds, for the holding-time fee."""

    qty: float = 0.0
    opened: float = 0.0

    def add(self, qty: float, now: float) -> None:
        """Grows the position by `qty` (same sign) at `now`."""
        if self.qty == 0:
            self.qty, self.opened = qty, now
            return
        if np.sign(qty) != np.sign(self.qty):
            raise ValueError("an add has the position's sign; a reduction is a close")
        total = self.qty + qty
        self.opened = (abs(self.qty) * self.opened + abs(qty) * now) / abs(total)
        self.qty = total

    def close(self, qty: float, now: float) -> float:
        """Reduces the position by `qty` (opposite sign, at most the position) and returns the hold
        the closed part's profit pays at. The open time stays."""
        if np.sign(qty) == np.sign(self.qty) or abs(qty) > abs(self.qty) + 1e-12:
            raise ValueError("a close is opposite the position and at most its size")
        hold = now - self.opened
        self.qty += qty
        if abs(self.qty) < 1e-12:
            self.qty, self.opened = 0.0, 0.0
        return hold

    def trade(self, qty: float, now: float) -> float | None:
        """Any fill: an add, a close, or a flip (a close of the whole position and an add of the
        remainder at `now`). Returns the hold a realised profit pays at, or None if nothing closed."""
        if self.qty == 0 or np.sign(qty) == np.sign(self.qty):
            self.add(qty, now)
            return None
        if abs(qty) <= abs(self.qty):
            return self.close(qty, now)
        rest = qty + self.qty
        hold = self.close(-self.qty, now)
        self.add(rest, now)
        return hold


def gamed_age(position_over_slice: float, interval_s: float) -> float:
    """The hold a slice's profit pays at, in the steady state where a slice `1 / position_over_slice`
    of a position is closed and added back every `interval_s`: each add pulls the open time toward
    now by the slice's share, so the age after an add settles at `(ratio - 1) * interval`, and a
    close one interval later pays at `ratio * interval`."""
    return position_over_slice * interval_s
