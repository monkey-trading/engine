"""The proposal in the production genesis's format, and the backtest tables built from the groups stage."""

import numpy as np

from pm.common import CANDIDATES


def toml(groups_json: dict) -> str:
    q, h, phi = groups_json["quantile"], groups_json["horizon_s"], groups_json["phi"]
    out = [
        "# Portfolio margin's launch values, proposed by research/portfolio-margin (#914) for the owner's",
        "# confirmation on #913. Shocks are in tenths of a basis point of notional, rounded up; loadings in",
        "# thousandths. A shock is the tail quantile of the move over the horizon divided by the close-out",
        f"# level ({groups_json['level']:.0%} at launch); a weekend shock is the gap's quantile, undivided.",
        "# The section's name and keys are a proposal: #639's F1 fixes the table the engine reads.",
        "[genesis.params.portfolio_margin]",
        f"quantile_bps = {round(q * 1e4)}",
        f"horizon_ms = {round(h * 1000)}",
        f"floor_share_bps = {round(phi * 1e4)}",
        "",
    ]
    for name, g in groups_json["groups"].items():
        if not g["kept"]:
            out += [f"# {name}: {g['reason']}", ""]
            continue
        w = g.get("weekend", {})
        out += ["[[genesis.params.portfolio_margin.groups]]", f'name = "{name}"', f"factor_tenth_bps = {round(g['factor'] * 1e5)}"]
        if w.get("factor"):
            out.append(f"weekend_factor_tenth_bps = {round(w['factor'] * 1e5)}")
        out.append("members = [")
        for m in g["members"]:
            line = f'  {{ market = "{m}", loading_milli = {round(g["beta"][m] * 1000)}, residual_tenth_bps = {round(g["rho"][m] * 1e5)}'
            if w.get("rho"):
                line += f", weekend_residual_tenth_bps = {round(w['rho'][m] * 1e5)}"
            out.append(line + " },")
        out += ["]", ""]
    return "\n".join(out)


def backtest(rows: list[dict], phi: float, shares=(0.0, 0.1, 0.25, 0.5, 1.0)) -> tuple[list[dict], list[dict]]:
    """(hedged against single, backtest by hedged share of flow) at the proposed share, from the groups stage's books."""
    sel = [r for r in rows if r["phi"] == phi]
    hvs, bt = [], []
    for r in sel:
        if r["hedge"] != 1.0:
            continue
        single = next(x for x in sel if x["group"] == r["group"] and x["pair"].split("/")[0] == r["pair"].split("/")[0] and x["size"] == r["size"] and x["tier"] == r["tier"] and x["hedge"] == 0.0 and x["pair"] == r["pair"])
        hvs.append({"group": r["group"], "pair": r["pair"], "size": r["size"], "tier": r["tier"],
                    "hedged_rule_rate": r["port_rate"], "hedged_rule_size_bps": r["port_size"] * 1e4,
                    "single_bands_rate": single["bands_rate"], "single_bands_size_bps": single["bands_size"] * 1e4,
                    "hedged_bands_rate": r["bands_rate"], "hedged_bands_size_bps": r["bands_size"] * 1e4,
                    "rate_ratio_to_single": r["port_rate"] / single["bands_rate"] if single["bands_rate"] else float("nan"),
                    "released_margin_share": r["saving"]})
        for s in shares:
            row = {"group": r["group"], "pair": r["pair"], "size": r["size"], "tier": r["tier"], "hedged_share_of_flow": s}
            for rule, tag in (("bands", "bands"), ("rule", "port")):
                for k in ("closed", "rate", "size", "house", "leverage"):
                    row[f"{rule}_{k}"] = s * r[f"{tag}_{k}"] + (1 - s) * single[f"{tag}_{k}"]
            row["released_margin_share"] = s * r["saving"]
            bt.append(row)
    return hvs, bt
