"""The proposal in the production genesis's format, and the backtest tables built from the groups stage."""

import numpy as np

from pm.common import CANDIDATES


def toml(groups_json: dict) -> str:
    q, h = groups_json["quantile"], groups_json["horizon_s"]
    out = [
        "# Portfolio margin's launch values, proposed by research/portfolio-margin (#914) for the owner's",
        "# confirmation on #913. Residual shocks are in tenths of a basis point per unit of loading-weighted exposure, rounded up (there is no factor shock); loadings in",
        "# thousandths. A shock is the tail quantile of the move over the horizon divided by the close-out",
        f"# level ({groups_json['level']:.0%} at launch); a weekend shock is the gap's quantile, undivided.",
        "# A group's floor share is a share of its matched exposure's per-position requirement (#913's D5 as revised);",
        "# unmatched exposure keeps its own requirement. The section's name and keys are a proposal: #639's F1 fixes",
        "# the table the engine reads.",
        "[genesis.params.portfolio_margin]",
        f"quantile_bps = {round(q * 1e4)}",
        f"horizon_ms = {round(h * 1000)}",
        "",
    ]
    for name, g in groups_json["groups"].items():
        if not g["kept"]:
            out += [f"# {name}: {g['reason']}", ""]
            continue
        w = g.get("weekend", {})
        out += ["[[genesis.params.portfolio_margin.groups]]", f'name = "{name}"', f"floor_share_bps = {round(g['phi'] * 1e4)}"]
        out.append("members = [")
        for m in g["members"]:
            line = f'  {{ market = "{m}", loading_milli = {round(g["beta"][m] * 1000)}, residual_tenth_bps = {round(g["rho"][m] * 1e5)}'
            if w.get("rho"):
                line += f", weekend_residual_tenth_bps = {round(w['rho'][m] * 1e5)}"
            out.append(line + " },")
        out += ["]", ""]
    return "\n".join(out)


def backtest(rows: list[dict], groups_json: dict, shares=(0.0, 0.1, 0.25, 0.5, 1.0)) -> tuple[list[dict], list[dict]]:
    """(hedged against single, backtest by hedged share of flow) at each kept group's share, from the groups
    stage's books. A lone leg runs the same under both rules, so the single position is the bands' run."""
    hvs, bt = [], []
    for name, g in groups_json["groups"].items():
        if not g["kept"]:
            continue
        sel = [r for r in rows if r["group"] == name and r["phi"] == g["phi"] and r["hedge"] == 1.0]
        for r in sel:
            single = next(x for x in rows if x["group"] == name and x["pair"] == r["pair"] and x["size"] == r["size"] and x["tier"] == r["tier"] and x["hedge"] == 0.0)
            hvs.append({"group": name, "pair": r["pair"], "size": r["size"], "tier": r["tier"], "phi": g["phi"],
                        "hedged_rule_rate": r["port_rate"], "hedged_rule_size_bps": r["port_size"] * 1e4,
                        "single_bands_rate": single["bands_rate"], "single_bands_size_bps": single["bands_size"] * 1e4,
                        "hedged_bands_rate": r["bands_rate"], "hedged_bands_size_bps": r["bands_size"] * 1e4,
                        "released_margin_share": r["saving"]})
            for s in shares:
                row = {"group": name, "pair": r["pair"], "size": r["size"], "tier": r["tier"], "hedged_share_of_flow": s}
                for rule, tag in (("bands", "bands"), ("rule", "port")):
                    for k in ("closed", "rate", "size", "house", "leverage"):
                        row[f"{rule}_{k}"] = s * r[f"{tag}_{k}"] + (1 - s) * single[f"{tag}_{k}"]
                row["released_margin_share"] = s * r["saving"]
                bt.append(row)
    return hvs, bt
