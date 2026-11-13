"""The launch pricing bundle as each volume tier pays it, in the terms of the cap study's model.

A volume tier changes how much size an account trades at the best price, how much leverage it takes
on new risk, and the fee past the hold curve, never a spread rate (#614, decisions D1, D5 and D7;
F5 to F7):

| Tier | Best-price size (F5) | Initial margin floor (D5) | Discount on the fee's base (D7, F7) |
|---|---|---|---|
| 0 | a quarter of the first spread bound | 5% (20x) | none |
| 1 | half of it | 2% (50x) | none |
| 2 | the first bound | none | 10% |
| 3 | two and a half times it | none | 20% |

- **Spreads.** The account's first spread band ends at its tier's share of the market's first bound,
  at most the second; the rates are the genesis's, in tenths of a bp (V52, D6). Every fill pays the
  bands' staircase on the slice of the position it opens or closes (D2), so a position opened and
  closed in one order pays its staircase twice.
- **The closing clamp.** A close pays at most its market's ceiling (V40, D10). At stress level 0 no
  band reaches a ceiling (`check_ceilings`), so the clamp never binds in a model that prices at level
  0; at a higher level a fill pays at least its level-0 charge, clamp included, so pricing every fill
  at level 0 is a lower bound on spread income.
- **The fee** is `max(base x (1 - discount), C(hold))` on the engine's table of the owner's curve:
  100% to 10 s, falling to 10% at 15 minutes (#614, 2026-11-13), at the launch base.
- **Leverage.** The margin floor sets the initial requirement only, for new risk and withdrawals;
  close-outs keep the market's bands (D5). Tier 0's floor makes every market that allows more than
  20x a flat 5% schedule, whose client-flow profiles the derived data lacks: `with_flat_schedule`
  approximates them from the 10x schedule's.
"""

from dataclasses import dataclass, replace

import numpy as np

from caps.common import use_spread


@dataclass(frozen=True)
class Tier:
    best_price: float  # the first spread bound's share at which the account's first band ends
    margin_floor_bps: int  # the initial requirement's floor; 0 = the market's bands
    discount: float  # the share off the fee's base


TIERS = {0: Tier(0.25, 500, 0.0), 1: Tier(0.5, 200, 0.0), 2: Tier(1.0, 0, 0.10), 3: Tier(2.5, 0, 0.20)}
CURVE = (10.0, 900.0)  # the full fee held to 10 s, 10% at 15 minutes (#614, 2026-11-13)


def fee_points() -> tuple:
    """The owner's hold curve as the engine's table (`calib.fee.curve_points`)."""
    use_spread()
    from calib.fee import curve_points

    return curve_points(*CURVE)


def fee_schedule(tier: int, params: dict):
    """The tier's profit fee as the cap model charges it (`accounting.HoldFee`)."""
    from model.accounting import HoldFee

    return HoldFee(points=fee_points(), base_bps=params["profit_fee_bps"], discount=TIERS[tier].discount)


def spread_bands(spec: dict, tier: int) -> tuple:
    """A market's spread bands for an account at `tier`, ((up_to, bps), ..., (None, bps)): the
    first ends at the tier's share of the first bound, at most the second (F5)."""
    tiers = spec["spread_tiers"]
    bands = [(None if "up_to" not in t else float(t["up_to"]), t["tenth_bps"] / 10) for t in tiers]
    if len(bands) > 1:
        first = bands[0][0] * TIERS[tier].best_price
        second = bands[1][0]
        bands[0] = (first if second is None else min(first, second), bands[0][1])
        if second is not None and bands[0][0] >= second:
            bands = [(second, bands[0][1])] + bands[2:]
    return tuple(bands)


def check_ceilings(genesis: dict) -> dict[str, tuple[float, int]]:
    """Each market's widest level-0 band and its close-spread ceiling, in bps. Raises if a band
    reaches its ceiling, where a close at level 0 would be clamped and this model's spreads would
    overstate what the house earns."""
    ceilings = {c["market"]: c["max_close_spread_bps"] for c in genesis["close_spread_ceilings"]}
    levels = genesis["params"]["stress_levels"]
    if levels[0]["min_spread_bps"] != 0 or levels[0]["size_spread_multiplier_bps"] != 10_000:
        raise ValueError("stress level 0 is not the bands alone")
    out = {}
    for m in genesis["markets"]:
        widest = max(t["tenth_bps"] for t in m["spread_tiers"]) / 10
        ceiling = ceilings.get(m["id"])
        if ceiling is not None and widest >= ceiling:
            raise ValueError(f"{m['symbol']}: a level-0 band of {widest} bps reaches its {ceiling} bps ceiling")
        out[m["symbol"]] = (widest, ceiling)
    return out


def initial_bands(margin_tiers: list[dict], floor_bps: int) -> list[dict]:
    """A market's margin bands for new risk under a tier's floor (D5), adjacent equal bands merged."""
    out = []
    for t in margin_tiers:
        band = {"bps": max(t["bps"], floor_bps)} | ({"up_to": t["up_to"]} if "up_to" in t else {})
        if out and out[-1]["bps"] == band["bps"]:
            out[-1] = band  # the merged band runs to the later band's bound
        else:
            out.append(band)
    return out


def bin_leverage(flow_rows: list[dict]) -> dict:
    """Per (kind, leverage bin label), papertrade's margin-weighted mean leverage, E[L]: the bin's
    notional over its margin, from `papertrade_flow.csv`."""
    out = {}
    for r in flow_rows:
        n, m = out.get((r["kind"], r["leverage_bin"]), (0.0, 0.0))
        out[(r["kind"], r["leverage_bin"])] = (n + float(r["notional_usd"]), m + float(r["margin_usd"]))
    return {k: n / m for k, (n, m) in out.items()}


def straddling_leverage(source_ratio: float, source_max: float, target_max: float, bin_high: float, mean: float) -> float:
    """E[min(L, target_max)] for a papertrade leverage bin whose leverage runs up to `bin_high`, from
    what it buys on the source schedule, E[min(L, source_max)] = `source_ratio`, and its mean, E[L].
    `g(c) = E[min(L, c)]` is concave and reaches E[L] at the bin's top, so the chord from source_max
    to the top bounds it from below; `min(L, c) <= c / source_max * min(L, source_max)` and
    `g(c) <= E[L]` from above. The estimate is the midpoint."""
    if not source_max < target_max < bin_high:
        raise ValueError("the target's maximum must lie between the source's and the bin's top")
    lower = source_ratio + (mean - source_ratio) * (target_max - source_max) / (bin_high - source_max)
    upper = min(source_ratio * target_max / source_max, mean, target_max)
    return (lower + upper) / 2


def with_flat_schedule(flow, rate_bps: int, source: list[dict], means: dict, labels: list[str]):
    """`flow` with `capped` profiles for one band at `rate_bps` (key `str(rate_bps)`), approximated
    from those of the bands `source`, whose first rate is higher. `means` is `bin_leverage`'s, and
    `labels` the leverage bins in the flow's bucket order.

    Leverage: a bin whose every position's leverage is at least the new maximum buys exactly it per
    margin, with no band to cut it; the bin that straddles the maximum buys `straddling_leverage`.
    Sizes: a position no larger than the source's first bound sat at the source's maximum or below
    it, so its notional scales by the kind's change in notional per margin; a larger one was cut by
    the source's bands, and its margin buys the new maximum per margin."""
    from model.flow import margin_for_notional, schedule_key

    key, src = str(rate_bps), schedule_key(source)
    cap, src_cap = 1e4 / rate_bps, 1e4 / source[0]["bps"]
    if cap <= src_cap:
        raise ValueError("the flat schedule must allow more leverage than the source's first band")
    sizes, leverage = dict(flow.sizes), dict(flow.leverage)
    for kind in ("directional", "hedged"):
        old = flow.leverage[(kind, "capped", src)]
        new = old.copy()
        for b, label in enumerate(labels):
            lo, hi = (float(x) for x in label.split("-"))
            new[b] = cap if lo >= cap else straddling_leverage(old[b], src_cap, cap, hi, means[(kind, label)])
        margin = np.array([sum(c.margin_share for c in flow.classes if c.kind == kind and c.bucket == b) for b in range(len(labels))])
        scale = float((margin * new).sum() / (margin * old).sum())
        bounds, shares = flow.sizes[(kind, "capped", src)]
        moved = np.where(bounds <= float(source[0]["up_to"]), bounds * scale, cap * margin_for_notional(bounds, source))
        sizes[(kind, "capped", key)] = (np.maximum.accumulate(moved), shares)
        leverage[(kind, "capped", key)] = new
    return replace(flow, sizes=sizes, leverage=leverage)


def with_tier(markets, flow, specs: list[dict], tier: int, means: dict, labels: list[str]):
    """The cap model's markets and flow for accounts at `tier`: each market's schedule key is its
    initial-margin bands under the tier's floor, with profiles for any new schedule; the close-out
    keeps each market's first band (`markets.margin_rate`)."""
    from model.flow import schedule_key

    floor = TIERS[tier].margin_floor_bps
    if floor == 0:
        return markets, flow
    by_symbol = {s["symbol"]: s for s in specs}
    keys = []
    for symbol, key in zip(markets.symbols, markets.schedule):
        bands = initial_bands(by_symbol[symbol]["margin_tiers"], floor)
        if schedule_key(bands) == key:
            keys.append(key)
            continue
        if len(bands) != 1:
            raise NotImplementedError(f"{symbol}: tier {tier}'s bands {schedule_key(bands)} have no client-flow profile")
        if ("directional", "capped", schedule_key(bands)) not in flow.sizes:
            source = max((s["margin_tiers"] for s in specs if s["margin_tiers"][0]["bps"] > floor and "up_to" in s["margin_tiers"][0]),
                         key=lambda t: (-t[0]["bps"], float(t[0]["up_to"])))
            flow = with_flat_schedule(flow, floor, source, means, labels)
        keys.append(schedule_key(bands))
    return replace(markets, schedule=keys), flow
