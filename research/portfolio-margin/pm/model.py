"""The requirement under both rules, per the design on #913 (D1, D3, D5, D8, D9).

For one account, `N` is the signed USD notional of each position:

    PP = sum_m B_m(|N_m|)                    the market's bands, each band at least the tier floor
    L_g = F_g |sum_{m in g} beta_m N_m| + sum_{m in g} rho_m |N_m|
    M = sum_g L_g + sum_{m ungrouped} B_m(|N_m|) + K,    K = sum_m |N_m| c_m / level
    R = min(PP, max(phi PP, M)),   at least the tier floor on sum_m |N_m|

with `c_m` the market's close-spread ceiling. The shocks `F_g` and `rho_m` are the tail quantile of
the move over the horizon divided by the close-out level, so `level * M` covers the move (D2). At a
weekend gap's start the weekend values replace the shocks and the bands' weekend rates, and the
threshold is R in full.
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
    factor_weekend: float = 0.0
    rho_weekend: dict[str, float] = field(default_factory=dict)


@dataclass
class Params:
    groups: list[Group]
    phi: float
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


def model_term(specs, params: Params, book, floor: float = 0.0, weekend: bool = False) -> np.ndarray:
    gof = params.group_of()
    total = 0.0
    seen = set()
    for s, n in book.items():
        g = gof.get(s)
        if g is None:
            total = total + staircase(specs[s].bands, n, max(floor, specs[s].weekend if weekend else 0.0))
        elif g.name not in seen:
            seen.add(g.name)
            net = sum(g.beta[m] * book[m] for m in g.members if m in book)
            use_weekend = weekend and bool(g.rho_weekend)  # a group that never closes has none
            f = g.factor_weekend if use_weekend else g.factor
            rho = g.rho_weekend if use_weekend else g.rho
            total = total + f * np.abs(net) + sum(rho[m] * np.abs(book[m]) for m in g.members if m in book)
    return total + closing_cost(specs, book, params.level)


def requirement(specs, params: Params, book, tier_floor: float = 0.0, weekend: bool = False) -> np.ndarray:
    """R at the initial requirement's bands (tier floor) or, with `weekend`, over a weekend gap."""
    pp = per_position(specs, book, tier_floor, weekend)
    m = model_term(specs, params, book, tier_floor, weekend)
    r = np.minimum(pp, np.maximum(params.phi * pp, m))
    gross = sum(np.abs(n) for n in book.values())
    return np.maximum(r, tier_floor * gross) if tier_floor else r
