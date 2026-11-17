"""The requirement under both rules, per the design on #913 as revised (D5, 2026-11-17).

For one account, `N` is the signed USD notional of each position. Within a group, each position's
loading-weighted exposure is `x_m = beta_m N_m`; `L` is the sum of the positive ones and `S` the sum of
the absolute negative ones. The side with the larger total is matched only in part:

    matched notional of m:  f_m |N_m|,   f_m = min(1, other side's total / own side's total)
    gross matched exposure = sum_m |x_m| f_m = 2 min(L, S) = sum_m |x_m| - |sum_m x_m|   (when net is nonzero,
                              the gross loading-weighted exposure less the absolute value of its net)

A lone position, or one whose group has nothing on the other side, has f = 0 and keeps its requirement.
With PP_m the market's bands on the whole position (each band at least the tier floor):

    unmatched part:  (1 - f_m) PP_m                         per-position margin, exactly
    matched part:    C_g = min(P_g, max(phi_g P_g, M_g)),    P_g = sum_m f_m PP_m
    M_g = sum_m rho_m beta_m |Nt_m| + sum_m |Nt_m| c_m / level,   Nt_m = f_m N_m   (rho per unit of loading-weighted exposure)
    R = sum over ungrouped PP_m + sum over groups (sum_m (1 - f_m) PP_m + C_g),  at least the tier floor on gross

with `c_m` the close-spread ceiling. The shocks `F_g` and `rho_m` are the tail quantile of the move over
the horizon divided by the close-out level, so `level * M` covers the move (D2). With the matched part
loading-neutral by construction the factor term is zero. At a weekend gap's start the weekend values and
rates replace the shocks and bands, and the threshold is R in full.
"""

from dataclasses import dataclass, field

import numpy as np

from pm.common import genesis


@dataclass
class Spec:
    symbol: str
    asset_class: str
    bands: list[tuple[float, float]]  # (up_to in USD or inf, rate)
    weekend: float  # weekend margin rate
    ceiling: float  # close-spread ceiling, as a rate
    spreads: list[tuple[float, float]]  # (up_to, rate) of a fill's spread bands


def load_specs() -> dict[str, Spec]:
    g = genesis()
    ceil = {c["market"]: c["max_close_spread_bps"] / 1e4 for c in g["close_spread_ceilings"]}
    out = {}
    for m in g["markets"]:
        bands = [(float(t["up_to"]) if "up_to" in t else np.inf, t["bps"] / 1e4) for t in m["margin_tiers"]]
        spreads = [(float(t["up_to"]) if "up_to" in t else np.inf, t["tenth_bps"] / 1e5) for t in m["spread_tiers"]]
        out[m["symbol"]] = Spec(m["symbol"], m["asset_class"], bands, m.get("weekend_margin_bps", 0) / 1e4, ceil[m["id"]], spreads)
    return out


def staircase(bands: list[tuple[float, float]], n: np.ndarray, floor: float = 0.0) -> np.ndarray:
    """Sum of each band's slice of |n| times the band's rate, raised to at least `floor`."""
    n = np.abs(np.asarray(n, dtype=float))
    out, lo = np.zeros_like(n), 0.0
    for up_to, rate in bands:
        out += np.clip(n - lo, 0, up_to - lo) * max(rate, floor)
        lo = up_to
    return out


def spread_cost(spec: Spec, n: np.ndarray) -> np.ndarray:
    """What a fill of |n| pays at the market's spread bands (a size's slices each at its band)."""
    return staircase(spec.spreads, n)


@dataclass
class Group:
    name: str
    members: list[str]
    beta: dict[str, float]
    factor: float  # F_g
    rho: dict[str, float]
    phi: float = 1.0  # this group's floor share
    factor_weekend: float = 0.0
    rho_weekend: dict[str, float] = field(default_factory=dict)


@dataclass
class Params:
    groups: list[Group]
    phi: float | None = None  # overrides every group's floor share (for scans)
    level: float = 0.5

    def group_of(self) -> dict[str, Group]:
        return {m: g for g in self.groups for m in g.members}


def per_position(specs, book: dict[str, np.ndarray], floor: float = 0.0, weekend: bool = False) -> np.ndarray:
    total = 0.0
    for s, n in book.items():
        sp = specs[s]
        total = total + staircase(sp.bands, n, max(floor, sp.weekend if weekend else 0.0))
    return total


def closing_cost(specs, book, level: float) -> np.ndarray:
    return sum(np.abs(n) * specs[s].ceiling for s, n in book.items()) / level


def _group_arrays(g: Group, book, weekend: bool):
    x = {m: g.beta[m] * book[m] for m in g.members if m in book}
    pos = sum(np.maximum(v, 0.0) for v in x.values())
    neg = sum(np.maximum(-v, 0.0) for v in x.values())
    with np.errstate(divide="ignore", invalid="ignore"):
        f_pos = np.where(pos > 0, np.minimum(1.0, neg / np.where(pos > 0, pos, 1.0)), 0.0)
        f_neg = np.where(neg > 0, np.minimum(1.0, pos / np.where(neg > 0, neg, 1.0)), 0.0)
    return {m: np.where(v > 0, f_pos, np.where(v < 0, f_neg, 0.0)) for m, v in x.items()}


def matched_exposure(g: Group, book) -> np.ndarray:
    """Gross loading-weighted exposure less the absolute value of its net, over the group's members."""
    x = [g.beta[m] * book[m] for m in g.members if m in book]
    return sum(np.abs(v) for v in x) - np.abs(sum(x))


def model_term(specs, params: Params, book, floor: float = 0.0, weekend: bool = False) -> np.ndarray:
    """The grid and closing costs on the whole book, grouped markets as one (used for the bounds)."""
    gof = params.group_of()
    total = 0.0
    seen = set()
    for s, n in book.items():
        g = gof.get(s)
        if g is None:
            total = total + staircase(specs[s].bands, n, max(floor, specs[s].weekend if weekend else 0.0))
        elif g.name not in seen:
            seen.add(g.name)
            total = total + _grid(g, book, weekend)
    return total + closing_cost(specs, book, params.level)


def _grid(g: Group, book, weekend: bool):
    """The residual shocks on the loading-weighted exposures. The factor shock is gone: the matched part's net is zero."""
    use_weekend = weekend and bool(g.rho_weekend)  # a group that never closes has none
    rho = g.rho_weekend if use_weekend else g.rho
    return sum(rho[m] * np.abs(g.beta[m] * book[m]) for m in g.members if m in book)


def requirement(specs, params: Params, book, tier_floor: float = 0.0, weekend: bool = False) -> np.ndarray:
    """R at the initial requirement's bands (tier floor) or, with `weekend`, over a weekend gap."""
    gof = params.group_of()
    total, seen, gross = 0.0, set(), 0.0
    for s, n in book.items():
        gross = gross + np.abs(n)
        if s not in gof:
            # over a weekend gap only a market with a weekend rate has a requirement today (V60)
            if not weekend or specs[s].weekend > 0:
                total = total + staircase(specs[s].bands, n, max(tier_floor, specs[s].weekend if weekend else 0.0))
    for g in params.groups:
        if not any(m in book for m in g.members) or g.name in seen:
            continue
        seen.add(g.name)
        f = _group_arrays(g, book, weekend)
        phi = g.phi if params.phi is None else params.phi
        matched = {m: f[m] * book[m] for m in f}
        pp_m, pp_u = 0.0, 0.0
        for m in f:
            if weekend and specs[m].weekend <= 0:
                continue  # no requirement over a weekend gap today (V60), for the matched part too: R never exceeds it
            pp = staircase(specs[m].bands, book[m], max(tier_floor, specs[m].weekend if weekend else 0.0))
            pp_m = pp_m + f[m] * pp
            pp_u = pp_u + (1.0 - f[m]) * pp
        grid = _grid(g, matched, weekend) + closing_cost(specs, matched, params.level)
        total = total + pp_u + np.minimum(pp_m, np.maximum(phi * pp_m, grid))
    return np.maximum(total, tier_floor * gross) if tier_floor else total
