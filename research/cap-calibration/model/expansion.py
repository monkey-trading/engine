"""Markets beyond the launch 37: clones of launch markets.

There is no history for markets 38 and up, so each is a clone of a launch market, its template:
it takes the template's margin bands, financing, cap fraction and session calendar, and its
returns follow the template's history with weight `shared` and an independent draw of the same
template's history with the rest (`prices.build`), so it correlates `shared` with its template,
`shared ** 2` with another clone of that template, and `shared` times the template's correlation
with any other market. Its flow is the template's, scaled (`flow`), or one set by `flow_mode`.
Templates are the launch markets quoted in USD, drawn by asset class in the proportions of
`class_weights`; within a class they cycle in launch order, so a run of any length is the same
list extended.
"""

import csv
from dataclasses import replace
from pathlib import Path

import numpy as np

from catalogue import PLANNED_CRYPTO_SYMBOLS, planned_crypto_tiers
from model.flow import Flow, schedule_key
from model.simulate import KINDS, Markets

# Launch markets that convert at another price (yen, euro, pound quotes) are not templates: a
# clone would need its own currency market.
NOT_TEMPLATES = {"USD-JPY", "DE40", "UK100", "JP225"}
FLOW_MODES = ("template", "fixed_total", "tail")


def asset_classes(derived: Path) -> dict[str, str]:
    with (derived / "market_stats.csv").open() as fh:
        return {r["symbol"]: r["asset_class"] for r in csv.DictReader(fh)}


def template_sequence(symbols: list[str], classes: dict[str, str], class_weights: dict[str, float], count: int) -> list[str]:
    """`count` templates: classes in the proportions of `class_weights` (largest deficit first,
    ties by class order), each class cycling through its launch markets in order."""
    pools = {c: [s for s in symbols if classes[s] == c and s not in NOT_TEMPLATES] for c in class_weights}
    empty = [c for c, pool in pools.items() if not pool]
    if empty:
        raise ValueError(f"no launch market to clone in {empty}")
    total = sum(class_weights.values())
    taken = {c: 0 for c in class_weights}
    out = []
    for i in range(count):
        c = max(class_weights, key=lambda k: class_weights[k] / total * (i + 1) - taken[k])
        out.append(pools[c][taken[c] % len(pools[c])])
        taken[c] += 1
    return out


def tail_share(markets: Markets, classes: dict[str, str], listed: set[str]) -> dict[str, float]:
    """Each asset class's smallest listed launch share, the convention for a market with no
    listing (`catalogue.HYPERLIQUID`)."""
    share = {}
    for s, m in zip(markets.symbols, markets.mix):
        if s in listed:
            share[classes[s]] = min(share.get(classes[s], np.inf), float(m))
    return share


def expand(
    markets: Markets,
    bars: np.ndarray,
    classes: dict[str, str],
    listed: set[str],
    n_markets: int,
    class_weights: dict[str, float],
    shared: float = 0.7,
    flow_mode: str = "template",
    flow_scale: float = 1.0,
) -> tuple[Markets, np.ndarray]:
    """The launch markets plus clones up to `n_markets`, and their price columns. `flow_scale`
    multiplies the clones' flow only. Returns `(markets, bars)` with the clones' bars the
    template's columns repeated."""
    if flow_mode not in FLOW_MODES:
        raise ValueError(f"unknown flow mode {flow_mode}")
    launch = len(markets.symbols)
    if n_markets < launch:
        raise ValueError("fewer markets than the launch markets")
    if n_markets == launch:
        return replace(markets, shared=np.ones(launch)), bars
    templates = template_sequence(markets.symbols, classes, class_weights, n_markets - launch)
    index = np.array([markets.symbols.index(t) for t in templates])
    tails = tail_share(markets, classes, listed)
    mix = markets.mix[index] * flow_scale
    if flow_mode == "tail":
        mix = np.array([tails[classes[t]] for t in templates]) * flow_scale
    out = Markets(
        symbols=markets.symbols + [f"{t}~{k}" for k, t in enumerate(templates, 1)],
        margin_rate=np.concatenate([markets.margin_rate, markets.margin_rate[index]]),
        schedule=markets.schedule + [markets.schedule[i] for i in index],
        cap_bps=np.concatenate([markets.cap_bps, markets.cap_bps[index]]),
        total_cap_bps=markets.total_cap_bps,
        fin_long=np.concatenate([markets.fin_long, markets.fin_long[index]]),
        fin_short=np.concatenate([markets.fin_short, markets.fin_short[index]]),
        mix=np.concatenate([markets.mix, mix]),
        open_hours_per_week=np.concatenate([markets.open_hours_per_week, markets.open_hours_per_week[index]]),
        columns=np.arange(n_markets),
        fx_columns=np.concatenate([markets.fx_columns, np.full(len(index), -1)]),
        fx_invert=np.concatenate([markets.fx_invert, np.zeros(len(index), dtype=bool)]),
        shared=np.concatenate([np.ones(launch), np.full(len(index), shared)]),
    )
    if flow_mode == "fixed_total":
        out = replace(out, mix=out.mix / out.mix.sum() * markets.mix.sum())
    return out, np.concatenate([bars, bars[:, markets.columns[index]]], axis=1)


def planned_crypto(markets: Markets, genesis_markets: list[dict]) -> Markets:
    """The launch markets with BTC-USD and ETH-USD on the planned margin bands
    (`catalogue.planned_crypto_tiers`): the maximum leverage, the close-out trigger and the
    flow's notional per margin follow, as do the sizes the caps are tested against."""
    tiers = planned_crypto_tiers(genesis_markets)
    rate = np.array(markets.margin_rate)
    schedule = list(markets.schedule)
    for symbol in PLANNED_CRYPTO_SYMBOLS:
        i = markets.symbols.index(symbol)
        rate[i] = tiers[0]["bps"] / 1e4
        schedule[i] = schedule_key(tiers)
    return replace(markets, margin_rate=rate, schedule=schedule)


def require_profiles(markets: Markets, flow: Flow) -> None:
    """Every margin schedule in use must have the flow's size and leverage profiles: a market
    without them would open nothing, which would read as safe."""
    missing = sorted({(kind, key) for key in set(markets.schedule) for kind in KINDS if (kind, "capped", key) not in flow.sizes or (kind, "capped", key) not in flow.leverage})
    if missing:
        raise ValueError(f"no flow profile for margin schedules {missing}; rerun derive-papertrade")
