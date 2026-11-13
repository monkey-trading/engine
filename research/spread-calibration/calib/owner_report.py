"""`results/owner_values.md` and `results/owner_criterion.json`: the corrected headline at the owner's
values (#697), from the `owner_*` stages' results.

The launch criterion (#623) is that every attack class #549 measured, together, takes less a day than
the operator's honest share, 20% of what the house distributes. The owner rests it, at the 1 s fee
schedule, on the loss-rate stress trigger (#695): this states, for each flow, the most the classes
that trigger can reach may take a day for the criterion to hold, alone and together.
"""

import json
from pathlib import Path

from calib.fee import BASE, LAUNCH_DISCOUNTS
from calib.report import OPERATOR_KEEPS, load, pct, rogue_headline, table, usd

DAYS = 90
OPERATOR_SHARE = 0.20
FLOWS = ("current", "target")
SKIM = "skimming client fills (3 s look-back, no delay)"
SYNC = "held sync's kill option, 4 restarts a day (BTC-USD)"
CENSOR = "censoring closes for 10 s, over pricing them at arrival"
LAUNCH = f"{BASE}/{LAUNCH_DISCOUNTS[3]}"  # launch's base and tier 3's discount, the key `owner_fx` records each fee under
PAIR_FEE = round(BASE * (1 - LAUNCH_DISCOUNTS[3]), 4)  # a pair at 1% margin pays tier 3's fee past the curve


def share(variant: dict | None) -> float | None:
    return None if not variant or not variant["row"] else variant["row"]["distributed_mean"] / DAYS * OPERATOR_SHARE


def income(results: Path) -> dict:
    return (load(results, "owner_income.json") or {"variants": {}})["variants"]


def fx_total(fx: dict, candidate: str, lag: str, measure: str = "best", key: str = LAUNCH) -> float:
    """The FX and index markets' take a day (house loss) at a candidate."""
    c = (fx["markets"][m]["candidates"][candidate] for m in fx["markets"])
    if measure == "best":
        return sum(x["best"][key][lag]["per_day"] for x in c)
    return sum(x["upper"][key][lag] for x in c)


def pair(unprice: dict, fee_share: float, margin: float = 0.01) -> dict:
    """Unpriced pairs on BTC-USD and ETH-USD at `margin`: the year a day and its interval, and the
    four stress hours, as house losses."""
    f = unprice["fees"][str(fee_share)]
    row = next(t for t in f["unprice"] if t["markets"] == "BTC-USD+ETH-USD" and t["margin"] == margin)
    return {"day": row["usd_year"] / 365, "lo": row["usd_year_lo"] / 365, "hi": row["usd_year_hi"] / 365,
            "stress": f["stress_hours"][str(margin)]}


def budget(share_per_day: float, classes: dict) -> dict:
    """Whether `classes` ({name: (operator's take a day, whether the trigger sees it)}) together stay
    below the share, and what the classes the trigger sees may take for them to: the share less the
    classes it does not see, as the operator's take and as the house loss behind it (the operator
    keeps 80%), with the share of their unbounded take the trigger must cut."""
    seen = sum(v for v, reach in classes.values() if reach)
    unseen = sum(v for v, reach in classes.values() if not reach)
    room = share_per_day - unseen
    return {"holds": seen + unseen < share_per_day, "operator_budget_per_day": room,
            "house_loss_budget_per_day": room / OPERATOR_KEEPS,
            "cut_needed_share": max(0.0, 1 - room / seen) if seen > 0 else 0.0}


def criterion(results: Path) -> dict:
    """Per flow: each class's take a day as the operator's (80% of its accounts' profit, 20% of the
    house's gains), whether the loss-rate trigger can see it (the house pays it), the honest share
    (a lower bound, and the central estimate), and the budget the trigger must hold the classes it
    sees to."""
    lat = load(results, "owner_latency.json")
    fx = load(results, "owner_fx.json")
    unp = load(results, "owner_unprice.json")
    runs = income(results)
    rogue = rogue_headline()

    def g(attack):
        return float(rogue[(attack, "launch")]["operator_day"])

    cur, tgt = runs.get("current, launch, bound"), runs.get("target, launch, bound")
    fill_ratio = tgt["row"]["filled_mean"] / cur["row"]["filled_mean"] if cur and tgt else None
    p = pair(unp, PAIR_FEE)
    fx_best, fx_upper = fx_total(fx, "tenths", "3.0s") * OPERATOR_KEEPS, fx_total(fx, "tenths", "3.0s", "upper") * OPERATOR_KEEPS
    out = {"flows": {}, "fill_ratio_target": fill_ratio, "fx_upper_less_best": fx_upper - fx_best}
    for flow in FLOWS:
        skim = g(SKIM) * (1.0 if flow == "current" else (fill_ratio or float("nan")))
        classes = {
            "latency arbitrage, crypto (3 s)": (lat["launch"]["operator_per_day"], True),
            "latency arbitrage, FX and indices at D6's tenths (3 s)": (fx_best, True),
            "unpriced pairs at 1% margin, tier 3's fee": (max(p["day"], 0.0) * OPERATOR_KEEPS, True),
            "held sync's kill option": (g(SYNC), True),
            "skimming client fills": (skim, False),
            "censoring closes": (max(g(CENSOR), 0.0), False),
        }
        s_bound = share(runs.get(f"{flow}, launch, bound"))
        s_central = share(runs.get(f"{flow}, launch, central"))
        seen = sum(v for v, reach in classes.values() if reach)
        unseen = sum(v for v, reach in classes.values() if not reach)
        res = {"classes": {k: {"operator_per_day": v, "trigger_sees": r} for k, (v, r) in classes.items()},
               "together": seen + unseen, "seen": seen, "unseen": unseen,
               "share_bound": s_bound, "share_central": s_central}
        for name, s in (("bound", s_bound), ("central", s_central)):
            if s is None:
                continue
            res[f"at_{name}"] = budget(s, classes) | {
                "alone": {k: {"holds": v < s, "house_loss_budget_per_day": s / OPERATOR_KEEPS} for k, (v, r) in classes.items()},
                "stress_day_pairs_house_loss": p["stress"], "stress_day_holds": p["stress"] * OPERATOR_KEEPS + unseen < s,
            }
        out["flows"][flow] = res
    crypto_fast = lat["launch"]["fast_per_day"]
    fx_fast = fx_total(fx, "tenths", "0.25s")
    out["fastest_client"] = {"crypto": crypto_fast, "fx_and_indices": fx_fast, "total": crypto_fast + fx_fast,
                             "budget_per_day": 0.0}
    out["pairs"] = p
    return out


def criterion_section(results: Path, c: dict) -> list[str]:
    out = ["## The launch criterion at the owner's values", ""]
    flows = c["flows"]
    names = list(flows["current"]["classes"])
    rows = [[n, "yes" if flows["current"]["classes"][n]["trigger_sees"] else "no (clients pay)"]
            + [usd(flows[f]["classes"][n]["operator_per_day"]) for f in FLOWS] for n in names]
    rows.append(["**All together**", "", *(f"**{usd(flows[f]['together'])}**" for f in FLOWS)])
    rows.append(["Honest share, lower bound (tier 3's fee, the least fee churn's hold moments allow)", "",
                 *(usd(flows[f]["share_bound"]) for f in FLOWS)])
    rows.append(["Honest share, central estimate", "", *(usd(flows[f]["share_central"]) for f in FLOWS)])
    out += ["The operator's take a day by attack class, as #623 counts it (80% of its own accounts' profit), against its "
            "honest share (20% of the house's distributions). The loss-rate trigger (#695) can see a class only if the "
            "house pays it.", ""]
    out += table(["Class", "Trigger sees it", "Current flow", "Target flow"], rows)
    lines = []
    for f in FLOWS:
        a = flows[f].get("at_bound")
        if not a:
            continue
        verdict = "holds" if a["holds"] else "fails"
        cut = (f"so the trigger must cut at least {pct(a['cut_needed_share'], 0)} of it" if a["cut_needed_share"] > 0
               else "which is within it without the trigger")
        lines.append(f"- **{f.capitalize()} flow:** without the trigger the criterion {verdict}: {usd(flows[f]['together'])} a day "
                     f"against a share of at least {usd(flows[f]['share_bound'])}. The classes the trigger sees may take at most "
                     f"{usd(a['operator_budget_per_day'])} a day as the operator's take, that is **at most "
                     f"{usd(a['house_loss_budget_per_day'])} a day of house loss to them**. Unbounded they take "
                     f"{usd(flows[f]['seen'] / OPERATOR_KEEPS)} of house loss a day, {cut}.")
    out += lines + [""]
    extra = c["fx_upper_less_best"]
    out += ["The FX and index take is measured as the crypto takes are, with the size tiers, the stick and recycling. On "
            f"#623's upper bound for it (the whole position at the first tier, no stick) it is {usd(extra)} a day higher, and "
            f"all the classes together take {usd(flows['current']['together'] + extra)} on current flow and "
            f"{usd(flows['target']['together'] + extra)} on target flow.", ""]
    return out


def alone_section(c: dict) -> list[str]:
    out = ["### Each class alone, the stress day and the fastest honest client", ""]
    rows = []
    for name in c["flows"]["current"]["classes"]:
        cells = [name]
        for f in FLOWS:
            fl = c["flows"][f]
            a = fl.get("at_bound")
            take = fl["classes"][name]["operator_per_day"]
            cells.append("-" if not a else ("below the share" if a["alone"][name]["holds"] else f"above the share by {usd(take - fl['share_bound'])}"))
        rows.append(cells)
    p = c["pairs"]
    stress = p["stress"] * OPERATOR_KEEPS
    rows.append(["unpriced pairs on 2025-10-10's four stress hours, in one day",
                 *(f"above the share by {usd(stress - c['flows'][f]['share_bound'])}" if c["flows"][f]["share_bound"] else "-" for f in FLOWS)])
    out += table(["Class alone", "Current flow", "Target flow"], rows)
    out += [f"Unpriced pairs at 1% margin take {usd(p['day'] * OPERATOR_KEEPS)} a day on the year (95% interval "
            f"{usd(p['lo'] * OPERATOR_KEEPS)} to {usd(p['hi'] * OPERATOR_KEEPS)}) at tier 3's 8% fee, but "
            f"{usd(p['stress'] * OPERATOR_KEEPS)} over 2025-10-10's four stress hours ({usd(p['stress'])} of house loss). "
            "On such a day the trigger must hold the house's loss to the classes it sees within the same daily budget.", ""]
    fc = c["fastest_client"]
    out += [f"The fastest honest client (an honest node's 0.25 s lead, recycling wallets at tier 0) takes {usd(fc['total'])} a day: "
            f"{usd(fc['crypto'])} on the crypto markets and {usd(fc['fx_and_indices'])} on the FX and index markets at D6's tenths. "
            "#614's decision D9 requires the trigger to bring it to zero or less, so its budget is zero.", ""]
    return out


INCOME_ROWS = (("current, launch, bound", "Current flow, lower bound"), ("current, launch, central", "Current flow, central estimate"),
               ("target, launch, bound", "Target flow, lower bound"), ("target, launch, central", "Target flow, central estimate"))


def income_section(results: Path) -> list[str]:
    runs = income(results)
    old = load(results, "retained.json") or {"variants": {}}
    out = ["## The corrected income model", "",
           "#623's model charged churn (flow held under an hour) the profit fee on its gross price gain at the class's mean "
           "hold and added both fills' spreads beside it. The engine charges the fee on realised PnL at the spread-adjusted "
           "fill prices, so the fee is on the gain after both spreads. The model now charges each churn class "
           "`fee(h) * sigma * sqrt(h) * psi(2 s / (sigma * sqrt(h)))` over its holds, and positions held an hour or more the fee "
           "on their realised PnL less the spread (`research/cap-calibration/model/holdfee.py`). The flow records only each "
           "churn class's mean hold and mean square-root hold, so the lower bound takes the least fee any distribution of "
           "holds with those two moments allows (a linear programme on three holds); the central estimate tilts the measured "
           "within-hour holding profile to the same moments. Every account pays tier 3's fee (8% past the curve), the least "
           "any tier pays. #614's decision D5 is not modelled: every account trades at the market's own leverage, as tiers 2 "
           "and 3 do, while one below tier 2 would pay a higher fee on less notional where the market allows more than 20x "
           "(tier 0) or 50x (tier 1). The flow data does not say how volume spreads across tiers.", "",
           "Spreads are each market's first tier on both fills: D6's tenths on FX and indices, #623's whole bps elsewhere. "
           "Larger fills would pay more, so spread income is also understated.", ""]
    rows = []
    for key, label in INCOME_ROWS:
        v = runs.get(key)
        if not v or not v["row"]:
            continue
        r = v["row"]
        rows.append([label, usd(v["retained_nav"]), usd(r["spread_income_mean"]), usd(r["fee_mean"]), usd(r["churn_fee_mean"]),
                     usd(r["fee_mean"] - r["churn_fee_mean"]), usd(r["distributed_mean"]), usd(share(v)), str(v["paths"])])
    for name in ("current flow, spreads and fee", "target flow, spreads and fee"):
        v = old["variants"].get(name)
        if v and v["row"]:
            r = v["row"]
            rows.append([f"#623 at its proposal, {name.split(',')[0]}", usd(v["retained_nav"]), usd(r["spread_income_mean"]),
                         usd(r["fee_mean"]), "-", "-", usd(r["distributed_mean"]), usd(v["operator_share_per_day"]), str(old["paths"])])
    out += table(["Flow and model", "Retained NAV", "Spread income, 90 days", "Profit fees", "of which churn", "of which an hour or more",
                  "Distributed", "Operator's share a day", "Paths"], rows)
    corr = []
    for flow in FLOWS:
        v = runs.get(f"{flow}, launch, bound")
        if v and v["row"]:
            r = v["row"]
            corr.append([flow, usd(r["churn_fee_gross_mean"]), usd(r["churn_fee_central_mean"]), usd(r["churn_fee_mean"]),
                         usd((r["churn_fee_gross_mean"] - r["churn_fee_mean"]) / DAYS * OPERATOR_SHARE),
                         usd((r["churn_fee_gross_mean"] - r["churn_fee_central_mean"]) / DAYS * OPERATOR_SHARE)])
    if corr:
        out += ["Churn's profit fee over 90 days at the owner's values, three ways: as #623 charged it (the fee at the mean hold "
                "on the gain before spreads), the central estimate, and the lower bound the share uses. The last two columns "
                "are what #623's way adds to the share a day over each.", ""]
        out += table(["Flow", "#623's way", "Central", "Lower bound", "Over the lower bound, a day", "Over the central, a day"], corr)
    return out


def latency_section(results: Path) -> list[str]:
    lat = load(results, "owner_latency.json")
    out = ["## Latency arbitrage on the crypto markets", "",
           f"At the owner's fee on the engine's table, the operator's 3 s view takes {usd(lat['launch']['operator_per_day'])} a day "
           f"and the fastest honest client {usd(lat['launch']['fast_per_day'])}, after the spreads, the size tiers and the stick, "
           f"the better of keeping a wallet and recycling. #623 measured {usd(lat['log_linear_d3']['operator_per_day'])} on the "
           "log-linear curve the table rounds up. The best strategy on every market recycles wallets at tier 0, so tier "
           "discounts never reach it:", ""]
    rows = []
    for lag, who in (("3.0s", "Operator, 3 s"), ("0.25s", "Fastest client, 0.25 s")):
        for m, v in lat["launch"]["takes"][lag]["markets"].items():
            take = v["per_day"] * (OPERATOR_KEEPS if lag == "3.0s" else 1.0)
            rows.append([who, m, usd(take), f"{v['hold_s']:g} s", f"{v['theta_bps']:g} bps", usd(v["size"]), v["who"]])
    out += table(["Who", "Market", "Take a day", "Hold", "Threshold", "Size", "Wallet"], rows)
    return out


def fx_section(results: Path) -> list[str]:
    fx = load(results, "owner_fx.json")
    out = ["## FX and index first tiers (D6)", "",
           "Each market's latency take at D6's tenths (the 3 s move plus ordinary flow's 0.18 bps, rounded up to the tenth), "
           "at 1 bp and at the owner's reference tiers (0.1 bp to $2M, 1 bp to $10M, 3 bps above), under the owner's fee, "
           "at each market's exact spread. `Best` is measured as the crypto markets are (size tiers, the stick, recycling); "
           "`upper` is #623's bound (the whole $6.9M position at the candidate's spread, no stick). House loss a day; the "
           "operator keeps 80% of it.", ""]
    rows = []
    for m, r in fx["markets"].items():
        cells = [m, f"{r['tenths_bps']:g}"]
        for cand in ("tenths", "1 bp", "reference"):
            c = r["candidates"][cand]
            cells.append(f"{usd(c['best'][LAUNCH]['3.0s']['per_day'])} / {usd(c['upper'][LAUNCH]['3.0s'])}")
        cells.append(usd(r["candidates"]["tenths"]["best"][LAUNCH]["0.25s"]["per_day"]))
        rows.append(cells)
    tot = [f"**{len(fx['markets'])} markets**", ""]
    for cand in ("tenths", "1 bp", "reference"):
        tot.append(f"**{usd(fx_total(fx, cand, '3.0s'))} / {usd(fx_total(fx, cand, '3.0s', 'upper'))}**")
    tot.append(f"**{usd(fx_total(fx, 'tenths', '0.25s'))}**")
    rows.append(tot)
    out += table(["Market", "D6 tenths, bps", "3 s at the tenths, best / upper", "3 s at 1 bp", "3 s at the reference",
                  "0.25 s at the tenths, best"], rows)
    return out


def bases_section(results: Path) -> list[str]:
    lat = load(results, "owner_latency.json")
    fx = load(results, "owner_fx.json")
    unp = load(results, "owner_unprice.json")
    fee_rows = load(results, "owner_fee.json")["rows"]
    runs = income(results)
    out = ["## Across the fee bases and tier discounts the bounds admit (D7)", "",
           "The fee is `max(base x (1 - d_tier), C(hold))`. The bounds admit a base from launch's 10% to 15% (the owner "
           "refuses any cut below launch, #614, 2026-11-12) and tier discounts no larger than launch's (0, 0, 10%, 20%), "
           "ordered by tier. Pairs pay tier 3's fee; the honest share runs every account at tier 3.", ""]
    rows = []
    for g in lat["grid"]:
        if tuple(g["discounts"]) != tuple(LAUNCH_DISCOUNTS):
            continue
        base = g["base"]
        key = f"{base}/{LAUNCH_DISCOUNTS[3]}"
        pair_fee = round(base * (1 - LAUNCH_DISCOUNTS[3]), 4)
        p = pair(unp, pair_fee) if str(pair_fee) in unp["fees"] else None
        shares = []
        for flow in FLOWS:
            name = f"{flow}, launch, bound" if base == BASE else f"{flow}, base {base:.1%}".replace(".0%", "%") + ", bound"
            shares.append(usd(share(runs.get(name))) if runs.get(name) else "-")
        rows.append([f"{base * 100:g}%", usd(g["operator_per_day"]), usd(fx_total(fx, "tenths", "3.0s", key=key) * OPERATOR_KEEPS),
                     usd(p["day"] * OPERATOR_KEEPS) if p else "-", usd(p["stress"] * OPERATOR_KEEPS) if p else "-",
                     usd(g["fast_per_day"] + fx_total(fx, "tenths", "0.25s", key=key)), *shares])
    out += table(["Base", "Crypto latency, operator", "FX and indices, operator", "Pairs at 1%", "Pairs, stress hours",
                  "Fastest client", "Share, current", "Share, target"], rows)
    admitted = [r for r in fee_rows if r["base"] >= BASE]
    cut = [r for r in fee_rows if r["base"] < BASE]
    out += [f"On all {len(admitted)} admitted combinations of base and discounts (5-point steps), checked at every hundredth of a "
            "second to an hour on the engine's table: every tier's fee falls with the hold "
            f"({'yes' if all(r['falls_with_hold'] for r in admitted) else 'no'}), a higher tier never pays more than a lower one at "
            f"the same hold ({'yes' if all(r['higher_tier_never_pays_more'] for r in admitted) else 'no'}), and every tier pays at "
            f"least its launch fee at every hold ({'yes' if all(r['held_from_below'] for r in admitted) else 'no'}). The ordering "
            "guarantee and the protection of the calibrated fee from below hold.", ""]
    no_disc = {f: runs.get(f"{f}, no discount, bound") for f in FLOWS}
    if all(no_disc.values()):
        out += [f"Smaller tier discounts raise the honest share: with none, every account paying tier 0's fee, it is "
                f"{usd(share(no_disc['current']))} on current flow and {usd(share(no_disc['target']))} on target flow. That is tier 3's "
                "fee at a 12.5% base (12.5% less 20% is 10%), so the two runs coincide. They also raise the fee unpriced pairs "
                "pay. The latency takes do not move: their best strategies recycle wallets at tier 0, which has no discount.", ""]
    out += [f"**The long-hold cut the owner ruled out.** At a 5% base with the curve unchanged, a tier pays up to "
            f"{max(r['most_below_launch'] for r in cut) * 100:g} percentage points less than its launch fee from "
            f"{cut[0]['first_hold_below_launch_s']:g} s, "
            f"so the protection from below fails; the operator's crypto take rises to {usd(lat['cut']['operator_per_day'])} and the "
            f"fastest client's to {usd(lat['cut']['fast_per_day'])}. The owner's answer of 2026-11-12 refuses it, so it is not run "
            "through the income model.", ""]
    return out


def proposal_section(results: Path) -> list[str]:
    lat = load(results, "owner_latency.json")
    fx = load(results, "owner_fx.json")
    out = ["## A proposal for the owner: the curve's short end, not the base", "",
           "The operator's crypto take does not move with the base or the discounts: its best holds are 3 to 5 seconds, where "
           "the curve sets the fee. Holding the full fee longer does move it, and target flow still pays nothing more, since "
           "every curve below reaches 10% at 15 minutes. Each curve is a table on the engine's rules, at launch's base and "
           "discounts:", ""]
    rows = []
    for name, cv in lat["curves"].items():
        fx3 = sum(r["curves"][name]["3.0s"] for r in fx["markets"].values())
        fx0 = sum(r["curves"][name]["0.25s"] for r in fx["markets"].values())
        rows.append([name, pct(cv["short_fee_share"], 0), usd(cv["operator_per_day"]), usd(fx3 * OPERATOR_KEEPS),
                     usd(cv["operator_per_day"] + fx3 * OPERATOR_KEEPS), usd(cv["fast_per_day"] + fx0)])
    out += table(["Curve, 10% at 15 minutes", "Holds under 15 minutes pay", "Crypto latency, operator", "FX and indices, operator",
                  "Latency, operator, all", "Fastest client"], rows)
    out += ["A steeper short end charges every hold at least what D3's does, so the honest share is at least the one above. "
            "D7 lets the curve move only upwards, after notice. The fastest client's take does not move: its best holds are 15 "
            "to 30 minutes, past the curve.", ""]
    return out


def pairs_section(results: Path) -> list[str]:
    unp = load(results, "owner_unprice.json")
    out = ["## Unpriced pairs by fee", "",
           "#549's causal rule (a 5 s trigger of 40 bps, spells to 300 s) on $50M a side at the calibrated first tiers, as "
           "#623 runs it, at each fee the pair's winning leg can pay: tier 3's 8% at launch, the standard 10%, and tier 3's "
           "12% and tier 1's 15% at a 15% base. House loss a day.", ""]
    rows = []
    for f in unp["fees"]:
        p1, p2 = pair(unp, float(f), 0.01), pair(unp, float(f), 0.02)
        rows.append([pct(float(f), 0), f"{usd(p1['day'])} ({usd(p1['lo'])} to {usd(p1['hi'])})", usd(p1["stress"]),
                     f"{usd(p2['day'])} ({usd(p2['lo'])} to {usd(p2['hi'])})", usd(p2["stress"])])
    out += table(["Fee", "1% margin, year", "1%, four stress hours", "2% margin, year", "2%, four stress hours"], rows)
    return out


def write(results: Path) -> str:
    c = criterion(results)
    (results / "owner_criterion.json").write_text(json.dumps(c, indent=1, default=float))
    parts = ["# The corrected headline at the owner's values (#697)", "",
             "Generated by `python -I run.py owner_report` from the `owner_*` stages' results.", ""]
    parts += criterion_section(results, c)
    parts += alone_section(c)
    parts += income_section(results)
    parts += latency_section(results)
    parts += fx_section(results)
    parts += bases_section(results)
    parts += proposal_section(results)
    parts += pairs_section(results)
    text = "\n".join(parts).rstrip() + "\n"
    (results / "owner_values.md").write_text(text)
    return text
