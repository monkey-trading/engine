"""Trader flow: papertrade.xyz's measured behaviour, carried over to the launch markets.

Flow is carried over by margin (the collateral traders commit), not by notional, because the
launch markets allow far less leverage than papertrade's 1000x. Each papertrade flow class (kind,
hold class, leverage bin) brings its share of daily margin to every market in proportion to the
market mix. Each position's leverage is mapped onto the market's first margin tier:
- `capped`: the papertrade leverage, capped at the tier's maximum (most flow lands on the cap);
- `relative`: the same fraction of the maximum as of papertrade's 1000x;
and its notional is then what its margin buys under the market's margin bands, which charge each
slice of notional at its band's rate (`market.rs`, `staircase`).
"""

import csv
import json
from dataclasses import dataclass, replace
from pathlib import Path

import numpy as np

PAPERTRADE_MAX_LEVERAGE = 1000


@dataclass
class FlowClass:
    kind: str  # "directional" or "hedged"
    hold_h: int  # 0 = churn, opened and closed within the hour
    bucket: int  # papertrade leverage bin
    margin_share: float  # share of all papertrade margin
    mean_hold_h: float
    mean_sqrt_hold_h: float
    long_share: float
    min_hold_s: float = 0.0  # no hold in the class is shorter (the target flow's kept part is held 15 minutes or more)


@dataclass
class Flow:
    classes: list[FlowClass]
    leverage: dict  # (kind, mapping, margin schedule) -> each leverage bin's notional over margin
    daily_margin: np.ndarray  # USD of margin opened per full papertrade day
    sizes: dict  # (kind, mapping, margin schedule) -> (notional bounds, share of margin at or below each)
    long_share_ar1: float
    long_share_resid_sd: float
    long_share_slope: float  # on the previous hour's return
    holding: tuple = ()  # ((hold_from_s, hold_to_s, notional share), ...): every closed position's holds


def schedule_key(tiers: list[dict]) -> str:
    """A margin band schedule as text, such as `1000@1000000,2000`: each band's bps and upper bound."""
    return ",".join(f"{t['bps']}@{t['up_to']}" if "up_to" in t else str(t["bps"]) for t in tiers)


def banded_notional(margin, tiers: list[dict]):
    """The notional that `margin` covers when each slice of notional is charged its band's rate."""
    edges = [0.0] + [float(t["up_to"]) for t in tiers if "up_to" in t]
    rates = [t["bps"] / 1e4 for t in tiers]
    costs = np.cumsum([0.0] + [(hi - lo) * r for lo, hi, r in zip(edges, edges[1:], rates)])
    margin = np.asarray(margin, dtype=float)
    within = np.interp(margin, costs, edges)
    return np.where(margin > costs[-1], edges[-1] + (margin - costs[-1]) / rates[-1], within)


def margin_for_notional(notional, tiers: list[dict]):
    """The margin that buys `notional` under the bands: the inverse of `banded_notional`."""
    edges = [0.0] + [float(t["up_to"]) for t in tiers if "up_to" in t]
    rates = [t["bps"] / 1e4 for t in tiers]
    costs = np.cumsum([0.0] + [(hi - lo) * r for lo, hi, r in zip(edges, edges[1:], rates)])
    notional = np.asarray(notional, dtype=float)
    return np.where(notional > edges[-1], costs[-1] + (notional - edges[-1]) * rates[-1], np.interp(notional, edges, costs))


def with_schedule(flow: Flow, source: list[dict], tiers: list[dict]) -> Flow:
    """`flow` with the `capped` size and leverage profiles of the bands `tiers` when it has none,
    approximated from those of the bands `source`, which must have the same first-tier rate.

    Both bands price a notional up to their first bound alike, so a position whose notional on
    `source` is at most the first bound of `tiers` keeps it. A larger one is taken to be bound
    by its bands, not by its own leverage: its margin is what buys that notional on `source`, and
    its notional on `tiers` is what that margin buys there. The leverage profile scales the two
    higher bins by one factor, so the notional per margin changes as the sizes' does, up to the
    maximum leverage.
    The exact profile needs the papertrade positions (`derive-papertrade` with the bands among the
    launch schedules), and replaces this when present."""
    key, src_key = schedule_key(tiers), schedule_key(source)
    if tiers[0]["bps"] != source[0]["bps"]:
        raise ValueError("the bands must share a first-tier rate")
    if any((k, "capped", key) in flow.sizes for k in ("directional", "hedged")):
        return flow
    sizes, leverage = dict(flow.sizes), dict(flow.leverage)
    keep_below = float(tiers[0]["up_to"])
    for kind in ("directional", "hedged"):
        if (kind, "capped", src_key) not in flow.sizes:
            continue
        bounds, shares = flow.sizes[(kind, "capped", src_key)]
        moved = np.where(bounds <= keep_below, bounds, banded_notional(margin_for_notional(bounds, source), tiers))
        sizes[(kind, "capped", key)] = (np.maximum.accumulate(moved), shares)
        moved = np.maximum.accumulate(moved)
        # Each size cell's notional is its margin share times the notional its margin buys on the
        # source bands; the profile scales by how much that notional changes on `tiers`.
        buys = bounds / np.maximum(margin_for_notional(bounds, source), 1e-12)
        notional = np.diff(shares, prepend=0.0) * buys
        change = float((notional * moved / bounds).sum() / notional.sum())
        ratio = leverage[(kind, "capped", src_key)].copy()
        by_bin = np.array([sum(c.margin_share for c in flow.classes if c.kind == kind and c.bucket == b) for b in range(len(ratio))])
        total = float((by_bin * ratio).sum())
        low = by_bin[0] * ratio[0]
        scale = (total * change - low) / (total - low) if total > low else 1.0
        ratio[1:] = np.minimum(ratio[1:] * scale, 1e4 / tiers[0]["bps"])  # no more than the maximum leverage
        leverage[(kind, "capped", key)] = ratio
    return replace(flow, sizes=sizes, leverage=leverage)


def with_genesis_profiles(flow: Flow, specs: list[dict]) -> Flow:
    """`flow` with `capped` profiles for every genesis margin schedule that has none, each
    approximated (`with_schedule`) from a schedule with profiles and the same first-tier rate.
    BTC-USD's and ETH-USD's bands (1% to $2M, 2% to $10M, 5% above) have no profile of their own in
    the derived data, and a market without one would open nothing, which would read as safe."""
    def profiled(tiers):
        key = schedule_key(tiers)
        return all((kind, "capped", key) in flow.sizes and (kind, "capped", key) in flow.leverage for kind in ("directional", "hedged"))

    for spec in specs:
        tiers = spec["margin_tiers"]
        if profiled(tiers):
            continue
        source = next((s["margin_tiers"] for s in specs if s["margin_tiers"][0]["bps"] == tiers[0]["bps"] and profiled(s["margin_tiers"])), None)
        if source is None:
            raise ValueError(f"{spec['symbol']}: no profiled schedule shares its first-tier rate")
        flow = with_schedule(flow, source, tiers)
    return flow


def load(derived: Path) -> Flow:
    with (derived / "papertrade_flow.csv").open() as fh:
        rows = list(csv.DictReader(fh))
    total = sum(float(r["margin_usd"]) for r in rows)
    bins = sorted({r["leverage_bin"] for r in rows}, key=lambda b: int(b.split("-")[0]))
    classes = [
        FlowClass(
            kind=r["kind"],
            hold_h=int(r["hold_class_h"]),
            bucket=bins.index(r["leverage_bin"]),
            margin_share=float(r["margin_usd"]) / total,
            mean_hold_h=float(r["mean_hold_h"]),
            mean_sqrt_hold_h=float(r["mean_sqrt_hold_h"]),
            long_share=float(r["long_share"]),
        )
        for r in rows
    ]
    with (derived / "papertrade_daily.csv").open() as fh:
        daily = [float(r["opened_margin_usd"]) for r in csv.DictReader(fh) if r["full_day"] == "True"]
    summary = json.loads((derived / "papertrade_summary.json").read_text())
    with (derived / "papertrade_leverage.csv").open() as fh:
        leverage_rows = list(csv.DictReader(fh))
    leverage = {}
    for r in leverage_rows:
        by_bin = leverage.setdefault((r["kind"], r["mapping"], r["schedule"]), np.ones(len(bins)))
        by_bin[bins.index(r["leverage_bin"])] = float(r["notional_usd"]) / float(r["margin_usd"])
    with (derived / "papertrade_sizes.csv").open() as fh:
        size_rows = list(csv.DictReader(fh))
    sizes = {}
    for r in size_rows:
        bounds, shares = sizes.setdefault((r["kind"], r["mapping"], r["schedule"]), ([], []))
        bounds.append(float(r["notional_upto_usd"]))
        shares.append(float(r["margin_share"]))
    sizes = {k: (np.array(b), np.array(v)) for k, (b, v) in sizes.items()}
    with (derived / "papertrade_holding.csv").open() as fh:
        holding = tuple((float(r["hold_from_s"]), float(r["hold_to_s"]), float(r["notional_share"])) for r in csv.DictReader(fh))
    return Flow(
        classes=classes,
        leverage=leverage,
        daily_margin=np.array(daily),
        sizes=sizes,
        long_share_ar1=summary["long_share_ar1"],
        long_share_resid_sd=summary["long_share_resid_sd"],
        long_share_slope=summary["long_share_slope_on_prev_hour_return"],
        holding=holding,
    )


def fit_share(room, sizes: tuple[np.ndarray, np.ndarray]):
    """Share of margin in orders whose notional is at most `room`, the largest order the engine
    fills on that side (`accounting.order_room`); with no room nothing opens."""
    bounds, shares = sizes
    room = np.maximum(np.asarray(room, dtype=float), 0.0)
    fitted = np.interp(np.log(np.maximum(room, 1e-12)), np.log(bounds), shares, left=0.0, right=1.0)
    return np.where(room > 0, fitted, 0.0)


def spread_table(sizes: tuple[np.ndarray, np.ndarray], bands) -> tuple[np.ndarray, np.ndarray, float]:
    """A size profile priced through size bands (`accounting.staircase_rate`): per size bound, the
    sum over the orders at or below it of their margin share times the spread, in bps, one fill of
    each pays; and the profile's total share. Each cell of the profile pays the rate at its lower
    bound, the least any order in it pays, since a band's rate never falls with size."""
    from model.accounting import staircase_rate

    bounds, shares = sizes
    lower = np.concatenate([[0.0], bounds[:-1]])
    return bounds, np.cumsum(np.diff(shares, prepend=0.0) * staircase_rate(lower, bands)), float(shares[-1])


def fit_spread(room, table: tuple[np.ndarray, np.ndarray, float]):
    """The spread table's sum over the orders whose notional is at most `room`, as `fit_share` sums
    their margin share; their mean spread, in bps, is this over `fit_share`."""
    bounds, paid, _ = table
    room = np.maximum(np.asarray(room, dtype=float), 0.0)
    fitted = np.interp(np.log(np.maximum(room, 1e-12)), np.log(bounds), paid, left=0.0, right=paid[-1])
    return np.where(room > 0, fitted, 0.0)


def mapped_leverage(papertrade_leverage: float, max_leverage: np.ndarray, mapping: str) -> np.ndarray:
    if mapping == "capped":
        return np.minimum(papertrade_leverage, max_leverage)
    if mapping == "relative":
        return np.maximum(1.0, papertrade_leverage / PAPERTRADE_MAX_LEVERAGE * max_leverage)
    raise ValueError(f"unknown leverage mapping {mapping}")


def daily_margin_path(flow: Flow, days: int, scale: float) -> np.ndarray:
    """Margin opened per simulated day: papertrade's full days in order, then the mean of its last
    seven, times `scale`."""
    observed = flow.daily_margin
    tail = observed[-7:].mean()
    path = np.concatenate([observed[:days], np.full(max(0, days - len(observed)), tail)])
    return path[:days] * scale


def load_mix(derived: Path, symbols: list[str]) -> np.ndarray:
    with (derived / "market_mix.csv").open() as fh:
        share = {r["symbol"]: float(r["share"]) for r in csv.DictReader(fh)}
    return np.array([share[s] for s in symbols])
