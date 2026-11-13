"""Per-market close-spread ceilings: the most an operator could ever legitimately need to charge.

The ceiling is a fixed limit on operator power, so it is not a forecast of price moves. From V52 a
close is charged a stress minimum, a toxicity add-on (a liquidation waives it) and the size spread on
the slice it closes. The minimum and add-on exist to cover one risk: the index being stale or off the
true price when a client closes. The engine bounds that risk per market in its price rules (a quote may be
`max_age_ms` old, and the sources it medians may differ by `max_divergence_bps`). `compute_index` truncates the sources' spread to whole bps before comparing
it with that limit, so a market limited to `max_divergence_bps` admits sources just under one bp
beyond it; the need counts the exclusive bound:

    need = (max_divergence_bps + 1) + hourly tail move * sqrt(max_age_ms / 1 hour)

and the ceiling is the smallest multiple of 5 bps at or above the need, never below the market's
launch close charge: the widest stress minimum and the top toxicity add-on, plus the market's first
spread band at the widest size multiplier (from V52 the engine refuses a ceiling below it). The
method is fixed on the calibration sample and tested on the later validation sample; the proposed
value then refits on both samples (the larger tail). Gaps, closures and contract rolls change margin
and close-out risk, not this need: a price jump is not a spread (see the README). The tail is
therefore the in-bar move (close against the bar's own open); the open against the previous close is
left out. The two folded cash indices carry their overnight move inside the bar, which overstates
their tail.
"""

import math

LAUNCH_STEP_BPS = 5
BURDEN_LIMIT_PERCENT = 30
MIN_CALIBRATION_HOURS = 1_000
MIN_VALIDATION_HOURS = 250
HOUR_MS = 3_600_000
SELECTION_STATISTIC = ("the larger of the calibration hourly p99 (upper end of its weekly block-bootstrap interval) "
                       "and the validation hourly p99")

CLASS_FLAGS = {
    "crypto": ["single-venue USDT spot; USDT/USD basis unmeasured"],
    "fx": ["aggregator spot FX; no independent simultaneous quotes"],
    "index": ["front-month continuation: roll gaps zeroed by the inherited transform"],
    "commodity": ["front-month continuation: roll gaps zeroed by the inherited transform"],
    "equity": ["regular-session bars; corporate actions not audited"],
}
PROXY_SYMBOLS = {"DE40": "cash index standing in for a longer-session future; overnight move folded into the bar",
                 "UK100": "cash index standing in for a longer-session future; overnight move folded into the bar"}


def top_toxicity_bps(params):
    """The widest toxicity add-on: of the score's tiers from V54 (`score_tiers`), and of the tiers before it."""
    toxicity = params["toxicity"]
    tiers = toxicity.get("tiers", []) + toxicity.get("score_tiers", [])
    return max((tier["extra_spread_bps"] for tier in tiers), default=0)


def launch_close_charge_bps(params):
    """The widest stress minimum plus the top toxicity add-on: the most a close pays before its size spread."""
    stress = max(level["min_spread_bps"] for level in params["stress_levels"])
    return stress + top_toxicity_bps(params)


def tenth_bps(tier):
    """A spread band's rate in tenths of a bp; a band written in whole bps charges ten tenths a bp."""
    return tier["tenth_bps"] if "tenth_bps" in tier else tier["bps"] * 10


def launch_close_charge_tenth_bps(params, market):
    """The engine's floor for a market's ceiling from V52, in tenths of a bp: `launch_close_charge_bps` plus the
    first spread band at the widest size multiplier, rounded up (`close_spread::launch_close_charge_tenth_bps`)."""
    multiplier = max(level["size_spread_multiplier_bps"] for level in params["stress_levels"])
    first = tenth_bps(market["spread_tiers"][0])
    return launch_close_charge_bps(params) * 10 + -(-first * multiplier // 10_000)


def floor_bps(charge_tenth_bps):
    """The least whole-bps ceiling the engine admits over a launch close charge: `ceiling * 10 >= charge`."""
    return -(-charge_tenth_bps // 10)


def horizon_scale(max_age_ms):
    """Square-root-of-time factor from one hour to the longest age a quote may have."""
    if max_age_ms <= 0:
        raise ValueError("a quote's maximum age must be positive")
    return math.sqrt(max_age_ms / HOUR_MS)


def need_bps(divergence_bps, hourly_tail_bps, max_age_ms):
    # The engine truncates the divergence to whole bps before comparing, so the limit admits
    # sources up to just under divergence_bps + 1 apart.
    return divergence_bps + 1 + hourly_tail_bps * horizon_scale(max_age_ms)


def round_up(value_bps, floor, step=LAUNCH_STEP_BPS):
    """Smallest multiple of `step` at or above `value_bps`, never below `floor` (whole bps)."""
    if value_bps <= floor:
        return floor
    return int(math.ceil(value_bps / step - 1e-12)) * step


def exceedance(rows, unit, divergence_bps, max_age_ms, ceiling_bps):
    """Share of observed hours whose reference-risk need would exceed the ceiling."""
    import numpy as np
    active = rows[:, 0] != np.iinfo(np.int32).min
    values = rows[active].astype(np.float64) / unit
    if not len(values):
        return None
    moves = np.abs(np.expm1(values[:, 3])) * 10_000
    needs = need_bps(divergence_bps, moves, max_age_ms)
    return float(np.mean(needs > ceiling_bps))


def propose(item, params):
    """One results row for one market, from its statistics and the genesis it is listed under."""
    market = item["market"]
    rules = market["price_rules"]
    charge = launch_close_charge_tenth_bps(params, market)
    floor = floor_bps(charge)
    margin = market["margin_tiers"][0]["bps"]
    calibration, validation = item["calibration"], item["validation"]
    interval = calibration.get("hourly_p99_week_block_bootstrap")
    row = {
        "market_id": market["id"], "symbol": market["symbol"], "asset_class": market["asset_class"],
        "source": item.get("source"), "ticker": item.get("ticker"), "instrument_note": item.get("instrument_note"),
        "max_age_ms": rules["max_age_ms"], "max_divergence_bps": rules["max_divergence_bps"],
        "min_initial_margin_bps": margin, "launch_close_charge_tenth_bps": charge, "floor_bps": floor,
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
