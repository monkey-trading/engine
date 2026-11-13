"""Spread tiers for every launch market, from its own volatility.

A tier's spread is what a fill pays on top of the index. The best tier covers the mean move over
the price-lag window in a busy hour (the market's 90th-percentile open hour, at #510's 3 s bound)
plus the adverse selection of ordinary flow, rounded up to the whole basis points the engine
stores. The 3 s move dominates: a spread that covers it also covers the move over an honest
node's shorter lead. Larger size tiers double and quadruple the best tier, the widening the stress
levels already apply, keeping the genesis's size bounds: a large order's adverse selection and the
house's exposure to it grow with size, and this study has no measurement of how fast.

`NORMS` are the order of magnitude of published retail CFD minimum spreads for each asset class,
as a check, not a calibration target: brokers publish typical spreads for small tickets on their
own pages, change them often, and widen them for size and in fast markets.
"""

import json
import math

from calib.common import STUDIES, genesis_markets

LAG_S = 3.0
QUANTILE = 90
TIER_MULTIPLIERS = (1, 2, 4)
# Ordinary flow's edge against the house: the #111 flow's directional raw profit over notional.
ADVERSE_KEY = "raw_pnl_over_notional_bps_directional"

# (low, high) bps, by asset class and for a few markets with their own conventions.
NORMS = {
    "crypto": (4.0, 50.0),
    "fx": (0.5, 1.5),
    "index": (0.4, 1.5),
    "JP225": (1.0, 3.0),
    "XAU-USD": (1.0, 3.0),
    "XAG-USD": (3.0, 10.0),
    "WTI": (3.0, 6.0),
    "BRENT": (3.0, 6.0),
    "equity": (2.0, 10.0),
}


def adverse_bps() -> float:
    summary = json.loads((STUDIES["cap"] / "data" / "derived" / "papertrade_summary.json").read_text())
    return float(summary[ADVERSE_KEY])


def norm_for(market: dict) -> tuple[float, float]:
    return NORMS.get(market["symbol"]) or NORMS.get(market["asset_class"]) or NORMS["equity"]


def best_tier_bps(move_bps: float, adverse: float) -> int:
    """Whole bps covering the lag move and ordinary flow's edge; at least 1."""
    return max(1, math.ceil(move_bps + adverse - 1e-9))


def calibrate(moves: dict) -> list[dict]:
    """A row per launch market: the move it covers, the proposed tiers, the genesis tiers, and the
    check against published norms."""
    adverse = adverse_bps()
    rows = []
    for m in genesis_markets():
        stats = moves["markets"][m["symbol"]]
        measured = stats.get(f"measured_{LAG_S}_p{QUANTILE}")
        move = measured if measured is not None else stats[f"scaled_{LAG_S}_p{QUANTILE}"]
        best = best_tier_bps(move, adverse)
        tiers = [{"up_to": t.get("up_to"), "bps": best * k} for t, k in zip(m["spread_tiers"], TIER_MULTIPLIERS)]
        lo, hi = norm_for(m)
        rows.append({
            "symbol": m["symbol"], "asset_class": m["asset_class"],
            "move_3s_p90_bps": move, "source": "measured" if measured is not None else "scaled",
            "honest_move_0.25s_p90_bps": stats.get(f"measured_0.25_p{QUANTILE}", stats[f"scaled_0.25_p{QUANTILE}"]),
            "adverse_bps": adverse, "tiers": tiers, "genesis_tiers": m["spread_tiers"],
            "norm_low_bps": lo, "norm_high_bps": hi,
            "against_norm": "below" if best < lo else ("above" if best > hi else "within"),
        })
    return rows


def toml_lines(rows: list[dict]) -> str:
    """The proposed `spread_tiers` line for each market, as genesis would carry it."""
    out = []
    for r in rows:
        parts = [f'{{ up_to = "{t["up_to"]}", bps = {t["bps"]} }}' if t["up_to"] else f'{{ bps = {t["bps"]} }}' for t in r["tiers"]]
        out.append(f'# {r["symbol"]}\nspread_tiers = [{", ".join(parts)}]')
    return "\n".join(out) + "\n"
