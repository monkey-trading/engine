"""Build the per-market report and figures from the pipeline's machine-readable results."""

import csv
import json
import os
import tempfile
from pathlib import Path

os.environ.setdefault("MPLBACKEND", "Agg")
if "MPLCONFIGDIR" not in os.environ:
    _CONFIG_DIR = tempfile.TemporaryDirectory(prefix="close-spread-matplotlib-")  # removed at exit
    os.environ["MPLCONFIGDIR"] = _CONFIG_DIR.name
import matplotlib.pyplot as plt
import numpy as np

ROOT = Path(__file__).resolve().parent
RESULTS = ROOT / "results"
BLUE, ORANGE, GREY = "#174d91", "#ba4d22", "#7a7f87"


def percent(value):
    return "n/a" if value is None else f"{value * 100:.2f}%"


def table(rows):
    lines = ["| Market | Class | Divergence | Hourly p99 (cal. upper) | Need | Calibration-only | Proposed | % of min margin | Validation hours over calibration-only | Confidence |",
             "|---|---|---:|---:|---:|---:|---:|---:|---:|---|"]
    for row in rows:
        if row["proposed_ceiling_bps"] is None:
            lines.append(f"| {row['symbol']} | {row['asset_class']} | | | | | Blocked | | | none |")
            continue
        lines.append(
            f"| {row['symbol']} | {row['asset_class']} | {row['max_divergence_bps']} | "
            f"{row['calibration_hourly_p99_upper_bps']:.0f} | {row['need_bps']:.1f} | "
            f"{row['calibration_only_ceiling_bps']} | **{row['proposed_ceiling_bps']}** | "
            f"{row['ceiling_percent_of_min_margin']:.1f}% | "
            f"{percent(row['validation_hours_over_calibration_ceiling'])} | {row['confidence']} |")
    return lines


def write_report(data):
    rows = data["markets"]
    live = [row for row in rows if row["proposed_ceiling_bps"] is not None]
    uncovered = [row["symbol"] for row in live if not row["validation_covered_by_calibration_ceiling"]]
    registry, reversals = data["registry_vs_global"], data["reversal_summary"]
    at_floor, gap = data["reversal_summary_if_every_ceiling_were_the_floor"], data["gap_comparison"]
    floor = data["objective"]["floor_bps"]
    fx = next(row for row in live if row["asset_class"] == "fx")
    floor_bound = ", ".join(row["symbol"] for row in live if row["proposed_ceiling_bps"] == floor)
    top_classes = ", ".join(f"{name} {value['share'] * 100:.0f}%" for name, value in reversals["top"].items()
                            if isinstance(value, dict))
    classes = ", ".join(f"{name} {low}" if low == high else f"{name} {low}-{high}"
                        for name, (low, high) in registry["class_ranges_bps"].items())
    lines = [
        "# Close-spread ceilings: results", "",
        f"{len(live)} of {len(rows)} launch markets have a proposed ceiling "
        f"({len(rows) - len(live)} blocked). By class, in basis points: {classes}.",
        "Nothing here is adopted: the burden limit, the floor and the tail statistic are the owner's to approve.", "",
        "A ceiling is the most an operator may ever charge to close, not a forecast of price moves. A close is",
        "charged a stress minimum plus a toxicity add-on only (no size spread on a pure close; liquidation waives",
        "toxicity), which cover one risk: the index being stale or off the true price when a client closes. The",
        "engine bounds that risk per market in its price rules, so the need is", "",
        "`need = (max_divergence_bps + 1) + hourly tail move * sqrt(max_age_ms / 1 hour)`", "",
        f"The ceiling is the smallest multiple of {data['objective']['step_bps']} bps at or above the need, never "
        f"below {data['objective']['floor_bps']} bps (the largest close charge the launch parameters allow, so no "
        "launch close is clamped by more than the rounding unit: at a ceiling equal to the launch charge the "
        "floored exact bound trims the engine's upward rounding, paying one raw unit less than today). The tail "
        "move is the upper end of the "
        "weekly block-bootstrap interval of the calibration sample's hourly p99 of the in-bar move (the open against the previous close is left out). Hourly bars are scaled to the "
        "one-second quote age by square-root-of-time, an assumption no hourly data can test.", "",
        "Gaps, closures and contract rolls are not in the need: a price jump is not a spread. They move margin and "
        "close-out risk, which `ceiling_effects.csv` shows a close spread cannot repair: in a 5% gap on EUR-USD, "
        "an uncapped 103-bp close charge cuts the zero-capital house shortfall by "
        f"{gap['shortfall_reduction_min'] * 100:.0f}% to {gap['shortfall_reduction_max'] * 100:.0f}% while costing "
        f"the trader {gap['trader_cost_ratio']:.0f} times as much.", "",
        "![Ceilings against the reference-risk need](ceilings_vs_need.png)", "",
        "## Per-market ceilings (basis points of the index price)", ""]
    lines += table(rows)
    lines += [
        "",
        "Method and test: the method is fixed on the calibration sample (2024-10-28 to 2026-04-27) and tested on "
        "the later validation sample (to 2026-10-12). "
        + (f"The calibration-only ceiling covers the validation need for {len(live) - len(uncovered)} of "
           f"{len(live)} markets; {', '.join(uncovered)} did not" if uncovered else "It covers every market")
        + ", so the proposed value refits on the larger of the two tails (in-sample on the validation tail). Over the "
        "validation hours, the share whose need would exceed the calibration-only ceiling is at most "
        f"{max(row['validation_hours_over_calibration_ceiling'] for row in live) * 100:.2f}%. A market at the floor "
        "fails the test whenever its validation need exceeds the launch charge, so the count of markets whose "
        f"calibration-only ceiling is above the floor ({data['objective']['markets_set_by_method']}) is not the "
        "population that can fail it.", "",
        "Burden: every proposed ceiling is at most "
        f"{max(row['ceiling_percent_of_min_margin'] for row in live):.0f}% of the market's smallest initial margin "
        f"(limit {data['objective']['burden_limit_percent_of_min_initial_margin']}%). Floor-bound ceilings "
        f"({floor_bound}) sit at the {floor}-bp floor, {floor / fx['min_initial_margin_bps'] * 100:.0f}% of their "
        f"{fx['min_initial_margin_bps']}-bp margin, so the launch parameters themselves already cost them that share "
        "of margin to close at the top stress level: an owner decision for the launch stress minimum, not for the ceiling.",
        "",
        "Sensitivity: doubling the volatility input raises "
        f"{sum(row['ceiling_if_volatility_doubled_bps'] > row['proposed_ceiling_bps'] for row in live)} ceilings, by at "
        f"most {max(row['ceiling_if_volatility_doubled_bps'] - row['proposed_ceiling_bps'] for row in live)} bps. The "
        "divergence limit in the genesis price rules, not volatility, sets most of each ceiling.", "",
        "## What the choices cost", "",
        f"Registry against one global value: a single value must be at least {registry['global_value_bps']} bps "
        f"(the largest market), which costs {len(registry['markets_whose_global_burden_exceeds_limit'])} of "
        f"{registry['markets_total']} markets more than the burden limit, up to "
        f"{registry['global_burden_percent_of_min_margin_worst']:.0f}% of margin. At the strictest value "
        f"({registry['strictest_ceiling_bps']} bps) {registry['markets_above_strictest']} markets would need more "
        "than they may charge.", "",
        "Reversal blending (grid of 6 position sizes and 4 opening fractions per market and stress level, against "
        "each market's proposed ceiling): the share of reversals whose blended spread exceeds the ceiling is "
        f"{reversals['calm']['share'] * 100:.1f}% at calm, {reversals['elevated']['share'] * 100:.1f}% at the "
        f"elevated level and {reversals['top']['share'] * 100:.1f}% at the top level (by class: {top_classes}). "
        "This is a grid, not a flow distribution. Under one global value at the floor instead, the shares are "
        f"{at_floor['calm']['share'] * 100:.1f}%, {at_floor['elevated']['share'] * 100:.1f}% and "
        f"{at_floor['top']['share'] * 100:.1f}%.",
        "", f"Rounding: see `rounding` in `ceilings.json`. At an index of 0.65 units one raw unit is "
        f"{10_000 / 650_000_000:.6f} bp; a seeded search of 20,000 pure closes found at most "
        f"{data['pure_close_excess_atoms_seeded']} raw unit above the exact bound. A floor to the exact bound makes "
        f"a close free only below {10_000 // floor + 1} raw units.", "",
        "## Evidence and limits", "",
        "- Data: the 102-week hourly grid inherited from the exposure-cap study, a transformed proxy with selected "
        "gaps removed. See [the source audit](../source-audit.md) for providers, dates, time zones, checksums, "
        "licensing and what is missing.",
        "- Nothing measures execution-horizon staleness or source divergence; the genesis limits stand in for it. "
        "That is why every confidence is at most moderate and DE40 and UK100 (cash-index proxies) are low.",
        "- Calm and stressed periods are both in the grid, but unseen tails, venue outages and proxy errors are not "
        "covered, and no historical calibration guarantees future outcomes.",
        "- The sweep conditions on two existing isolated positions and one price jump; it is not a "
        "cross-margin or admission simulation, and scenario counts are not probabilities.",
        "- Engine agreement: 13 vectors compare the pricing model with `Engine::apply` fills at V17 and V21 "
        "(`cargo test -p engine-core --test close_spread_calibration`).", ""]
    (RESULTS / "report.md").write_text("\n".join(lines))


def plots(data, statistics):
    """Both figures; returns the plotted market labels of the ceilings figure and the hourly-tails figure."""
    rows = [row for row in data["markets"] if row["proposed_ceiling_bps"] is not None]
    ceiling_labels = [row["symbol"] for row in rows]
    proposed = set(ceiling_labels)
    y = np.arange(len(rows))
    fig, ax = plt.subplots(figsize=(10, 12))
    ax.barh(y, [row["proposed_ceiling_bps"] for row in rows], color=BLUE, alpha=.85, label="Proposed ceiling")
    ax.scatter([row["need_bps"] for row in rows], y, color=ORANGE, marker="D", s=22, label="Reference-risk need")
    ax.scatter([row["calibration_only_ceiling_bps"] for row in rows], y, color="white", edgecolor=GREY, s=14,
               label="Calibration-only ceiling", zorder=3)
    ax.axvline(data["objective"]["floor_bps"], color=GREY, ls="--", lw=1, label="Launch close charge (floor)")
    ax.set(yticks=y, yticklabels=ceiling_labels, xlabel="basis points of the index price",
           title="Proposed close-spread ceilings")
    ax.invert_yaxis()
    ax.grid(axis="x", alpha=.2)
    ax.legend(loc="lower right", fontsize=8)
    fig.tight_layout()
    fig.savefig(RESULTS / "ceilings_vs_need.png", dpi=150, metadata={"Software": "close-spread-calibration"})
    plt.close(fig)
    items = [item for item in statistics["markets"]
             if item["calibration"].get("hourly_p99_week_block_bootstrap") is not None
             and item["market"]["symbol"] in proposed]
    tail_labels = [item["market"]["symbol"] for item in items]
    y = np.arange(len(items))
    calibration = np.array([item["calibration"]["abs_hourly_p99_bps"] for item in items])
    validation = np.array([item["validation"]["abs_hourly_p99_bps"] for item in items])
    low = np.array([item["calibration"]["hourly_p99_week_block_bootstrap"]["p025_bps"] for item in items])
    high = np.array([item["calibration"]["hourly_p99_week_block_bootstrap"]["p975_bps"] for item in items])
    fig, ax = plt.subplots(figsize=(10, 12))
    ax.hlines(y, low, high, color="#5b75a6", alpha=.65, label="Calibration weekly bootstrap 2.5-97.5%")
    ax.scatter(calibration, y, label="Calibration hourly p99", s=20, color=BLUE)
    ax.scatter(validation, y, label="Validation hourly p99", s=20, marker="x", color=ORANGE)
    ax.set(yticks=y, yticklabels=tail_labels, xlabel="Absolute in-bar move, close against open (basis points)",
           title="Inherited hourly tails: the volatility input")
    ax.invert_yaxis()
    ax.grid(axis="x", alpha=.2)
    ax.legend(loc="upper right", fontsize=8)
    fig.tight_layout(rect=(0, .035, 1, 1))
    fig.text(.5, .008, "Hourly in-bar moves of transformed proxy data; not execution-horizon evidence.",
             ha="center", fontsize=8)
    fig.savefig(RESULTS / "hourly_tails.png", dpi=150, metadata={"Software": "close-spread-calibration"})
    plt.close(fig)
    return ceiling_labels, tail_labels


if __name__ == "__main__":
    data = json.loads((RESULTS / "ceilings.json").read_text())
    statistics = json.loads((RESULTS / "market_statistics.json").read_text())
    write_report(data)
    plots(data, statistics)
    print(f"report and two figures written for {len(data['markets'])} markets; nothing adopted")
