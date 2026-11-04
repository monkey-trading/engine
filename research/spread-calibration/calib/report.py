"""`results/report.md` from the stages' results: every table the README cites."""

import csv
import json
from pathlib import Path

ROGUE_HEADLINE = Path(__file__).resolve().parents[2] / "rogue-operator" / "results" / "headline.csv"
OPERATOR_KEEPS = 0.8


def usd(x) -> str:
    if x is None:
        return "-"
    a = abs(x)
    sign = "-" if x < 0 else ""
    if a >= 1e9:
        return f"{sign}${a / 1e9:.2f}B"
    if a >= 1e6:
        return f"{sign}${a / 1e6:.2f}M"
    if a >= 1e3:
        return f"{sign}${a / 1e3:.0f}k"
    return f"{sign}${a:.0f}"


def pct(x, digits=1) -> str:
    return "-" if x is None else f"{x * 100:.{digits}f}%"


def table(header, rows) -> list[str]:
    out = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    out += ["| " + " | ".join(str(c) for c in r) + " |" for r in rows]
    return out + [""]


def load(results: Path, name: str):
    path = results / name
    return json.loads(path.read_text()) if path.exists() else None


def rogue_headline() -> dict:
    with ROGUE_HEADLINE.open() as fh:
        return {(r["attack"], r["flow"]): r for r in csv.DictReader(fh)}


def shares(retained) -> dict:
    out = {"#134, genesis pricing (before this study)": 76_058_460.83 / 90 * 0.20}
    if retained:
        for name, v in retained["variants"].items():
            out[name] = v["operator_share_per_day"]
    return out


def headline(results: Path, PROPOSAL, SIZE) -> list[str]:
    frontier = load(results, "frontier.json") or []
    fx = load(results, "fx_tiers.json")
    attacks = load(results, "attacks.json")
    retained = load(results, "retained.json")
    rogue = rogue_headline()
    name = f"{PROPOSAL['peak']:.0%} to {PROPOSAL['held_to_s']:g} s, 10% at {PROPOSAL['until_s'] / 60:g} min"
    prop = next((r for r in frontier if r["schedule"] == name), None)
    alt = next((r for r in frontier if r["schedule"] == "100% to 10 s, 10% at 30 min"), None)
    flat = next((r for r in frontier if r["schedule"] == "standard 10%"), None)
    rwa = sum(m["per_day"]["1.0"]["3.0s"] for m in fx["markets"].values()) * OPERATOR_KEEPS if fx else None
    unp = {r["margin"]: r for r in (attacks or {}).get("unprice", [])
           if r["kind"] == "causal" and r["spell_s"] == 300 and r["trigger_bps_5s"] == 40 and r["markets"] == "BTC-USD+ETH-USD"}
    sd = (attacks or {}).get("stress_hours_causal_300s_40bps", {})

    def g(attack, col="operator_day"):
        r = rogue.get((attack, "launch"))
        return float(r[col]) if r else None

    def u(m):
        r = unp.get(m)
        return None if r is None else r["usd_year"] / 365 * OPERATOR_KEEPS

    def u_ci(m):
        r = unp.get(m)
        return "" if r is None else f" ({usd(r['usd_year_lo'] / 365 * OPERATOR_KEEPS)} to {usd(r['usd_year_hi'] / 365 * OPERATOR_KEEPS)})"

    rows = [
        ["Latency arbitrage, crypto, operator's 3 s view", f"{usd(g('latency arbitrage, score laundered'))} (score laundered); {usd(g('latency arbitrage, top toxicity tier paid'))} (add-on paid)",
         usd(prop and prop["operator_latency_per_day"]),
         f"{usd(flat and flat['operator_latency_per_day'])} at a flat 10% fee; {usd(alt and alt['operator_latency_per_day'])} with the 10 s variant"],
        ["Latency arbitrage, FX and indices at a 1 bp first tier", "not measured (zero first tier)", usd(rwa),
         "an upper bound: full size at the first tier, no stick; zero on EUR-USD and GBP-USD"],
        ["Skimming client fills (3 s look-back)", usd(g("skimming client fills (3 s look-back, no delay)")),
         f"{usd(g('skimming client fills (3 s look-back, no delay)'))} at current flow, about a quarter of it at target flow",
         "spreads and the fee do not change it; it scales with client fills"],
        ["Censoring closes 10 s", usd(g("censoring closes for 10 s, over pricing them at arrival")), "no gain", "unchanged"],
        ["Held sync's kill option", usd(g("held sync's kill option, 4 restarts a day (BTC-USD)")), "about the same", "bounded by restarts"],
        ["Unpriced pairs at 1% margin (full leverage), BTC-USD and ETH-USD", usd(g("unpricing across moves, $50M pairs (spells to 300 s, 40 bps trigger)")),
         usd(u(0.01)) + u_ci(0.01), f"stress day's four hours: {usd(sd.get('0.01', 0) * OPERATOR_KEEPS)}"],
        ["Unpriced pairs at 2% margin (tier 1's leverage)", "-", usd(u(0.02)) + u_ci(0.02), f"stress day: {usd(sd.get('0.02', 0) * OPERATOR_KEEPS)}; stops paying"],
        ["Unpriced pairs at 5% margin (a fresh wallet's leverage)", "-", usd(u(0.05)) + u_ci(0.05), f"stress day: {usd(sd.get('0.05', 0) * OPERATOR_KEEPS)}; stops paying"],
        ["Altering Databento quotes", usd(g("altering Databento quotes, 3 bps a fill")), usd(g("altering Databento quotes, 3 bps a fill")),
         "unchanged: stops only with authenticated quotes or a second independent source"],
        ["Choosing the venue if `min_sources` were 1", usd(g("choosing the venue if min_sources = 1")), "not applicable", "production requires two sources"],
    ]
    combined = sum(x for x in (prop and prop["operator_latency_per_day"], rwa, g("skimming client fills (3 s look-back, no delay)"),
                               max(u(0.01) or 0.0, 0.0), g("held sync's kill option, 4 restarts a day (BTC-USD)")) if x)
    out = ["## Headline: each attack class against the launch criterion", "",
           "The operator's take a day at launch flow, at genesis values (#549) and at this study's proposals: the calibrated "
           f"spreads, the holding-time fee ({name}), the toxicity score and the volume tiers. Unpriced pairs are #549's causal "
           "rule (5 s trigger of 40 bps, spells to 300 s) on $50M a side with 95% intervals. The operator keeps 80% of what its own "
           "accounts take and 20% of what the house gains.", ""]
    out += table(["Attack class", "Genesis (#549)", "Proposed", "Note"], rows)
    out += [f"All the classes but the Databento quotes together, at the proposal: about {usd(combined)} a day; with them, "
            f"about {usd(combined + (g('altering Databento quotes, 3 bps a fill') or 0))}.", ""]
    out += ["The operator's honest share a day, the launch criterion's yardstick:", ""]
    out += table(["Flow and pricing", "Share a day"], [[k, usd(v)] for k, v in shares(retained).items()])
    return out


def spreads_section(results: Path) -> list[str]:
    rows = load(results, "spreads.json")
    fx = load(results, "fx_tiers.json")
    out = ["## Spread tiers", "",
           "Each market's best tier covers the mean index move over #510's 3 s price-lag bound in its 90th-percentile open hour, "
           "plus ordinary flow's edge (0.18 bps), rounded up to the whole basis points the engine stores (`Tier.bps` is an "
           "integer); larger sizes pay twice and four times it, at genesis's size bounds. Spreads apply to each fill.", ""]
    body = []
    for r in rows:
        tiers = ", ".join(f"{t['bps']} bps" + (f" to {usd(float(t['up_to']))}" if t.get("up_to") else " above") for t in r["tiers"])
        gen = ", ".join(f"{t['bps']}" for t in r["genesis_tiers"])
        body.append([r["symbol"], f"{r['move_3s_p90_bps']:.2f} ({r['source']})", tiers, gen,
                     f"{r['norm_low_bps']:g} to {r['norm_high_bps']:g} ({r['against_norm']})"])
    out += table(["Market", "3 s move, bps", "Proposed tiers", "Genesis tiers, bps", "Published retail norm, bps"], body)
    if fx:
        out += ["### FX and index first tiers", "",
                "FX majors need about 0.4 bps (a 0.2 bp move over the 3 s window plus 0.18 bps), indices 0.7 to 1.0: the 1 bp first "
                "tier is the engine's whole-basis-point unit, not the lag window. A tighter tier needs a finer spread unit, which "
                "is a new protocol version, and is the owner's choice. What each first tier leaves to latency arbitrage a day per "
                "market, on an operator-sized position (BTC-USD's measured strategy scaled by each market's move, under the "
                "proposed fee, with no stick or size tiers, so an upper bound). The reference is 0.1 bp to $2M, 1 bp to $10M and "
                "3 bps above:", ""]
        body = []
        for m, r in fx["markets"].items():
            body.append([m] + [f"{usd(r['per_day'][k]['3.0s'])} / {usd(r['per_day'][k]['0.25s'])}" for k in ("0.1", "0.25", "0.5", "1.0", "reference")])
        out += table(["Market (3 s view / 0.25 s lead)", "0.1 bp", "0.25 bp", "0.5 bp", "1 bp", "Reference"], body)
    return out


def fee_section(results: Path, PROPOSAL) -> list[str]:
    frontier = load(results, "frontier.json") or []
    fee = load(results, "fee.json")
    out = ["## The holding-time fee", "",
           "The target client holds 15 minutes or more (#614); shorter holds are flow a zero fee pays for. Each schedule below "
           "holds a peak fee up to a hold and falls linearly in the logarithm of the hold to 10% at its end. The target-flow "
           "cost is the extra fee as a share of gross profit on holds of 15 minutes or more; the short-hold column is the "
           "average fee on gross profit under 15 minutes. Latency takes are a day over BTC-USD, ETH-USD and SOL-USD at "
           "$6.9M a market, after the spreads, size tiers and the stick, the better of keeping a wallet and recycling.", ""]
    body = [[r["schedule"], pct(r["target_extra_share"]), pct(r["short_fee_share"], 0), usd(r["operator_latency_per_day"]),
             usd(r["fast_take_per_day"])] for r in frontier
            if r["schedule"] == "standard 10%" or "10% at 15 min" in r["schedule"] or "10% at 30 min" in r["schedule"] or "10% at 60 min" in r["schedule"]]
    out += table(["Schedule", "Target flow, extra", "Short holds, fee", "Operator, 3 s", "Fastest client, 0.25 s"], body)
    if fee:
        hc = fee["proposal_honest_cost"]
        out += [f"The proposal holds {PROPOSAL['peak']:.0%} to {PROPOSAL['held_to_s'] / 60:g} minutes and reaches 10% at "
                f"{PROPOSAL['until_s'] / 60:g}: target flow pays {pct(hc['target']['extra_fee_share_of_profit'])} more of its gross "
                f"profit, the short holds {pct(hc['short']['extra_fee_share_of_profit'] + 0.10, 0)} of theirs, and the #111 mix "
                f"as it stands {pct(hc['all']['extra_fee_share_of_profit'])} more. The 10 s variant costs target flow less and keeps "
                "the operator's latency take about the same, but leaves strategies held from seconds to a few minutes about half of "
                "their gains.", ""]
    return out


def toxicity_section(results: Path) -> list[str]:
    tox = load(results, "toxicity.json")
    if not tox:
        return []
    out = ["## The toxicity score", "",
           f"Markouts at {tox['horizon_s']:g} s in units of the trailing hour's markout spread (floored at half the year's), "
           f"weights capped at {tox['weight_cap']:g} times the account's mean fill, decaying with a {tox['half_life_days']:g}-day "
           f"half-life, no tier before {tox['min_fills']} fills; tiers at scores of 4 and 8 add 2 and 5 bps.", ""]
    body = []
    for m, r in tox["markets"].items():
        t = r["trailing"]
        body.append([m, f"{r['sigma_bps']:.2f}", f"{t['honest_kurtosis']:.0f}", f"{t['attack_edge_z']:.2f}",
                     f"{t['fills_to_tier']['4.0']:.0f}", pct(t["honest_reach"]["1000"]["4.0"], 2), pct(t["honest_reach"]["1000"]["8.0"], 2),
                     f"{t['dilution']['4.0']['share']:.1f}x"])
    out += table(["Market", "Markout spread, bps", "Kurtosis", "Latency fills' edge, z", "Fills to tier 1",
                  "Honest accounts reaching tier 1 in 1,000 fills", "Tier 2", "Diluting under tier 1, cost over edge"], body)
    return out


def carrot_section(results: Path) -> list[str]:
    c = load(results, "carrot.json")
    if not c:
        return []
    out = ["## Volume tiers", "",
           "Counted volume over 30 days, but for same-index round trips within the score's horizon and any part closed within "
           f"{c['min_hold_s']:g} s; a day counts at most a {c['days'][1]}th, {c['days'][2]}th and {c['days'][3]}th of tiers 1, 2 "
           "and 3's thresholds.", ""]
    names = ["Threshold", "Requests a second", "Best price, share of first bound", "Leverage, share of maximum", "Fee floor",
             "Financing discount", "Emissions boost"]
    body = [[usd(t["volume"]), f"{t['requests_per_s']:g}", f"{t['best_price_share']:g}", f"{t['leverage_share']:g}",
             pct(t["fee_floor"], 0), pct(t["financing_discount"], 0), f"{t['emissions_boost']:g}x"] for t in c["tiers"]]
    out += table(names, body)
    body = []
    for m, r in c["markets"].items():
        for lag, v in r["strategies"].items():
            a = v.get("at_recycler_best", {})
            body.append([m, lag, usd(v["keeper"]["per_day"]), usd(v["recycler"]["per_day"]), usd(a.get("gain_per_day")),
                         usd(a.get("forfeited_per_day")), f"{r['farming_cost_bps']:.1f}"])
    out += ["What recycling wallets gains and forfeits a day, at the latency strategy's best under the proposals:", ""]
    out += table(["Market", "View", "Keeping a wallet", "Recycling", "Add-on avoided", "Benefits and wallets forfeited",
                  "Farming cost, bps of counted volume"], body)
    return out


def retained_section(results: Path) -> list[str]:
    r = load(results, "retained.json")
    if not r:
        return ["## Retained NAV", "", "Not yet run.", ""]
    out = ["## Retained NAV and house income at launch (37 markets)", "",
           f"#134's retained NAV ({r['paths']} paths, 90 days) at the genesis margins (100x on BTC-USD and ETH-USD). Target flow "
           f"keeps the within-hour flow held 15 to 60 minutes ({pct(r['target_within_hour_share'])} of it) and "
           f"{pct(r['keep_short'], 0)} of the rest.", ""]
    body = []
    for name, v in r["variants"].items():
        row = v["row"] or {}
        body.append([name, usd(v["retained_nav"]), usd(row.get("distributed_mean")), usd(row.get("spread_income_mean")),
                     usd(row.get("fee_mean")), usd(row.get("bad_debt_mean")), usd(v["operator_share_per_day"])])
    out += table(["Variant", "Retained NAV", "Distributed, 90 days", "Spread income", "Profit fees", "Bad debt", "Operator's share a day"], body)
    notes = []
    for name, v in r["variants"].items():
        rows = sorted(v["rows"], key=lambda x: x["floor"])
        below = [x for x in rows if v["retained_nav"] is not None and x["floor"] < v["retained_nav"]]
        cash_safe = all(x["p_short"] <= 0.01 for x in rows)
        if below and cash_safe:
            notes.append((name, below[-1]["floor"], below[-1]["refused_share_directional"]))
    if len(notes) == len(r["variants"]):
        lo = min(n[2] for n in notes)
        out += [f"In every variant the floor is set by the exposure caps' refusals, not by house cash: house cash stays below zero "
                f"in at most 1% of paths at every floor of the grid, while at the grid's next floor down, {usd(notes[0][1])}, "
                f"{pct(lo)} or more of directional orders are refused. Spread income raises what is distributed, not what must be "
                "retained.", ""]
    return out


def per_market_section(results: Path) -> list[str]:
    p = load(results, "per_market_100x.json")
    if not p:
        return []
    out = ["## #500: BTC-USD and ETH-USD at 100x", "",
           "Each market alone at its share of the flow (#111's per-market stage): the floor where its refusals end, and the chance "
           "NAV ever falls below zero, bad debt (close-outs gapping past a balance) and profit fees there.", ""]
    body = []
    for sym in ("BTC-USD", "ETH-USD"):
        c = p["committed_10x"].get(sym)
        body.append([sym, "10x, committed", usd(c and c["floor"]), pct(c and c["p_ever"], 2), usd(c and c["bad_debt_mean"]),
                     usd(c and c["bad_debt_p99"]), usd(c and c["fee_mean"])])
        for name, v in p["variants"].items():
            a = v[sym]["at_binding"] or {}
            body.append([sym, f"100x, {name}", usd(v[sym]["binding_floor"]) if v[sym]["binding_floor"] is not None else "> grid",
                         pct(a.get("p_ever"), 2), usd(a.get("bad_debt_mean")), usd(a.get("bad_debt_p99")), usd(a.get("fee_mean"))])
    out += table(["Market", "Margins and pricing", "Refusals end at", "NAV<0 ever", "Bad debt mean", "Bad debt p99", "Fees mean"], body)
    gaps = load(results, "gaps.json")
    if gaps:
        out += ["The hourly model closes positions out at their trigger, so it shows no gap bad debt on markets that never close. "
                "On the trade grids (`run.py gaps`), the seconds a day, over BTC-USD and ETH-USD, in which the index moves past the "
                "half of the margin left at the trigger within the engine's next price, and the mean loss past the balance of a "
                "position closed out in them, as a share of its notional:", ""]
        body = []
        for r in gaps["year"]:
            sh = gaps["stress_hours"].get(f"{r['lag_s']}s@{r['margin']}", {"seconds": 0.0, "excess_sum": 0.0})
            mean_stress = sh["excess_sum"] / sh["seconds"] if sh["seconds"] else 0.0
            body.append([pct(r["margin"], 0), f"{r['lag_s']:g} s", f"{r['seconds_per_day']:.2f} (to {r['seconds_per_day_hi']:.2f})",
                         pct(r["mean_excess"], 2), f"{sh['seconds']:.0f}", pct(mean_stress, 2)])
        out += table(["Margin", "Next price within", "Seconds a day, year", "Mean loss past balance", "Seconds, four stress hours",
                      "Mean loss past balance"], body)
    return out


def write(results: Path, PROPOSAL, SIZE) -> str:
    parts = ["# Spread, fee and toxicity calibration: results", "",
             "Generated by `python -I run.py report` from the stages' results.", ""]
    parts += headline(results, PROPOSAL, SIZE)
    parts += spreads_section(results)
    parts += fee_section(results, PROPOSAL)
    parts += toxicity_section(results)
    parts += carrot_section(results)
    parts += retained_section(results)
    parts += per_market_section(results)
    text = "\n".join(parts).rstrip() + "\n"
    (results / "report.md").write_text(text)
    return text
