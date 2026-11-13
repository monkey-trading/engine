"""The engine rules the simulation applies, per unit of position notional.

Each function mirrors one rule in `crates/core` in floating point; the engine's `Fixed` rounding
(always in the house's favour, at 18 decimals) is far below the model's precision. Prices are
relative to the entry price, so a position's notional at entry is 1 and its price `x` is
`price / entry_price`. `side` is +1 for a long and -1 for a short.
"""

from dataclasses import dataclass

import numpy as np

PROFIT_FEE_BPS = 1000  # genesis.params.profit_fee_bps
CLOSEOUT_LEVEL_BPS = 5000  # genesis.params.closeout_level_bps


def profit_fee(realised, fee_bps: int = PROFIT_FEE_BPS):
    """Losers pay nothing; winners pay `fee_bps` of realised profit (`execution.rs`, `profit_fee`)."""
    return np.maximum(realised, 0.0) * fee_bps / 1e4


def closeout_trigger(side, balance, margin_rate, level_bps: int = CLOSEOUT_LEVEL_BPS):
    """The relative price at which a one-position account's equity falls to the close-out level.

    Equity is `balance + side * (x - 1)` and the requirement is `margin_rate * x` at the mark
    (`engine.rs`, `equity_and_margin`); close-out starts when equity is below `level` of it
    (`closeout.rs`, `next_closeout`). A long triggers at or below the returned price, a short at or
    above it. For a market quoted in another currency, both sides of the comparison scale by the
    FX ratio `g` since entry, so pass `balance / g`.
    """
    level = level_bps / 1e4
    return (1.0 - side * balance) / (1.0 - side * level * margin_rate)


def closeout_fill(side, trigger, bar_open, bar_extreme):
    """Fill price of a close-out in a bar that reaches `trigger`: the bar's open when the market
    gapped through the trigger, else the trigger (continuous trading). `bar_extreme` is the low
    for a long and the high for a short. Returns NaN when the bar does not reach the trigger."""
    gapped = side * (bar_open - trigger) <= 0
    reached = side * (bar_extreme - trigger) <= 0
    return np.where(gapped, bar_open, np.where(reached, trigger, np.nan))


def realised_pnl(side, exit_price, fx_ratio=1.0):
    """Realised PnL per unit of USD entry notional. A market quoted in another currency accrues PnL
    in that currency and converts it to USD at the FX index of the fill (#98), so its PnL carries
    `fx_ratio`, the FX index at exit over at entry."""
    return side * (exit_price - 1.0) * fx_ratio


def settle(pnl, balance, financing=0.0, fee_bps: float = PROFIT_FEE_BPS, spread_cost=0.0):
    """One account's exit: the profit fee, the bad debt written off against the house, and what is
    left to the client. Financing has already moved the balance at each rollover, so the account
    loses at most its balance after financing (`closeout.rs`, `settle_bad_debt`). Returns
    `(fee, bad_debt, proceeds)`; the house takes `-pnl + fee - bad_debt`. `fee_bps` is the profit
    fee at the position's hold, for a fee that falls with holding time. `spread_cost` is what both
    fills' spreads took from the price, which the house books when the position opens: the engine
    realises PnL at the spread-adjusted fill prices, so the fee is on `pnl - spread_cost`."""
    fee = profit_fee(pnl - spread_cost, fee_bps)
    equity = balance + pnl - fee - financing
    bad = np.maximum(-equity, 0.0)
    return fee, bad, equity + bad


def daily_financing(side, notional, benchmark_bps, markup_bps, day_basis):
    """One rollover's financing, positive when the account pays: longs pay benchmark + markup,
    shorts pay markup - benchmark (`funds.rs`, `daily_financing`)."""
    annual_bps = np.where(np.asarray(side) > 0, benchmark_bps + markup_bps, markup_bps - benchmark_bps)
    return np.abs(notional) * annual_bps / 1e4 / day_basis


def exposure_cap(nav, cap_bps, floor):
    """A house exposure cap: the larger of a share of house NAV and a fixed floor (#15 section 2,
    PR #99)."""
    return np.maximum(np.asarray(nav) * cap_bps / 1e4, floor)


def order_room(exposure, cap, total_room):
    """The largest long and short order that fills against clients' net exposure `exposure` in a
    market: a fill that grows the absolute exposure must stay within the market cap and within
    `total_room`, the total cap less the gross exposure of the other markets; one that shrinks it
    always fills (`execution.rs`, `check_risk`). Returns `(long_room, short_room)`."""
    exposure = np.asarray(exposure, dtype=float)
    long_room = np.maximum(2 * np.maximum(-exposure, 0.0), np.minimum(cap - exposure, total_room - exposure))
    short_room = np.maximum(2 * np.maximum(exposure, 0.0), np.minimum(cap + exposure, total_room + exposure))
    return np.maximum(long_room, 0.0), np.maximum(short_room, 0.0)


def pair_room(exposure, cap, total_room):
    """The largest leg of a hedged pair whose two orders the engine fills at clients' net exposure
    `exposure`. The accounts open the restoring leg first — it shrinks the absolute exposure,
    which `check_risk` exempts — and the returning leg is then checked with the first leg on the
    books; it lands the exposure back where the pair found it, so a pair fits only while that
    state is within the market cap and `total_room`, and its leg is the state's room plus the
    restoring leg's. Returns the leg room, 0 when the state is past a cap (`execution.rs`,
    `check_risk`)."""
    exposure = np.asarray(exposure, dtype=float)
    headroom = np.minimum(cap, total_room)
    return np.where(np.abs(exposure) <= headroom, np.abs(exposure) + np.maximum(headroom, 0.0), 0.0)


def accepted_share(exposure, cap, longs, shorts):
    """Share of new long and new short notional accepted when they would take the house's net
    client exposure past `cap`.

    The engine checks each order alone and refuses one whose fill would push the absolute net
    exposure past the cap (`execution.rs`, `check_risk`), while a fill that does not grow the
    absolute exposure is never refused. Over one step the model nets the step's orders: a netted
    step that moves the exposure toward zero — including across it — fills in full, however large
    the orders; a step that grows it accepts the reducing side in full and clips the crowded side
    to the cap. Returns `(long_share, short_share)`.
    """
    exposure, cap = np.asarray(exposure, dtype=float), np.asarray(cap, dtype=float)
    longs, shorts = np.asarray(longs, dtype=float), np.asarray(shorts, dtype=float)
    with np.errstate(divide="ignore", invalid="ignore"):
        long_room = np.where(longs > 0, (cap - exposure + shorts) / longs, 1.0)
        short_room = np.where(shorts > 0, (cap + exposure + longs) / shorts, 1.0)
    long_share, short_share = np.clip(long_room, 0.0, 1.0), np.clip(short_room, 0.0, 1.0)
    toward_zero = np.abs(exposure + longs - shorts) <= np.abs(exposure)
    return np.where(toward_zero, 1.0, long_share), np.where(toward_zero, 1.0, short_share)


@dataclass(frozen=True)
class HoldFee:
    """The profit fee as a function of a closed position's hold, `max(base x (1 - discount),
    C(hold))` (#614, decision D7): `points` are the hold curve C's ((hold_s, fee share), ...) at
    rising holds, `base_bps` is `profit_fee_bps` and `discount` the volume tier's share off it.

    `interp="engine"` is the engine's table: C is the first fee at or below the first hold, linear
    between points below the last, rounded up to the bp, and zero at and past the last hold, where
    the discounted base alone applies. `interp="log"` is #623's family: linear in the logarithm of
    the hold between points, the first fee below them, the base past the last."""

    points: tuple
    base_bps: float = PROFIT_FEE_BPS
    discount: float = 0.0
    interp: str = "engine"

    def bps(self, hold_s):
        hold = np.asarray(hold_s, dtype=float)
        holds = np.array([h for h, _ in self.points], dtype=float)
        fees = np.array([f for _, f in self.points], dtype=float) * 1e4
        floor = self.base_bps * (1 - self.discount)
        if self.interp == "log":
            curve = np.interp(np.log(np.maximum(hold, 1e-3)), np.log(holds), fees, left=fees[0], right=floor)
        elif self.interp == "engine":
            curve = np.where(hold >= holds[-1], 0.0, np.ceil(np.interp(hold, holds, fees, left=fees[0]) - 1e-9))
        else:
            raise ValueError(f"unknown interpolation {self.interp}")
        return np.maximum(curve, floor)


def fee_bps_at(fee: HoldFee | None, hold_s):
    """The profit fee in bps at a hold of `hold_s` under `fee`; the standard fee with none."""
    return float(PROFIT_FEE_BPS) if fee is None else fee.bps(hold_s)
