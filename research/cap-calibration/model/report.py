"""Writes `results/report.md`: the data summary and every results table, from the stage outputs."""

import csv
import json
import math
from pathlib import Path

from model.experiments import MEASURES, SCALES, TARGETS, binding_point, candidates, chosen

MEASURE_NAMES = {
    "p_ever": "NAV below zero at some hour",
    "p_short": "house cash below zero (the vault short of client balances) at some hour",
}


def usd(x) -> str:
    if x is None or x == "":
        return "-"
    x = float(x)
    sign = "-" if x < 0 else ""
    x = abs(x)
    for unit, scale in (("B", 1e9), ("M", 1e6), ("k", 1e3)):
        if x >= scale:
            return f"{sign}${x / scale:.3g}{unit}"
    return f"{sign}${x:.0f}"


def pct(x, digits: int = 2) -> str:
    x = max(float(x), 0.0)  # refused shares can come out a rounding error below zero
    if x == 0:
        return "0"
    if x < 0.0001:
        return "<0.01%"
    return f"{100 * x:.{digits}f}%"


def counted(row: dict, key: str) -> str:
    """A measure as its share of paths, the count, and the 95% upper bound."""
    count = int(row["ever_paths" if key == "p_ever" else "short_paths"])
    return f"{pct(row[key], 2)} ({count}; <{pct(row[key + '_upper'], 2)})"


def table(header: list[str], rows: list[list]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(lines)


def read(path: Path) -> list[dict]:
    """Rows with every numeric field as a float."""
    with path.open() as fh:
        rows = list(csv.DictReader(fh))
    for row in rows:
        for k, v in row.items():
            try:
                row[k] = float(v)
            except ValueError:
                pass
    return rows


def data_section(derived: Path) -> str:
    s = json.loads((derived / "papertrade_summary.json").read_text())
    rows = [
        ["Window", f"{s['from'][:16]} to {s['to'][:16]} UTC ({s['days']} days), blocks {s['blocks'][0]:,} to {s['blocks'][1]:,}"],
        ["Accounts, positions", f"{s['accounts']:,} accounts, {s['positions']:,} positions ({s['closed']:,} closed, {pct(s['liquidated_share'])} liquidated)"],
        ["Opened notional", f"{usd(s['opened_notional_usd'])}, {pct(s['btc_share_of_notional'], 1)} BTC"],
        ["Hold (count) p10 / p50 / p90 / p99", " / ".join(f"{v * 3600:.0f}s" if v < 1 / 30 else f"{v * 60:.0f}min" if v < 1 else f"{v:.1f}h" for v in s["hold_hours_quantiles_by_count"].values())],
        ["Exposure-time held over 1h / 24h", f"{pct(s['exposure_time_share_held_over_1h'], 1)} / {pct(s['exposure_time_share_held_over_24h'], 1)}"],
        ["Leverage (notional-weighted) p10 / p50 / p90", " / ".join(f"{v:.0f}x" for v in s["leverage_quantiles_by_notional"].values())],
        ["Hedged share of notional / exposure-time", f"{pct(s['hedged_share_of_notional'], 1)} / {pct(s['hedged_share_of_exposure_time'], 1)}"],
        ["Hedged share on the full days the model samples: notional / margin", f"{pct(s['full_days_hedged_share_of_notional'], 1)} / {pct(s['full_days_hedged_share_of_margin'], 1)} ({s['full_days_positions']:,} positions)"],
        ["Exposure offset within accounts / market-wide", f"{pct(s['offset_within_accounts_share'], 1)} / {pct(s['offset_market_wide_share'], 1)}"],
        ["Hedged flow's cost per $1 of loss (haircut and fee on the winning leg)", f"${s['hedged_cost_per_usd_of_loss']:.3f}"],
        ["Winners keep, of raw gains", pct(s["winner_keeps_share_of_raw_gain"], 1)],
        ["Traders' raw PnL over notional (all / directional)", f"{s['raw_pnl_over_notional_bps']:.2f} / {s['raw_pnl_over_notional_bps_directional']:.2f} bps"],
        ["Directional long share: mean, hourly AR(1), residual sd, slope on last hour's return", f"{s['long_share_mean']:.3f}, {s['long_share_ar1']:.2f}, {s['long_share_resid_sd']:.3f}, {s['long_share_slope_on_prev_hour_return']:.2f}"],
        ["Deposits / withdrawals / net", f"{usd(s['deposits_usd'])} / {usd(s['withdrawals_usd'])} / {usd(s['net_deposits_usd'])}"],
        ["Checks", f"{s['unmatched_closes']} unmatched closes; fee / adjusted PnL median {s['fee_over_adjusted_pnl_median']}; BTC entry vs Binance hour close median gap {pct(s['btc_entry_vs_binance_hour_close_median_gap'], 3)}"],
    ]
    out = ["## papertrade.xyz behaviour", "", table(["Measure", "Value"], rows), ""]
    daily = read(derived / "papertrade_daily.csv")
    out += ["Daily flow:", "", table(
        ["Date", "Full day", "Positions", "Notional", "Margin", "Active accounts", "Hedged share", "Deposits", "Withdrawals"],
        [[d["date"], d["full_day"], f"{d['positions']:,.0f}", usd(d["opened_notional_usd"]), usd(d["opened_margin_usd"]), f"{d['active_accounts']:,.0f}", pct(d["hedged_share"], 1), usd(d["deposits_usd"]), usd(d["withdrawals_usd"])] for d in daily],
    ), ""]
    return "\n".join(out)


def market_section(derived: Path) -> str:
    stats = read(derived / "market_stats.csv")
    mix = {r["symbol"]: r for r in read(derived / "market_mix.csv")}
    rows = [
        [
            r["symbol"],
            f"{float(r['margin_bps']) / 100:g}%",
            pct(mix[r["symbol"]]["share"], 3) + ("" if mix[r["symbol"]]["listed"] == "True" else " *"),
            pct(r["vol_ann_window"], 0),
            pct(r["vol_ann_long"], 0) if not math.isnan(r["vol_ann_long"]) else "-",
            pct(r["daily_abs_p99_window"], 1),
            pct(r["gap_abs_p99"], 2),
            pct(r["gap_abs_max"], 1),
            f"{r['gaps_past_half_margin_per_year']:g}",
            f"{r['gaps_past_margin_per_year']:g}",
            pct(r["hourly_abs_max"], 1),
            pct(r["daily_abs_max_long"], 1) if not math.isnan(r["daily_abs_max_long"]) else "-",
        ]
        for r in stats
    ]
    header = ["Market", "Margin", "Mix", "Vol (window)", "Vol (long)", "Daily p99", "Gap p99", "Gap max", "Gaps past margin/2 a year", "Gaps past margin a year", "Hour max", "Day max (long)"]
    return "\n".join(["## Launch markets", "", table(header, rows), "", "`*` no Hyperliquid listing: the asset class's smallest listed volume.", ""])


def per_market_section(results: Path) -> str:
    sweep = read(results / "per_market_sweep.csv")
    rows = []
    for symbol in dict.fromkeys(r["symbol"] for r in sweep):
        mine = sorted((r for r in sweep if r["symbol"] == symbol), key=lambda r: r["floor"])
        bind = binding_point(mine)
        at_bind = next((r for r in mine if r["floor"] == bind), mine[-1])
        rows.append([
            symbol,
            usd(bind) if bind is not None else "> $100M",
            counted(at_bind, "p_ever"),
            counted(at_bind, "p_short"),
            pct(at_bind["refused_share_week1"], 1),
            usd(at_bind["bad_debt_mean"]),
            usd(at_bind["fee_mean"]),
        ])
    header = ["Market", "Refusals end at", "NAV<0 ever", "Cash<0", "Refused, week 1", "Bad debt mean", "Fees mean"]
    intro = "Each market alone at its share of the flow, at the floor where its 90-day refused share falls under 1%. With a zero floor nothing opens. Each probability is the share of paths, with the count and the one-sided 95% upper bound in brackets."
    return "\n".join(["## Each market alone", "", intro, "", table(header, rows), ""])


JOINT_HEADER = ["Total floor", "NAV<0 ever", "Cash<0", "Queue", "NAV<0 day 90", "NAV<-$100k", "NAV<-$1M", "First dip p50 (day)", "Refused, week 1", "Refused, 90 days", "NAV day 90 p50", "Lowest NAV p1", "Bad debt mean"]


def joint_row(r: dict) -> list:
    return [
        usd(r["total_floor"]),
        counted(r, "p_ever"),
        counted(r, "p_short"),
        pct(r["p_queue"], 3),
        pct(r["p_end"], 3),
        pct(r["p_dip_100k"], 2),
        pct(r["p_dip_1m"], 2),
        f"{r['first_negative_day_p50']:.1f}" if r["first_negative_day_p50"] >= 0 else "-",
        pct(r["refused_share_week1"], 1),
        pct(r["refused_share_directional"], 1),
        usd(r["nav_end_p50"]),
        usd(r["min_nav_p01"]),
        usd(r["bad_debt_mean"]),
    ]


def label(row: dict) -> str:
    if row["family"] == "uniform":
        return f"{usd(row['level'])} on every market"
    return "binding" if row["level"] == 1.0 else f"level {pct(row['level'], 2)}"


def joint_section(results: Path) -> str:
    out = ["## All markets together", ""]
    paths = {f: results / f"joint_{f}.csv" for f in (*MEASURES, "uniform")}
    if not all(p.exists() for p in paths.values()):
        return ""
    families = {f: sorted(read(p), key=lambda r: r["total_floor"]) for f, p in paths.items()}
    titles = {**{k: f"Floors set by risk level for {MEASURE_NAMES[k]}" for k in MEASURES}, "uniform": "The same floor on every market"}
    for family, rows in families.items():
        out += [f"### {titles[family]}", "", table(["Floors"] + JOINT_HEADER, [[label(r)] + joint_row(r) for r in rows]), ""]
    symbols = list(json.loads(families["uniform"][0]["market_floors"]))
    for key in MEASURES:
        picks = {t: chosen(candidates(families, key), key, t) for t in TARGETS}
        lines = [
            f"- {pct(t)}: " + (f"{label(c)}, total floor {usd(c['total_floor'])}, {counted(c, key)}, refusing {pct(c['refused_share_week1'], 1)} in week 1 and {pct(c['refused_share_directional'], 1)} over 90 days" if c else "no candidate meets it")
            for t, c in picks.items()
        ]
        out += [f"### Chosen for {MEASURE_NAMES[key]}", "", "Of the candidates meeting each target, the one that refuses least over 90 days:", "", *lines, ""]
        body = [[s] + [usd(json.loads(c["market_floors"])[s]) if c else "-" for c in picks.values()] for s in symbols]
        body.append(["Total"] + [usd(c["total_floor"]) if c else "-" for c in picks.values()])
        out += [table(["Market"] + [f"Floor at {pct(t)}" for t in TARGETS], body), ""]
        total_path = results / f"joint_total_{key}.csv"
        if total_path.exists():
            totals = sorted(read(total_path), key=lambda r: r["total_floor"])
            body = [[f"x{r['level']:g}"] + joint_row(r) for r in totals]
            base = {"family": totals[0]["base_family"], "level": totals[0]["base_level"]}
            out += [f"Total floor below the sum, at the floors chosen at 1% ({label(base)}):", "", table(["Share of sum"] + JOINT_HEADER, body), ""]
    return "\n".join(out)


def sensitivity_section(results: Path) -> str:
    path = results / "sensitivity.csv"
    if not path.exists():
        return ""
    rows = read(path)
    names = list(dict.fromkeys(r["scenario"] for r in rows))
    body = []
    for name in names:
        mine = {r["floor_scale"]: r for r in rows if r["scenario"] == name}
        base = mine[1.0]
        largest = lambda key: max((s for s in SCALES if mine[float(s)][key] <= 0.01), default=None)  # noqa: E731
        body.append([
            name,
            counted(base, "p_ever"),
            counted(base, "p_short"),
            pct(base["p_dip_100k"], 2),
            pct(base["refused_share_week1"], 1),
            usd(base["nav_end_p50"]),
            usd(base["fee_mean"]),
            usd(base["bad_debt_mean"]),
            " / ".join(f"x{v:g}" if v is not None else "none" for v in (largest("p_ever"), largest("p_short"))),
        ])
    first = rows[0]
    measure, met = first["base_measure"], first["base_meets_target"] == "True"
    chosen_floors = label({"family": first["base_family"], "level": first["base_level"]})
    base = f"the floors chosen for {MEASURE_NAMES[measure]} at 1% ({chosen_floors})" if met else f"the smallest candidate ({chosen_floors}), as none met 1%"
    header = ["Scenario", "NAV<0 ever", "Cash<0", "NAV<-$100k", "Refused, week 1", "NAV day 90 p50", "Fees mean", "Bad debt mean", "Largest scale at 1%: NAV / cash"]
    scales = ", ".join(f"x{s:g}" for s in SCALES)
    intro = f"Every scenario at {base}, and with every floor scaled together by {scales}."
    return "\n".join(["## Sensitivity", "", intro, "", table(header, body), ""])


def retained_section(results: Path) -> str:
    """The retained-NAV study (`model/retained.py`): the baseline at every market count, the
    stability of the 37-market answer in the number of paths, and the sensitivities."""
    from model import retained

    rows = read(results / "retained_nav.csv")
    by_run: dict[tuple, list[dict]] = {}
    for r in rows:
        by_run.setdefault((r["variant"], int(r["markets"])), []).append(r)

    def floor(v):
        return "not in grid" if v is None else usd(v)

    def summary(run_rows):
        got = retained.retained_nav(run_rows)
        at = got["row"]
        # The highest share of paths with house cash below zero at any floor that serves the flow.
        served = [r for r in run_rows if r["floor"] >= (got["serve_floor"] or float("inf"))]
        worst = max((r["p_short"] for r in served), default=float("nan"))
        return got, at, worst

    head = []
    for (variant, n), run_rows in sorted(by_run.items(), key=lambda kv: kv[0][1]):
        if variant != "baseline":
            continue
        got, at, worst = summary(run_rows)
        paths = int(run_rows[0]["paths"])
        p = at["p_short"] if at else float("nan")
        head.append(
            [n, paths, usd(got["serve_floor"]), floor(got["retained_nav"]), pct(p) if at else "-", pct(retained.standard_error(p, paths), 2) if at else "-", pct(worst), pct(at["refused_share_directional"]) if at else "-", pct(at["refused_share_week1"]) if at else "-", usd(at["fee_mean"]) if at else "-"]
        )
    out = [
        "## Retained NAV by market count",
        "",
        "The house holds its liquidity floor as NAV, sweeping everything above it each hour, and trades under the genesis cap fractions of that NAV. Retained NAV is the lowest grid floor that refuses at most 1% of directional notional and keeps house cash below zero in at most 1% of paths, at it and at every higher grid floor. Baseline clones are equity markets correlated 0.7 with their template, each as busy as it. Paths, with the binomial standard error of the measure at the retained NAV; the last columns are the worst share at any floor that serves the flow, and the refusal and mean fees at the retained NAV.",
        "",
        table(["Markets", "Paths", "Serves flow from", "Retained NAV", "Cash<0", "SE", "Cash<0, worst served floor", "Refused, 90 days", "Refused, week 1", "Fees mean"], head),
        "",
    ]
    stability = results / "retained_stability.csv"
    if stability.exists():
        st = read(stability)
        rows_st = []
        for count in sorted({int(r["paths"]) for r in st}):
            sub = [r for r in st if int(r["paths"]) == count]
            got = retained.retained_nav(sub)
            at = got["row"]
            worst = max((r["p_short"] for r in sub if r["floor"] >= (got["serve_floor"] or float("inf"))), default=float("nan"))
            rows_st.append([count, usd(got["serve_floor"]), floor(got["retained_nav"]), pct(at["p_short"]) if at else "-", pct(worst), pct(retained.standard_error(max(worst, 1e-9), count), 2)])
        out += ["The 37-market baseline on the first 500, 1,000, 2,000 and 4,000 paths (SE at the worst share):", "", table(["Paths", "Serves flow from", "Retained NAV", "Cash<0 at it", "Worst served", "SE"], rows_st), ""]
    variants = [v for v in retained.VARIANTS if v != "baseline"]
    counts = sorted({n for (v, n) in by_run if v != "baseline"})
    table_rows = []
    for variant in ["baseline", *variants]:
        cells = [variant]
        for n in counts:
            run_rows = by_run.get((variant, n))
            if not run_rows:
                cells.append("-")
                continue
            got, at, worst = summary(run_rows)
            cells.append(f"{floor(got['retained_nav'])} ({pct(worst)}; {int(run_rows[0]['paths'])})")
        table_rows.append(cells)
    out += [
        "Sensitivities: retained NAV (worst cash<0 share at a served floor; paths) at the counts run.",
        "",
        table(["Variant", *[f"{n} markets" for n in counts]], table_rows),
        "",
    ]
    return "\n".join(out)


def write(derived: Path, results: Path) -> None:
    parts = [
        "# Cap calibration results",
        "",
        "Generated by `run.py report` from `data/derived/` and `results/`. Probabilities are over 90 days; refused shares are of opening notional, directional flow and churn.",
        "",
        data_section(derived),
        market_section(derived),
    ]
    if (results / "per_market_sweep.csv").exists():
        parts.append(per_market_section(results))
    parts.append(joint_section(results))
    parts.append(sensitivity_section(results))
    if (results / "retained_nav.csv").exists():
        parts.append(retained_section(results))
    (results / "report.md").write_text("\n".join(parts))
