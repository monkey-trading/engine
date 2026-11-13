"""`results/report.md` and `results/proposal.toml` from the stages' results."""

import csv
import json
from pathlib import Path

from caps import house
from caps.common import SPREAD, genesis

DAYS = 90
OPERATOR_SHARE = 0.20  # of what the house distributes (#623's launch criterion)
OPERATOR_KEEPS = 0.8  # of its own accounts' take (#549)
TIERS = house.BRACKET
MEASURE_NAMES = {"p_ever": "NAV below zero at some hour", "p_short": "house cash below zero at some hour"}


def usd(x: float | None) -> str:
    if x is None:
        return "-"
    sign, a = ("-" if x < 0 else ""), abs(x)
    for unit, scale in (("B", 1e9), ("M", 1e6), ("k", 1e3)):
        if a >= scale:
            return f"{sign}${a / scale:.3g}{unit}"
    return f"{sign}${a:.0f}"


def pct(x: float | None, digits: int = 2) -> str:
    return "-" if x is None else f"{x * 100:.{digits}f}%"


def table(header: list[str], rows: list[list]) -> list[str]:
    out = ["| " + " | ".join(header) + " |", "|" + "---|" * len(header)]
    out += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return out + [""]


def load(results: Path, name: str):
    path = results / name
    if not path.exists():
        return None
    return json.loads(path.read_text()) if path.suffix == ".json" else house.read_rows(path)


def floors_of(row: dict) -> dict:
    f = row["market_floors"]
    return json.loads(f) if isinstance(f, str) else f


def label(family: str, level: float) -> str:
    if family == "uniform":
        return f"{usd(level)} on every market"
    key, tier = family.split("@")
    name = "refusal-ending" if level == 1.0 else f"{level * 100:g}% risk level"
    return f"{name} ({MEASURE_NAMES[key].split(' at')[0]}, {tier.replace('tier', 'tier ')} alone)"


def proposal_section(results: Path) -> list[str]:
    prop = load(results, "proposal.json")
    if not prop:
        return []
    out = ["## Proposed floors", ""]
    rows = []
    for flow in ("current", "target"):
        for key, name in MEASURE_NAMES.items():
            for target in (0.01, 0.001, 0.0001):
                name_key = f"{key}@{target}" + ("" if flow == "current" else f"@{flow}")
                if name_key not in prop:
                    continue
                c = prop[name_key]
                if c is None:
                    rows.append([flow, name, pct(target, 2), "no floor set meets it at both tiers", "", "", "", ""])
                    continue
                r0, r3 = (c["rows"][str(t)] for t in TIERS)
                rows.append([flow, name, pct(target, 2), label(c["family"], c["level"]), usd(r0["total_floor"]),
                             f"{pct(r0[key])} / {pct(r3[key])}", f"{pct(r0['refused_share_directional'], 1)} / {pct(r3['refused_share_directional'], 1)}",
                             f"{'yes' if c['serves_flow'] else 'no'} ({pct(c['worst_market_refusal'], 1)})"])
    out += table(["Flow", "Measure", "Target", "Floor set", "Total floor", "Measured, tier 0 / tier 3", "Refused over 90 days, tier 0 / tier 3",
                  "Every market under 1% refused at both (worst)"], rows)
    return out


def candidates_section(results: Path) -> list[str]:
    out = ["## Every floor set at both ends of the tier range", ""]
    for tier in TIERS:
        rows = load(results, f"tier{tier}/joint.csv")
        if not rows:
            continue
        seen, body = set(), []
        for r in sorted(rows, key=lambda r: r["total_floor"]):
            key = tuple(sorted(floors_of(r).items()))
            if key in seen:
                continue
            seen.add(key)
            body.append([label(r["family"], r["level"]), usd(r["total_floor"]), pct(r["p_ever"]), pct(r["p_short"]), pct(r["p_dip_100k"]),
                         pct(r["refused_share_week1"], 1), pct(r["refused_share_directional"], 1), usd(r["nav_end_p50"]), usd(r["fee_mean"]),
                         usd(r["spread_income_mean"]), usd(r["bad_debt_mean"])])
        out += [f"### Every account at tier {tier}", ""]
        out += table(["Floor set", "Total floor", "NAV<0 ever", "Cash<0", "NAV<-$100k", "Refused, week 1", "Refused, 90 days", "NAV day 90 p50",
                      "Fees mean", "Spread income mean", "Bad debt mean"], body)
    return out


def total_section(results: Path) -> list[str]:
    out = []
    for tier in TIERS:
        rows = load(results, f"tier{tier}/total.csv")
        if not rows:
            continue
        if not out:
            out = ["## The total floor at the proposed market floors", ""]
        body = [[pct(r["level"], 0), usd(r["total_floor"]), pct(r["p_ever"]), pct(r["p_short"]), pct(r["refused_share_week1"], 1),
                 pct(r["refused_share_directional"], 1)] for r in rows]
        out += [f"Every account at tier {tier}:", ""]
        out += table(["Share of the market floors' sum", "Total floor", "NAV<0 ever", "Cash<0", "Refused, week 1", "Refused, 90 days"], body)
    return out


def stress_section(results: Path) -> list[str]:
    out = []
    for tier in TIERS:
        rows = load(results, f"tier{tier}/stress_utilisation.csv")
        choice = load(results, f"tier{tier}/stress_choice.json")
        if not rows:
            continue
        if not out:
            out = ["## Utilisation levels", ""]
        mix = market_mix([r["symbol"] for r in rows])
        families = list(dict.fromkeys(r["family"] for r in rows))
        levels = sorted({int(r["level_bps"]) for r in rows})
        body = []
        for level in levels:
            cells = [str(level)]
            for f in families:
                sel = [r for r in rows if r["family"] == f and int(r["level_bps"]) == level]
                total = sum(mix[r["symbol"]] for r in sel)
                weighted = sum(r["fire_rate"] * mix[r["symbol"]] for r in sel) / total if total else 0.0
                btc = next((r["fire_rate"] for r in sel if r["symbol"] == "BTC-USD"), None)
                cells.append(f"{pct(weighted, 1)} ({pct(btc, 1)})")
            body.append(cells)
        out += [f"Every account at tier {tier}: the share of open hours at or above each level, flow-weighted, with BTC-USD's beside it.", ""]
        out += table(["Level bps"] + families, body)
        if choice:
            out += [f"- `{f}`: level 1 at {c['utilisation_bps_l1']} (flow-weighted fire rate {pct(max(c['fire_rate_by_family'].values()), 2)}, "
                    f"worst market {pct(c['worst_market_fire_rate'], 1)})" for f, c in choice.items()] + [""]
    return out


def sensitivity_section(results: Path) -> list[str]:
    out = []
    for tier in TIERS:
        rows = load(results, f"tier{tier}/sensitivity.csv")
        if not rows:
            continue
        if not out:
            out = ["## The proposed floors under the main sensitivities", ""]
        body = [[r["scenario"], f"x{r['floor_scale']:g}", pct(r["p_ever"]), pct(r["p_short"]), pct(r["p_dip_100k"]), pct(r["refused_share_directional"], 1),
                 usd(r["nav_end_p50"]), usd(r["spread_income_mean"]), usd(r["bad_debt_mean"])] for r in rows]
        out += [f"Every account at tier {tier}, {int(rows[0]['paths'])} paths:", ""]
        out += table(["Scenario", "Floors", "NAV<0 ever", "Cash<0", "NAV<-$100k", "Refused, 90 days", "NAV day 90 p50", "Spread income mean", "Bad debt mean"], body)
    return out


def market_mix(symbols: list[str]) -> dict:
    path = house.derived() / "market_mix.csv"
    with path.open() as fh:
        mix = {r["symbol"]: float(r["share"]) for r in csv.DictReader(fh)}
    return {s: mix[s] for s in set(symbols)}


def spread_results(name: str):
    path = SPREAD / "results" / name
    return json.loads(path.read_text()) if path.exists() else None


def classes(flow: str, fill_ratio: float | None, pairs_day: float, gaps_day: float) -> dict:
    """#549's classes as the operator's take a day (80% of its accounts' profit) at the owner's fee
    curve: #697's latency takes under the 10 s curve, its held-sync and skimming figures, and this
    study's unpriced and session-gap pairs."""
    lat = spread_results("owner_latency.json")
    fx = spread_results("owner_fx.json")
    crit = spread_results("owner_criterion.json")
    curve = "100% to 10 s"
    fx_take = sum(m["curves"][curve]["3.0s"] for m in fx["markets"].values()) * OPERATOR_KEEPS
    old = crit["flows"]["current"]["classes"]
    skim = old["skimming client fills"]["operator_per_day"] * (1.0 if flow == "current" else (fill_ratio or float("nan")))
    return {
        "latency arbitrage, crypto (3 s)": (lat["curves"][curve]["operator_per_day"], True),
        "latency arbitrage, FX and indices at D6's tenths (3 s)": (fx_take, True),
        "unpriced pairs at 1% margin, tier 2, opened ahead (no financing)": (max(pairs_day, 0.0) * OPERATOR_KEEPS, True),
        "session-gap pairs, $50M a side per paying market, tier 2": (max(gaps_day, 0.0) * OPERATOR_KEEPS, True),
        "held sync's kill option": (old["held sync's kill option"]["operator_per_day"], True),
        "skimming client fills": (skim, False),
        "censoring closes": (old["censoring closes"]["operator_per_day"], False),
    }


def fastest_client() -> float:
    lat = spread_results("owner_latency.json")
    fx = spread_results("owner_fx.json")
    curve = "100% to 10 s"
    return lat["curves"][curve]["fast_per_day"] + sum(m["curves"][curve]["0.25s"] for m in fx["markets"].values())


PAIR_NOTIONAL = 50e6  # a side, as #549 sizes the operator's pairs


def gap_take(results: Path, tier: int = 2) -> float:
    """Session-gap pairs at `PAIR_NOTIONAL` a side on each market, played only across the closures of
    a kind (weekends, other closures, daily breaks) whose year pays at `tier`: the house's loss a day."""
    rows = load(results, "session_gaps.csv") or []
    pay = [r["payoff_bps_year"] for r in rows if r["tier"] == tier and r["kind"] != "all" and r["payoff_bps_year"] > 0]
    return sum(pay) / 1e4 * PAIR_NOTIONAL / 365


def income_section(results: Path) -> list[str]:
    runs = load(results, "income.json")
    pairs = load(results, "unpriced_pairs.json")
    if not runs:
        return []
    out = ["## Retained NAV, house income and the honest share", ""]
    body = []
    for name, run in runs.items():
        r = run["row"]
        if not r:
            body.append([name, usd(run["retained_nav"]), "-", "-", "-", "-", "-"])
            continue
        body.append([name, usd(run["retained_nav"]), pct(r["p_short"]), usd(r["distributed_mean"]), usd(r["spread_income_mean"]), usd(r["fee_mean"]),
                     usd(r["distributed_mean"] / DAYS * OPERATOR_SHARE)])
    out += table(["Tier, flow", "Retained NAV", "Cash<0 there", "Distributed, 90 days", "Spread income", "Profit fees", "Honest share a day"], body)
    rule = pair_rule(pairs, tier=2, opens="standing")
    pairs_day = rule["per_day"] if rule else 0.0
    gaps_day = gap_take(results)
    for tier in TIERS:
        cur, tgt = runs.get(f"tier {tier}, current"), runs.get(f"tier {tier}, target")
        if not cur or not cur["row"]:
            continue
        ratio = tgt["row"]["filled_mean"] / cur["row"]["filled_mean"] if tgt and tgt["row"] else None
        rows, totals = [], {}
        names = None
        for flow, run in (("current", cur), ("target", tgt)):
            if not run or not run["row"]:
                continue
            c = classes(flow, ratio, pairs_day, gaps_day)
            names = list(c)
            totals[flow] = (sum(v for v, _ in c.values()), sum(v for v, seen in c.values() if seen), run["row"]["distributed_mean"] / DAYS * OPERATOR_SHARE, c)
        for n in names:
            rows.append([n] + [usd(totals[f][3][n][0]) for f in totals])
        rows.append(["**All together**"] + [f"**{usd(totals[f][0])}**" for f in totals])
        rows.append([f"Honest share, every account at tier {tier}"] + [usd(totals[f][2]) for f in totals])
        out += [f"### The launch criterion, every account at tier {tier}", ""]
        out += table(["Class (the operator's take a day)"] + [f"{f.capitalize()} flow" for f in totals], rows)
        for f, (together, seen, share, _) in totals.items():
            verdict = "holds" if together < share else "fails"
            out.append(f"- {f.capitalize()} flow: the classes together take {usd(together)} a day against a share of {usd(share)}, so without the loss-rate "
                       f"trigger the criterion {verdict}; the classes the trigger sees may take at most {usd((share - (together - seen)) / OPERATOR_KEEPS)} a day of house loss.")
        out.append("")
    out += [f"The fastest honest client under the 10 s curve takes {usd(fastest_client())} a day (#697's measure); #614's decision D9 sets its budget at zero.", ""]
    return out


def pair_rule(pairs: dict | None, tier: int, opens: str, closes: str = "engine") -> dict | None:
    """The unpriced-pair rule #549 and #697 report (300 s, 40 bps) for one tier, opening and close."""
    from caps.unprice import SPELL_S, TRIGGER

    if not pairs:
        return None
    return next((r for r in pairs["rules"] if r["tier"] == tier and r["opens"] == opens and r["closes"] == closes
                 and r["spell_s"] == SPELL_S and r["trigger_bps_5s"] == TRIGGER * 1e4), None)


def unprice_section(results: Path) -> list[str]:
    p = load(results, "unpriced_pairs.json")
    if not p:
        return []
    out = ["## Paired positions across unpriced moves at 1%", "",
           f"House loss at {usd(p['pair_notional'])} a side over BTC-USD and ETH-USD, unpriced up to 300 s once the venues move 40 bps in 5 s. "
           f"On the same stress hours #697's measure gives {usd(p['check_697_stress_hours'])} and #734's "
           f"{usd(p['check_734_stress_hours']['trigger'])} reopened and {usd(p['check_734_stress_hours']['standing'])} opened ahead.", ""]
    body = []
    for tier in sorted({r["tier"] for r in p["rules"]}):
        for opens in ("trigger", "standing"):
            for closes in ("engine", "move"):
                r = pair_rule(p, tier, opens, closes)
                if r:
                    body.append([tier, pct(r["fee"], 0), opens, closes, usd(r["per_day"]), f"{usd(r['per_day_lo'])} to {usd(r['per_day_hi'])}",
                                 usd(r["stress_hours"]), r["stress_spells"], " / ".join(str(n) for n in r["stress_closing_levels"])])
    out += table(["Tier", "Fee", "Opening", "Closes at", "A day, the year", "95% interval", "2025-10-10's four stress hours", "Spells there",
                  "Closes at levels 0 / 1 / 2"], body)
    return out


def gaps_section(results: Path) -> list[str]:
    rows = load(results, "session_gaps.csv")
    if not rows:
        return []
    out = ["## Paired positions across session gaps", ""]
    body = []
    for r in rows:
        if r["kind"] not in ("all", "weekend"):
            continue
        body.append([r["symbol"], int(r["tier"]), r["kind"], pct(r["margin"], 1), f"{r['closures_year']:.0f}", f"{r['mean_abs_gap_bps']:.0f}",
                     pct(r["past_balance_share"], 1), f"{r['payoff_bps_year']:.0f}", f"{r['payoff_per_margin_year']:.2f}", f"{r['bad_debt_bps_year']:.0f}"])
    out += table(["Market", "Tier", "Closures", "Balance", "Closures a year", "Mean gap bps", "Past the balance", "Pair's payoff, bps of a leg a year",
                  "Payoff over the pair's margin, a year", "House bad debt, bps of a leg a year"], body)
    return out


def proposed_total(results: Path) -> float | None:
    """The total floor of the total-floor sweep's smallest share of the market floors' sum that, at
    both ends, meets the target and keeps every market's refusals at or under the limit, as
    `house.propose` chooses the market floors."""
    sweeps = [load(results, f"tier{t}/total.csv") for t in TIERS]
    if not all(sweeps):
        return None
    for share in sorted({r["level"] for r in sweeps[0]}):
        rows = [next(r for r in sweep if r["level"] == share) for sweep in sweeps]
        if all(r["p_short"] <= house.TARGET and house.worst_market_refusal(r) <= house.REFUSAL_LIMIT for r in rows):
            return rows[0]["total_floor"]
    return None


def proposal_toml(results: Path) -> str:
    """The proposed floors and level-1 utilisation in the genesis's fields, for F11 to apply."""
    prop = load(results, "proposal.json")
    choice = prop and prop.get(f"p_short@{house.TARGET}")
    if not choice:
        return ""
    floors = floors_of(next(iter(choice["rows"].values())))
    stress = [load(results, f"tier{t}/stress_choice.json") for t in TIERS]
    levels = [s["proposed"]["utilisation_bps_l1"] for s in stress if s]
    total = proposed_total(results) or sum(floors.values())
    lines = ["# Proposed by research/launch-caps (#500) for the owner's decision D8; not applied.",
             f"# Floor set: {label(choice['family'], choice['level'])}.", "[genesis.params]", f'house_total_cap_floor = "{total:.0f}"']
    if levels and None not in levels:
        lines += [f"# Level-1 trigger utilisation_bps: {max(levels)}, the lowest that passes at both ends of the tier range "
                  f"(tier 0 {levels[0]}, tier 3 {levels[-1]}); level 2 stays at 9500.", ""]
    ids = {m["symbol"]: m["id"] for m in genesis()["markets"]}
    for symbol, floor in sorted(floors.items(), key=lambda kv: ids[kv[0]]):
        lines += ["[[genesis.markets]]", f"id = {ids[symbol]}", f'symbol = "{symbol}"', f'house_cap_floor = "{floor:.0f}"', ""]
    return "\n".join(lines)


def write(results: Path) -> str:
    text = ["# Launch caps at the launch pricing (#500)", "",
            "Generated by `run.py report` from the stages' results; the README gives the method and reads them.", ""]
    for section in (proposal_section, total_section, stress_section, sensitivity_section, income_section, unprice_section, gaps_section, candidates_section):
        text += section(results)
    body = "\n".join(text)
    (results / "report.md").write_text(body)
    toml = proposal_toml(results)
    if toml:
        (results / "proposal.toml").write_text(toml)
    return body
