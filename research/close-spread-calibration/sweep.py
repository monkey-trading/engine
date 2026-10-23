"""Conditional paired-account gap sensitivity; not a probability or admission simulation."""

from decimal import Decimal, ROUND_CEILING

D = Decimal
ATOM = D("0.000000001")
BPS = D(10_000)
CANDIDATES_BPS = (0, 1, 2, 3, 5, 10, 13, 25, 50, 100)


def charge(amount, rate_bps):
    return (amount * D(rate_bps) / BPS).quantize(ATOM, rounding=ROUND_CEILING)


def margin_cost(notional, tiers):
    result, low = D(0), D(0)
    for tier in tiers:
        high = D(tier["up_to"]) if "up_to" in tier else None
        end = notional if high is None else min(notional, high)
        if end > low:
            result += charge(end - low, tier["bps"])
        if high is not None:
            low = high
    return result


def paired_gap(*, notional, margin_bps, move, cap_bps, minimum_bps, toxicity_bps,
               closeout_level_bps, profit_fee_bps, capital, margin_tiers=None):
    """Close two already-admitted opposite positions after one unobserved price jump."""
    if notional <= 0 or move <= -1 or min(margin_bps, cap_bps, minimum_bps, toxicity_bps,
                                         closeout_level_bps, profit_fee_bps, capital) < 0:
        raise ValueError("invalid paired-gap scenario")
    tiers = margin_tiers if margin_tiers is not None else [{"bps": margin_bps}]
    margin = margin_cost(notional, tiers)
    bad_debt, profit_fees, total_cost, largest_cost = D(0), D(0), D(0), D(0)
    liquidations = 0
    for direction in (1, -1):
        at_index_equity = margin + notional * move * direction
        required = margin_cost(notional * (1 + move), tiers)
        liquidation = at_index_equity < charge(required, closeout_level_bps)
        liquidations += int(liquidation)
        requested = minimum_bps + (0 if liquidation else toxicity_bps)
        close_cost = charge(notional * (1 + move), min(cap_bps, requested))
        total_cost += close_cost
        largest_cost = max(largest_cost, close_cost)
        pnl = notional * move * direction - close_cost
        fee = charge(max(pnl, D(0)), profit_fee_bps)
        profit_fees += fee
        bad_debt += max(-(margin + pnl - fee), D(0))
    cash = capital + total_cost + profit_fees - bad_debt
    return {
        "largest_account_cost_usd": largest_cost,
        "initial_margin_per_account_usd": margin,
        "largest_cost_percent_initial_margin": largest_cost / margin * 100 if margin else None,
        "total_close_cost_usd": total_cost,
        "liquidations": D(liquidations),
        "bad_debt_usd": bad_debt,
        "profit_fees_usd": profit_fees,
        "house_cash_after_usd": cash,
        "house_cash_shortfall_usd": max(-cash, D(0)),
    }


def frontier(market, profit_fee_bps, closeout_level_bps):
    """Sweep explicit synthetic gap/capital assumptions and inherited proxy shocks."""
    spec = market["market"]
    shocks = {"synthetic_flat": D(0), "synthetic_up_1pct": D("0.01"),
              "synthetic_down_1pct": D("-0.01"), "synthetic_up_5pct": D("0.05"),
              "synthetic_down_5pct": D("-0.05")}
    for sample in ("calibration", "validation"):
        for field in ("abs_hourly_max_bps", "abs_gap_max_bps"):
            value = market[sample].get(field)
            if value is not None:
                magnitude = D(str(value)) / BPS
                shocks[f"{sample}_{field}_up"] = magnitude
                if magnitude < 1:
                    shocks[f"{sample}_{field}_down"] = -magnitude
    rows = []
    low = D(0)
    for band, tier in enumerate(spec["margin_tiers"]):
        high = D(tier["up_to"]) if "up_to" in tier else None
        notional = (low + high) / 2 if high is not None else max(low * 2, D(100_000))
        low = high if high is not None else low
        for cap in CANDIDATES_BPS:
            for requested in (0, 10, 100):
                for shock, move in shocks.items():
                    for capital_fraction in (D(0), D("0.01"), D("0.1")):
                        result = paired_gap(notional=notional, margin_bps=tier["bps"], move=move,
                                            cap_bps=cap, minimum_bps=requested, toxicity_bps=3,
                                            closeout_level_bps=closeout_level_bps,
                                            margin_tiers=spec["margin_tiers"],
                                            profit_fee_bps=profit_fee_bps,
                                            capital=2 * notional * capital_fraction)
                        rows.append({"symbol": spec["symbol"], "margin_band": band,
                                     "notional_per_leg_usd": str(notional), "marginal_margin_bps": tier["bps"],
                                     "candidate_cap_bps": cap, "requested_stress_minimum_bps": requested,
                                     "requested_toxicity_bps": 3,
                                     "shock": shock, "move_fraction": str(move),
                                     "capital_fraction_gross_notional": str(capital_fraction),
                                     **{key: None if value is None else str(value) for key, value in result.items()}})
    return rows
