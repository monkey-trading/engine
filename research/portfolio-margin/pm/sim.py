"""An account-level simulator on the cap study's hourly bars: books held through a week, valued under
both rules.

Each account opens in one group's members at `N0` (signed USD notionals) with equity equal to its
initial requirement at the tier's floor under the rule, so both rules hold the same positions and
the portfolio rule's looser requirement shows as leverage. Each hour the members are open it is
marked at the hour's close (the gap and the in-bar move) and, if its equity is below the close-out
level times the requirement at the market's bands (D6's threshold on a price move), closed whole:
a group closes together, and with one group in the account that is the least that restores the level.
At the last open hour before a closure of 40 hours or more (a weekend gap's start) the threshold is
the requirement in full at the weekend values (D9). A close pays the fill spreads (the bands of
`Spec.spreads`, at the notional's size) and the profit fee on each leg's realised gain, 10%, at
the marks. An account that survives the week closes at the end on the same terms.

An account's equity cannot fall below zero: the loss past it is the house's (negative balance
protection), so the house's result is the client's loss, capped at its balance, with the spreads and
fees the client paid. Only the hour's close is read: a move inside an hour that the close does not
show goes unseen by both rules alike.
"""

import numpy as np

from pm import model

FEE = 0.10
WEEK = 168


class Window:
    """Returns, open hours and weekend-gap starts of a set of members over the whole history."""

    def __init__(self, hr, members: list[str]):
        from caps.gaps import closures

        from pm.hourly import CLOSED

        self.members = list(members)
        cols = [hr.index[m] for m in members]
        b = hr.bars[:, cols]
        self.open = (b[..., 0] != CLOSED)
        self.ret = np.where(self.open, b[..., 0].astype(float) + b[..., 3], 0.0) / 1e6
        self.all_open = self.open.all(axis=1)
        gap_start = np.zeros(hr.hours, bool)
        for last, reopen in closures(self.all_open):
            if reopen - last - 1 >= 40:
                gap_start[last] = True
        self.gap_start = gap_start
        self.starts = np.flatnonzero(self.all_open[: hr.hours - WEEK - 1])


def simulate(w: Window, specs, params, rule: str, members: list[str], n0: np.ndarray, start: np.ndarray, tier_floor: float, level: float = 0.5) -> dict:
    """`n0` (accounts, members); `start` (accounts,) the opening hours. `rule` is 'bands' or 'portfolio'."""
    a, k = n0.shape

    def req(n, floor, weekend=False):
        book = {m: n[:, i] for i, m in enumerate(members)}
        if rule == "bands":
            if weekend:  # today: only a market with a weekend rate has a gap-start check (V60)
                return sum(model.staircase(specs[m].bands, book[m], specs[m].weekend) for m in members if specs[m].weekend > 0)
            return model.per_position(specs, book, floor)
        return model.requirement(specs, params, book, floor, weekend)

    e0 = req(n0, tier_floor)
    gross0 = np.abs(n0).sum(axis=1)
    cum = np.zeros((a, k))
    alive = np.ones(a, bool)
    closed_out = np.zeros(a, bool)
    final = np.zeros(a)
    for t in range(1, WEEK + 1):
        idx = start + t
        cum += w.ret[idx]
        n = n0 * np.exp(cum)
        eq = e0 + (n - n0).sum(axis=1)
        check = alive & w.all_open[idx]
        thr = level * req(n, 0.0)
        gs = w.gap_start[idx]
        thr = np.where(gs, np.maximum(thr, req(n, 0.0, weekend=True)), thr)
        hit = check & (eq < thr)
        if hit.any():
            final[hit] = _close(specs, members, n[hit], n0[hit], eq[hit])
            closed_out |= hit
            alive &= ~hit
    # the week's end: whoever is left closes on the same terms
    n = n0 * np.exp(cum)
    eq = e0 + (n - n0).sum(axis=1)
    final[alive] = _close(specs, members, n[alive], n0[alive], eq[alive])
    result = np.maximum(final, 0.0)  # the balance after the close; the rest is the house's
    return {
        "e0": e0, "gross": gross0, "closed_out": closed_out,
        "past_balance": np.maximum(-final, 0.0), "house": -(result - e0),
    }


def _close(specs, members, n, n0, eq):
    """Equity after closing every leg: the fill spreads and the profit fee on each leg's gain."""
    cost = sum(model.spread_cost(specs[m], n[:, i]) for i, m in enumerate(members))
    gains = sum(np.maximum((n[:, i] - n0[:, i]) - model.spread_cost(specs[m], n[:, i]), 0.0) for i, m in enumerate(members))
    return eq - cost - FEE * gains


def simulate_shut(w: Window, specs, params, rule: str, members: list[str], n0: np.ndarray, start: np.ndarray, tier_floor: float, shut_hours: int, level: float = 0.5) -> dict:
    """As `simulate` for a pair, with the second member shut for its first `shut_hours` hours: its mark stays
    at the last, its moves arrive at the end of the outage, and while it is shut only the first leg can close
    (D10: the rest close by the normal rule, with no protection of the hedge, and the shut leg waits)."""
    a, k = n0.shape
    assert k == 2

    def req(n, alive, floor, weekend=False):
        book = {m: n[:, i] * alive[:, i] for i, m in enumerate(members)}
        if rule == "bands":
            if weekend:
                return sum(model.staircase(specs[m].bands, book[m], specs[m].weekend) for m in members if specs[m].weekend > 0)
            return model.per_position(specs, book, floor)
        return model.requirement(specs, params, book, floor, weekend)

    full = np.ones((a, k))
    e0 = req(n0, full, tier_floor)
    gross0 = np.abs(n0).sum(axis=1)
    cum = np.zeros((a, k))
    alive = np.ones((a, k))
    cash = np.zeros(a)
    missed = np.zeros(a)
    closed_out = np.zeros(a, bool)
    for t in range(1, WEEK + 1):
        idx = start + t
        r = w.ret[idx].copy()
        if t <= shut_hours:
            missed += r[:, 1]
            r[:, 1] = 0.0
        elif t == shut_hours + 1:
            r[:, 1] += missed
        cum += r
        n = n0 * np.exp(cum)
        unreal = ((n - n0) * alive).sum(axis=1)
        eq = e0 + cash + unreal
        shut = t <= shut_hours
        anyalive = alive.sum(axis=1) > 0
        check = anyalive & w.all_open[idx] if not shut else anyalive & w.open[idx, 0]
        thr = level * req(n, alive, 0.0)
        gs = w.gap_start[idx]
        thr = np.where(gs & ~shut, np.maximum(thr, req(n, alive, 0.0, weekend=True)), thr)
        hit = check & (eq < thr)
        if hit.any():
            for i, m in enumerate(members):
                if shut and i == 1:
                    continue
                sel = hit & (alive[:, i] > 0)
                if sel.any():
                    leg = (n[sel, i] - n0[sel, i]) - model.spread_cost(specs[m], n[sel, i])
                    cash[sel] += (n[sel, i] - n0[sel, i]) - model.spread_cost(specs[m], n[sel, i]) - FEE * np.maximum(leg, 0.0)
                    alive[sel, i] = 0
                    closed_out |= sel
    if shut_hours >= WEEK:  # the shut leg's moves arrive when the week ends
        cum[:, 1] += missed
    n = n0 * np.exp(cum)
    for i, m in enumerate(members):
        sel = alive[:, i] > 0
        leg = (n[sel, i] - n0[sel, i]) - model.spread_cost(specs[m], n[sel, i])
        cash[sel] += leg - FEE * np.maximum(leg, 0.0)
    final = e0 + cash
    return {"e0": e0, "gross": gross0, "closed_out": closed_out, "past_balance": np.maximum(-final, 0.0), "house": -(np.maximum(final, 0.0) - e0)}
