"""Per-market ceilings, their out-of-sample test, and the evidence for #344's five choices."""

import csv
import importlib.util
import json
from decimal import Decimal as D
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CEILINGS, PATHS, SWEEP = load("ceilings"), load("closing_paths"), load("sweep")
RUN = load("run")
GAP_SHOCK = D("0.05")
ATTACK_BPS = 100


def exceedances(rows):
    """Out-of-sample exceedance of the calibration-only ceiling, and in-sample exceedance of the proposal."""
    with np.load(RUN.REPO / "research/cap-calibration/data/derived/hourly_bars.npz", allow_pickle=False) as data:
        bars, unit = data["bars"], float(data["unit"])
    cutoff = RUN.CALIBRATION_WEEKS * 168
    for index, row in enumerate(rows):
        if row["proposed_ceiling_bps"] is None:
            continue
        args = (unit, row["max_divergence_bps"], row["max_age_ms"])
        row["validation_hours_over_calibration_ceiling"] = CEILINGS.exceedance(
            bars[cutoff:, index], *args, row["calibration_only_ceiling_bps"])
        row["calibration_hours_over_proposed_ceiling"] = CEILINGS.exceedance(
            bars[:cutoff, index], *args, row["proposed_ceiling_bps"])
        row["validation_hours_over_proposed_ceiling"] = CEILINGS.exceedance(
            bars[cutoff:, index], *args, row["proposed_ceiling_bps"])
    return rows


def effects(items, rows, params):
    """Trader exit cost at the ceiling and house outcome in a 5% gap, per margin band."""
    out = []
    ceilings = {row["symbol"]: row["proposed_ceiling_bps"] for row in rows}
    for item in items:
        spec = item["market"]
        ceiling = ceilings[spec["symbol"]]
        if ceiling is None:
            continue
        low = D(0)
        for band, tier in enumerate(spec["margin_tiers"]):
            high = D(tier["up_to"]) if "up_to" in tier else None
            notional = (low + high) / 2 if high is not None else max(low * 2, D(100_000))
            low = high if high is not None else low
            margin = SWEEP.margin_cost(notional, spec["margin_tiers"])
            for label, cap in (("at_ceiling", ceiling), ("no_cap_operator_attack", 9_999)):
                cost = SWEEP.charge(notional, min(cap, ATTACK_BPS + 3))
                gap = SWEEP.paired_gap(
                    notional=notional, margin_bps=tier["bps"], move=GAP_SHOCK, cap_bps=cap,
                    minimum_bps=ATTACK_BPS, toxicity_bps=3, closeout_level_bps=params["closeout_level_bps"],
                    profit_fee_bps=params["profit_fee_bps"], capital=D(0), margin_tiers=spec["margin_tiers"],
                    self_quoted_jpy=spec["symbol"] == "USD-JPY")
                out.append({"symbol": spec["symbol"], "margin_band": band, "notional_usd": str(notional),
                            "case": label, "close_cost_bps": min(cap, ATTACK_BPS + 3),
                            "close_cost_usd": str(cost),
                            "close_cost_percent_initial_margin": str(round(cost / margin * 100, 4)),
                            "bad_debt_usd_in_5pct_gap": str(gap["bad_debt_usd"]),
                            "house_cash_shortfall_usd_zero_capital": str(gap["house_cash_shortfall_usd"]),
                            "spread_revenue_usd": str(gap["total_close_cost_usd"])})
    return out


def comparisons(rows):
    """Registry against a single global value, and the headroom a strictest-ceiling refusal strands."""
    live = [row for row in rows if row["proposed_ceiling_bps"] is not None]
    global_value = max(row["proposed_ceiling_bps"] for row in live)
    strictest = min(row["proposed_ceiling_bps"] for row in live)
    classes = {}
    for row in live:
        classes.setdefault(row["asset_class"], []).append(row)
    return {
        "global_value_bps": global_value, "strictest_ceiling_bps": strictest,
        "markets_whose_global_burden_exceeds_limit": [
            row["symbol"] for row in live
            if global_value / row["min_initial_margin_bps"] * 100 > CEILINGS.BURDEN_LIMIT_PERCENT],
        "markets_above_strictest": sum(row["proposed_ceiling_bps"] > strictest for row in live),
        "markets_total": len(live),
        "stranded_headroom_bps_by_class": {
            name: sorted({row["proposed_ceiling_bps"] - strictest for row in group})
            for name, group in classes.items()},
        "class_ranges_bps": {name: [min(r["proposed_ceiling_bps"] for r in group),
                                    max(r["proposed_ceiling_bps"] for r in group)]
                             for name, group in classes.items()},
        "global_burden_percent_of_min_margin_worst": max(
            global_value / row["min_initial_margin_bps"] * 100 for row in live),
    }


def gap_comparison(rows):
    """What an uncapped operator charge does in a 5% gap against the capped one, per EUR-USD band."""
    by_band = {}
    for row in rows:
        if row["symbol"] == "EUR-USD":
            by_band.setdefault(row["margin_band"], {})[row["case"]] = row
    reductions = [1 - D(pair["no_cap_operator_attack"]["house_cash_shortfall_usd_zero_capital"])
                  / D(pair["at_ceiling"]["house_cash_shortfall_usd_zero_capital"]) for pair in by_band.values()]
    ratios = [pair["no_cap_operator_attack"]["close_cost_bps"] / pair["at_ceiling"]["close_cost_bps"]
              for pair in by_band.values()]
    return {"market": "EUR-USD", "shortfall_reduction_min": float(min(reductions)),
            "shortfall_reduction_max": float(max(reductions)), "trader_cost_ratio": ratios[0]}


def reversal_summary(rows):
    out = {}
    for level in ("calm", "elevated", "top"):
        subset = [row for row in rows if row["stress_level"] == level]
        out[level] = {"cells": len(subset), "exceeding": sum(row["exceeds_ceiling"] for row in subset),
                      "share": sum(row["exceeds_ceiling"] for row in subset) / len(subset),
                      "max_blended_bps": max(row["blended_reversal_bps"] for row in subset)}
        for name in sorted({row["asset_class"] for row in subset}):
            group = [row for row in subset if row["asset_class"] == name]
            out[level][name] = {"share": sum(row["exceeds_ceiling"] for row in group) / len(group),
                                "max_blended_bps": max(row["blended_reversal_bps"] for row in group)}
    return out


def write_csv(path, rows):
    with path.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)


def build():
    statistics = json.loads((ROOT / "results/market_statistics.json").read_text())
    params = statistics["protocol_params"]
    rows = exceedances([CEILINGS.propose(item, params) for item in statistics["markets"]])
    ceilings = {row["symbol"]: row["proposed_ceiling_bps"] for row in rows if row["proposed_ceiling_bps"]}
    live = [item for item in statistics["markets"] if item["market"]["symbol"] in ceilings]
    levels, toxicity = PATHS.levels_from(params)
    reversals = PATHS.reversal_rows(live, ceilings, levels, toxicity)
    floor = CEILINGS.launch_close_charge_bps(params)
    at_floor = PATHS.reversal_rows(live, {symbol: floor for symbol in ceilings}, levels, toxicity)
    effects_rows = effects(live, rows, params)
    return {
        "inputs": statistics["input_sha256"], "engine_revision": statistics["engine_revision"],
        "objective": {
            "need": "(max_divergence_bps + 1) + hourly tail move * sqrt(max_age_ms / 1h)",
            "selection_statistic": CEILINGS.SELECTION_STATISTIC,
            "markets_where_validation_could_fail": sum(
                row["calibration_only_ceiling_bps"] > floor for row in rows if row["proposed_ceiling_bps"]),
            "floor_bps": CEILINGS.launch_close_charge_bps(params), "step_bps": CEILINGS.LAUNCH_STEP_BPS,
            "burden_limit_percent_of_min_initial_margin": CEILINGS.BURDEN_LIMIT_PERCENT,
            "owner_decisions": "the burden limit, the floor, and whether to adopt the tail statistic",
        },
        "markets": rows, "registry_vs_global": comparisons(rows),
        "reversal_summary": reversal_summary(reversals), "reversals": reversals,
        "reversal_summary_if_every_ceiling_were_the_floor": reversal_summary(at_floor),
        "gap_comparison": gap_comparison(effects_rows),
        "rounding": PATHS.rounding_rows(), "pure_close_excess_atoms_seeded": PATHS.pure_close_excess_atoms(),
        "effects": effects_rows,
    }


if __name__ == "__main__":
    result = build()
    results = ROOT / "results"
    write_csv(results / "ceilings.csv", [{k: (";".join(v) if isinstance(v, list) else v)
                                          for k, v in row.items()} for row in result["markets"]])
    write_csv(results / "reversal_blended_spread.csv", result.pop("reversals"))
    write_csv(results / "ceiling_effects.csv", result.pop("effects"))
    (results / "ceilings.json").write_text(json.dumps(result, indent=2, allow_nan=False) + "\n")
    proposed = [row for row in result["markets"] if row["proposed_ceiling_bps"] is not None]
    print(f"{len(proposed)} of {len(result['markets'])} markets have a proposed ceiling; the rest are blocked")
