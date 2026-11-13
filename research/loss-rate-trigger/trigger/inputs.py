"""The sibling studies this one reads, and the owner's values it runs at.

- `research/cap-calibration` (#111, #134): the hourly bars, the client flow and its target-flow
  reshaping, through `research/spread-calibration`'s `calib.target`;
- `research/rogue-operator` (#549): the 50 ms trade grids of the sampled hours and their year weights;
- `research/spread-calibration` (#623, #697): the fee function on the engine's table (`calib.fee`)
  and the attack classes' measured takes at the owner's values (`results/owner_*.json`).

Both sibling studies name their code `model`, so a process imports one of them, as
`calib.common.use` enforces; this study's code is `trigger`.
"""

import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
RESEARCH = HERE.parent
SPREAD = RESEARCH / "spread-calibration"
CAP = RESEARCH / "cap-calibration"
GENESIS = RESEARCH.parent / "config" / "production-genesis.toml"
if str(SPREAD) not in sys.path:
    sys.path.insert(1, str(SPREAD))

# The owner's values (#614, 2026-11-13): the full fee to 10 s, falling to the base at 15 minutes; the
# base 10% less the tier's discount past the curve (#614, D7). Honest flow pays tier 3's fee, the
# least any tier pays, as #697's honest share does.
CURVE = (10.0, 900.0)
HONEST_TIER = 3
STEADY_DAYS = 7  # the cap study's steady state: the mean of the reference venue's last seven full days


def genesis() -> dict:
    with GENESIS.open("rb") as fh:
        return tomllib.load(fh)["genesis"]


def fee_fn(tier: int = HONEST_TIER, base: float = 0.10, curve: tuple = CURVE):
    """The fee share a tier's winning close pays at a hold, under D7's function on the engine's table."""
    from calib import fee

    points = fee.curve_points(*curve)
    discount = fee.LAUNCH_DISCOUNTS[tier]
    return lambda hold_s: fee.d7(hold_s, base, discount, points)


def spread_tiers(spec: dict) -> list:
    """A market's size-spread bands as (up_to USD, rate), in the genesis's tenths of a bp (`V52`)."""
    return [(float(t.get("up_to", "inf")), t["tenth_bps"] / 1e5) for t in spec["spread_tiers"]]


def cap_flow(target: bool):
    """The cap study's bars and flow on the genesis's markets: (symbols, bars, flow, mix, open hours
    a week, specs). `target` reshapes the flow to the owner's target client (`calib.target`)."""
    import csv

    import numpy as np
    from calib import target as target_flow
    from calib.common import use

    use("cap")
    from model import prices
    from model.flow import load, load_mix, with_genesis_profiles

    derived = CAP / "data" / "derived"
    bars, symbols = prices.load_bars(derived / "hourly_bars.npz")
    g = genesis()
    by_symbol = {m["symbol"]: m for m in g["markets"]}
    keep = [i for i, s in enumerate(symbols) if s in by_symbol]
    missing = sorted(set(by_symbol) - {symbols[i] for i in keep})
    if missing:
        raise ValueError(f"genesis markets with no hourly bars: {missing}")
    bars, symbols = bars[:, keep], [symbols[i] for i in keep]
    specs = [by_symbol[s] for s in symbols]
    flow = with_genesis_profiles(load(derived), specs)
    if target:
        with (derived / "papertrade_holding.csv").open() as fh:
            flow = target_flow.target_flow(flow, list(csv.DictReader(fh)))
    open_hours = (bars[:, :, 0] != prices.CLOSED).sum(axis=0) / (bars.shape[0] / prices.WEEK)
    return symbols, bars, flow, load_mix(derived, symbols), np.asarray(open_hours), specs


def market_flows(symbols, flow, mix, open_hours, specs, daily_margin: float) -> dict:
    """Each market's `MarketFlow` at `daily_margin` of opened margin a day."""
    import numpy as np
    from model.flow import schedule_key

    from trigger import honest

    out = {}
    for sym, share, hours, spec in zip(symbols, mix, open_hours, specs):
        key = schedule_key(spec["margin_tiers"])
        margin_per_s = daily_margin * share * 7 / hours / 3600
        classes = []
        for c in flow.classes:
            mu, sigma = honest.lognormal_hold(c.mean_hold_h, c.mean_sqrt_hold_h)
            churn = c.hold_h == 0
            classes.append(honest.FlowClass(
                kind=c.kind, notional_per_s=margin_per_s * c.margin_share * float(flow.leverage[(c.kind, "capped", key)][c.bucket]),
                mu=mu, sigma=sigma, min_hold_s=max(c.min_hold_s, 1.0) if churn else 3600.0,
                max_hold_s=3600.0 if churn else np.inf, long_share=c.long_share,
                leverage=float(flow.leverage[(c.kind, "capped", key)][c.bucket])))
        sizes = {kind: honest.count_sizes(*flow.sizes[(kind, "capped", key)], spec["margin_tiers"]) for kind in ("directional", "hedged")}
        out[sym] = honest.MarketFlow(sym, spec["margin_tiers"], spread_tiers(spec), tuple(classes), sizes)
    return out


def steady_margin(flow) -> float:
    return float(flow.daily_margin[-STEADY_DAYS:].mean())


ROGUE_DATA = RESEARCH / "rogue-operator" / "data" / "derived"


def grid_hours():
    """The rogue study's sampled hours (#549) as `(market, hour, stratum, log index on its 50 ms grid,
    grid step in seconds, steps before the hour)`, read as its `model.paths.load_hours` reads them,
    without importing its `model`, which a process sharing the cap study's cannot."""
    import json

    import numpy as np

    z = np.load(ROGUE_DATA / "grids.npz")
    meta = [m for m in json.loads((ROGUE_DATA / "grids.json").read_text()) if m["venue"] == "coinbase"]
    step_s = float(z["step_ms"]) / 1000
    lead = int(round(float(z["lead_s"]) / step_s))
    x = np.log(np.cumsum(z["coinbase"].astype(np.int64), axis=1) / 100.0)
    return [(m["market"], m["hour"], m["stratum"], x[i], step_s, lead) for i, m in enumerate(meta)]


def grid_weights() -> dict:
    """(market, stratum) -> the year's hours each sampled hour of that stratum stands for."""
    import json

    sample = json.loads((ROGUE_DATA / "sample.json").read_text())
    counts = {}
    for market, _, stratum, *_ in grid_hours():
        counts[(market, stratum)] = counts.get((market, stratum), 0) + 1
    out = {}
    for market, v in sample["markets"].items():
        for k, s in enumerate(v["strata"]):
            if (market, k) in counts:
                out[(market, k)] = s["hours"] / counts[(market, k)] * 8760 / v["year_hours"]
    return out
