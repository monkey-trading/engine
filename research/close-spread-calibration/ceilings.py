"""Per-market close-spread ceilings: the most an operator could ever legitimately need to charge.

The ceiling is a fixed limit on operator power, so it is not a forecast of price moves. A close is
charged only a stress minimum and a toxicity add-on (a pure close pays no size spread, and a
liquidation waives toxicity), which exist to cover one risk: the index being stale or off the true
price when a client closes. The engine bounds that risk per market in its price rules (a quote may be
`max_age_ms` old, and the sources it medians may differ by `max_divergence_bps`). So the need is

    need = max_divergence_bps + hourly tail move * sqrt(max_age_ms / 1 hour)

and the ceiling is the smallest multiple of 5 bps at or above the need, never below the largest
charge the launch parameters already allow. The method is fixed on the calibration sample and tested
on the later validation sample; the proposed value then refits on both samples (the larger tail). Gaps, closures and contract rolls change margin and
close-out risk, not this need: a price jump is not a spread (see the README).
"""

import math

LAUNCH_STEP_BPS = 5
BURDEN_LIMIT_PERCENT = 30
MIN_CALIBRATION_HOURS = 1_000
MIN_VALIDATION_HOURS = 250
HOUR_MS = 3_600_000
SELECTION_STATISTIC = "calibration hourly p99, upper end of its weekly block-bootstrap interval"

CLASS_FLAGS = {
    "crypto": ["single-venue USDT spot; USDT/USD basis unmeasured"],
    "fx": ["aggregator spot FX; no independent simultaneous quotes"],
    "index": ["front-month continuation: roll and session gaps zeroed by the inherited transform"],
    "commodity": ["front-month continuation: roll and session gaps zeroed by the inherited transform"],
    "equity": ["regular-session bars; corporate actions not audited"],
}
PROXY_SYMBOLS = {"DE40": "cash index standing in for a longer-session future",
                 "UK100": "cash index standing in for a longer-session future"}


def launch_close_charge_bps(params):
    """The largest nominal charge the launch parameters put on a voluntary pure close."""
    stress = max(level["min_spread_bps"] for level in params["stress_levels"])
    toxicity = max((tier["extra_spread_bps"] for tier in params["toxicity"]["tiers"]), default=0)
    return stress + toxicity


def horizon_scale(max_age_ms):
    """Square-root-of-time factor from one hour to the longest age a quote may have."""
    if max_age_ms <= 0:
        raise ValueError("a quote's maximum age must be positive")
    return math.sqrt(max_age_ms / HOUR_MS)


def need_bps(divergence_bps, hourly_tail_bps, max_age_ms):
    return divergence_bps + hourly_tail_bps * horizon_scale(max_age_ms)


def round_up(value_bps, floor_bps, step=LAUNCH_STEP_BPS):
    """Smallest multiple of `step` at or above `value_bps`, never below `floor_bps`."""
    if value_bps <= floor_bps:
        return floor_bps
    return int(math.ceil(value_bps / step - 1e-12)) * step


def exceedance(rows, unit, divergence_bps, max_age_ms, ceiling_bps):
    """Share of observed hours whose reference-risk need would exceed the ceiling."""
    import numpy as np
    active = rows[:, 0] != np.iinfo(np.int32).min
    values = rows[active].astype(np.float64) / unit
    if not len(values):
        return None
    moves = np.abs(np.expm1(values[:, 0] + values[:, 3])) * 10_000
    needs = divergence_bps + moves * horizon_scale(max_age_ms)
    return float(np.mean(needs > ceiling_bps))


def propose(item, params, floor_bps=None):
    """One results row for one market, from its statistics and the genesis it is listed under."""
    market = item["market"]
    rules = market["price_rules"]
    floor = launch_close_charge_bps(params) if floor_bps is None else floor_bps
    margin = market["margin_tiers"][0]["bps"]
    calibration, validation = item["calibration"], item["validation"]
    interval = calibration.get("hourly_p99_week_block_bootstrap")
    row = {
        "market_id": market["id"], "symbol": market["symbol"], "asset_class": market["asset_class"],
        "source": item.get("source"), "ticker": item.get("ticker"), "instrument_note": item.get("instrument_note"),
        "max_age_ms": rules["max_age_ms"], "max_divergence_bps": rules["max_divergence_bps"],
        "min_initial_margin_bps": margin, "launch_close_charge_bps": floor,
        "calibration_active_hours": calibration["active_hours"],
        "validation_active_hours": validation["active_hours"],
        "evidence_flags": CLASS_FLAGS[market["asset_class"]] + (
            ["proxy: " + PROXY_SYMBOLS[market["symbol"]]] if market["symbol"] in PROXY_SYMBOLS else []),
    }
    if (interval is None or calibration["active_hours"] < MIN_CALIBRATION_HOURS
            or validation["active_hours"] < MIN_VALIDATION_HOURS):
        return row | {"status": "blocked_pending_evidence", "proposed_ceiling_bps": None,
                      "confidence": "none"}
    divergence, age = rules["max_divergence_bps"], rules["max_age_ms"]
    calibration_need = need_bps(divergence, interval["p975_bps"], age)
    calibration_ceiling = round_up(calibration_need, floor)
    validation_need = need_bps(divergence, validation["abs_hourly_p99_bps"], age)
    validation_max = need_bps(divergence, validation["abs_hourly_max_bps"], age)
    need = max(calibration_need, validation_need)
    ceiling = round_up(need, floor)
    doubled = round_up(need_bps(divergence, 2 * max(interval["p975_bps"], validation["abs_hourly_p99_bps"]), age),
                       floor)
    proxy = market["symbol"] in PROXY_SYMBOLS
    return row | {
        "status": "proposed_low_confidence" if proxy else "proposed",
        "confidence": "low" if proxy else "moderate",
        "calibration_hourly_p99_bps": calibration["abs_hourly_p99_bps"],
        "calibration_hourly_p99_upper_bps": interval["p975_bps"],
        "validation_hourly_p99_bps": validation["abs_hourly_p99_bps"],
        "calibration_need_bps": calibration_need, "validation_need_bps": validation_need,
        "need_bps": need, "calibration_only_ceiling_bps": calibration_ceiling,
        "validation_covered_by_calibration_ceiling": calibration_ceiling >= validation_need,
        "validation_max_need_bps": validation_max,
        "proposed_ceiling_bps": ceiling,
        "ceiling_binding_constraint": "launch charge" if need <= floor else "reference-risk need",
        "ceiling_percent_of_min_margin": ceiling / margin * 100,
        "within_burden_limit": ceiling / margin * 100 <= BURDEN_LIMIT_PERCENT,
        "ceiling_if_volatility_doubled_bps": doubled,
    }
