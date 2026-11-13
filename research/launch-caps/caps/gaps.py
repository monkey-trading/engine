"""Paired positions held across session gaps, at each volume tier's leverage and pricing.

A pair is a long in one account and a short of the same notional in another. It leaves the house's
net exposure, and so its caps, where it was, and each leg's loss stops at its account's balance
(negative balance protection), so a gap past the balance is the house's while the other leg keeps
its gain less the profit fee. Opened at the last hour's close before a market closes and closed at
its reopening open, per unit of each leg's USD notional at entry, with gap `J` at the reopening:

    leg = max(pnl - 2 s - fee - financing, -(m + s)),   fee = f * max(pnl - 2 s, 0)

where `pnl` is the leg's price move in USD (`J` for the long, `-J` for the short, times the change
in the quote currency's USD value for a market quoted in another currency), `s` one fill's spread
(its first band: each leg stays within the account's best-price size), `m` the tier's initial
requirement (D5: the market's first band or the tier's floor, whichever is higher), `f` the profit
fee past the hold curve (the base less the tier's discount; every closure is longer than the curve's
15 minutes) and `financing` what the leg pays at the rollovers it crosses. The fee is on realised
PnL at the spread-adjusted fills. The engine admits a fill only if the account's equity after it,
at the mid, covers the requirement (`execution/mod.rs`, `check_risk`), so the least an account
holds is `m` plus its opening fill's spread, and that is the most it loses.
The pair's payoff is the sum of its legs, which is the house's loss on it; the house's bad debt is
the part of a leg's loss past its balance. Both legs close at the reopening's open, as a close-out
there would fill; the close-out level does not enter.

The closures are each market's in the cap study's hourly bars (`research/cap-calibration`, 102
weeks to 2026-10-12): a run of closed hours between two open ones, with the gap the bars record at
the reopening. The stress level at the reopening is taken as 0, the least the pair can pay.
"""

import numpy as np

CLASSES = ("fx", "index", "commodity")
KINDS = (("break", 0, 3), ("closure", 3, 40), ("weekend", 40, 10_000))  # (name, hours from, to) a closure lasts


def closures(is_open: np.ndarray) -> list[tuple[int, int]]:
    """Each closure of one market as (last open hour, reopening hour): a run of closed hours with an
    open hour on either side."""
    out, last = [], None
    for h, open_now in enumerate(is_open):
        if open_now:
            if last is not None and h - last > 1:
                out.append((last, h))
            last = h
    return out


def log_levels(bars: np.ndarray, column: int) -> tuple[np.ndarray, np.ndarray]:
    """A market's log open and close at each hour, from the bars' moves (`prices.build`); a closed
    hour carries the last close."""
    from model.prices import CLOSED

    b = bars[:, column].astype(np.float64)
    is_open = bars[:, column, 0] != CLOSED
    moves = np.where(is_open[:, None], b, 0.0) / 1e6
    close = np.cumsum(moves[:, 0] + moves[:, 3])
    prev = np.concatenate([[0.0], close[:-1]])
    return prev + moves[:, 0], close


def leg_results(pnl, s_open: float, s_close: float, margin: float, fee: float, financing) -> tuple[np.ndarray, np.ndarray]:
    """One leg's result per unit of notional and the house's bad debt on it: its PnL after its opening
    and closing fills' spreads, the fee on that and financing, losing at most its balance, the
    requirement `margin` plus the opening spread its equity had to cover (`check_risk`)."""
    realised = np.asarray(pnl, dtype=float) - s_open - s_close
    net = realised - fee * np.maximum(realised, 0.0) - np.asarray(financing, dtype=float)
    balance = margin + s_open
    return np.maximum(net, -balance), np.maximum(-net - balance, 0.0)


def pair_rows(bars: np.ndarray, symbols: list[str], specs: dict, fin_long: dict, fin_short: dict, tiers: dict, base_fee: float) -> list[dict]:
    """A row per (market, closure, tier) of the FX, index and commodity markets: the gap, the
    rollovers crossed, the pair's payoff and the house's bad debt, per unit of leg notional."""
    from model.prices import CLOSED
    from model.simulate import is_rollover

    rows = []
    for m, symbol in enumerate(symbols):
        spec = specs[symbol]
        if spec["asset_class"] not in CLASSES:
            continue
        is_open = bars[:, m, 0] != CLOSED
        log_open, log_close = log_levels(bars, m)
        quote = spec.get("quote_currency", {"currency": "usd"})
        fx_symbol = None if quote["currency"] == "usd" else next(s for s, x in specs.items() if x["id"] == quote["fx_market"])
        fx = None if fx_symbol in (None, symbol) else log_levels(bars, symbols.index(fx_symbol))[1]
        first = spec["margin_tiers"][0]["bps"]
        spread = spec["spread_tiers"][0]["tenth_bps"] / 1e5
        for last, reopen in closures(is_open):
            move = np.expm1(log_open[reopen] - log_close[last])
            # A market quoted in yen converts its PnL at USD-JPY's price at the fill (#98): USD-JPY
            # at its own reopening price, Japan 225 at USD-JPY's last close before its reopening.
            if fx_symbol == symbol:
                g = np.exp(-(log_open[reopen] - log_close[last]))
            elif fx is not None:
                g = np.exp(-(fx[reopen - 1] - fx[last]))
            else:
                g = 1.0
            hours = reopen - last - 1
            rolls = int(sum(is_rollover(h) for h in range(last + 1, reopen + 1)))
            kind = next(name for name, lo, hi in KINDS if lo <= hours < hi)
            for tier, t in tiers.items():
                margin = max(first, t.margin_floor_bps) / 1e4
                fee = base_fee * (1 - t.discount)
                long_leg, long_bad = leg_results(move * g, spread, spread, margin, fee, rolls * fin_long[symbol])
                short_leg, short_bad = leg_results(-move * g, spread, spread, margin, fee, rolls * fin_short[symbol])
                rows.append({"symbol": symbol, "asset_class": spec["asset_class"], "tier": tier, "kind": kind, "hour": last, "closed_hours": hours,
                             "rollovers": rolls, "gap": float(move), "margin": margin, "spread": spread, "fee": fee,
                             "payoff": float(long_leg + short_leg), "bad_debt": float(long_bad + short_bad),
                             "past_balance": bool(long_bad + short_bad > 0)})
    return rows


def summary(rows: list[dict], years: float) -> list[dict]:
    """Per (market, tier, closure kind and all kinds): closures a year, the mean absolute gap, the
    share of closures whose gap passes a leg's balance, and the pair's payoff and the house's bad debt
    a year, in bps of each leg's notional, with the payoff as a share of the margin the pair posts."""
    out = []
    keys = sorted({(r["symbol"], r["tier"]) for r in rows}, key=lambda k: (k[1], k[0]))
    for symbol, tier in keys:
        mine = [r for r in rows if r["symbol"] == symbol and r["tier"] == tier]
        for kind in ("all",) + tuple(k for k, _, _ in KINDS):
            sel = mine if kind == "all" else [r for r in mine if r["kind"] == kind]
            if not sel:
                continue
            pay = sum(r["payoff"] for r in sel) / years
            out.append({"symbol": symbol, "asset_class": sel[0]["asset_class"], "tier": tier, "kind": kind, "margin": sel[0]["margin"],
                        "closures_year": len(sel) / years, "mean_abs_gap_bps": float(np.mean([abs(r["gap"]) for r in sel]) * 1e4),
                        "past_balance_share": float(np.mean([r["past_balance"] for r in sel])),
                        "payoff_bps_year": pay * 1e4, "payoff_per_margin_year": pay / (2 * sel[0]["margin"]),
                        "payoff_bps_per_closure": pay * years / len(sel) * 1e4,
                        "bad_debt_bps_year": sum(r["bad_debt"] for r in sel) / years * 1e4,
                        "best_closure_bps": max(r["payoff"] for r in sel) * 1e4})
    return out


MARGIN_SWEEP_BPS = (50, 100, 150, 200, 300, 400, 500, 750, 1000)


def breakeven(bars, symbols, specs, fin_long, fin_short, base_fee: float, best_price_tier, kind: str = "weekend") -> dict:
    """Per market, the pair's payoff a year across closures of `kind` (bps of a leg) at each margin in
    `MARGIN_SWEEP_BPS` (the market's first band where it is higher), at `best_price_tier`'s fee, the
    least any tier pays; and the smallest of those margins at which the pair no longer pays."""
    from dataclasses import replace

    tiers = {bps: replace(best_price_tier, margin_floor_bps=bps) for bps in MARGIN_SWEEP_BPS}
    rows = [r for r in pair_rows(bars, symbols, specs, fin_long, fin_short, tiers, base_fee) if r["kind"] == kind]
    years = bars.shape[0] / 8760
    out = {}
    for symbol in dict.fromkeys(r["symbol"] for r in rows):
        pay = {bps: sum(r["payoff"] for r in rows if r["symbol"] == symbol and r["tier"] == bps) / years * 1e4 for bps in MARGIN_SWEEP_BPS}
        first = specs[symbol]["margin_tiers"][0]["bps"]
        out[symbol] = {"first_band_bps": first, "payoff_bps_year": pay,
                       "breakeven_margin_bps": next((bps for bps in MARGIN_SWEEP_BPS if bps >= first and pay[bps] <= 0), None)}
    return out
