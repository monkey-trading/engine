"""The `groups` stage: fit every candidate group, find the floor share each needs, keep or drop it.

Under the revised D5 only matched exposure (`model.requirement`) gets the grid's credit, floored at the
group's own share of the matched part's per-position requirement; unmatched exposure keeps its
requirement exactly. A group's share is therefore calibrated on hedged books alone (a pair hedged in
full and in half), and a lone leg is run too, to confirm it is unchanged.

For each candidate group (D3) the loadings and shocks are fitted (`calibrate`). A group's *floor
share* is the smallest `phi` on a 0.1 grid at which its books (two pairs, each hedged in full and each leg alone), run on the account simulator
at two tiers and four sizes, lose past their balance no more often than 2 times as often as
the same books under per-position margin (plus 0.2 points) and no more in size than 2 times (plus
0.1 bp of gross), the allowance for what a fitted model leaves out: closures' gaps, a break in the
loadings, a jump inside an hour. A group is dropped when no `phi` below 1 meets that, or when its
offsets save on average less than 15% of the per-position requirement across those sizes at its
`phi`. The proposal's `phi` is the largest of the kept groups' (D5 sets one share).
"""

import numpy as np

from pm import calibrate, checks, model, sim
from pm.context import LEVEL, SEED, TIER_FLOORS, Ctx

PHI_GRID = [round(x, 2) for x in np.arange(0.0, 1.0001, 0.1)]
RATE_RATIO, RATE_SLACK = 2.0, 0.002  # the allowance
SIZE_RATIO, SIZE_SLACK = 2.0, 1e-5
MIN_SAVING = 0.15
ACCOUNTS = 2000
TIERS_USED = (0, 1, 2)


def test_sizes(spec) -> list[float]:
    b1 = spec.bands[0][0]
    return [b1 / 4, b1, 4 * b1, min(20 * b1, 1e8)]


def fit_all(ctx: Ctx, candidates: dict[str, list[str]]) -> dict:
    out = {}
    for name, members in candidates.items():
        f = calibrate.hourly_fit(ctx.hr, members, ctx.q, ctx.scale, ctx.level)
        w = calibrate.weekend_fit(ctx.hr, members, f["beta"], ctx.q)
        out[name] = {"members": members, "fit": f, "weekend": w}
    return out


def to_group(name: str, d: dict) -> model.Group:
    w = d["weekend"]
    return model.Group(name, d["members"], d["fit"]["beta"], 0.0, d["fit"]["rho"], 1.0, 0.0, w.get("rho", {}))


def pair_books(g: model.Group) -> list[tuple[str, str]]:
    return [(g.members[0], g.members[1])] + ([(g.members[0], g.members[2])] if len(g.members) > 2 else [])


def books(ctx: Ctx, g: model.Group):
    """(members, size, hedge) of the books tested: each pair hedged in full and each leg alone."""
    for a, b in pair_books(g):
        for size in test_sizes(ctx.specs[a]):
            for hedge in (1.0, 0.5, 0.0):  # in full, in half, and the leg alone (unchanged by the rule)
                yield [a, b], size, hedge


def evaluate(ctx: Ctx, g: model.Group, phis=PHI_GRID) -> list[dict]:
    """One row per (book, tier, phi): the rule's and the bands' loss past balance on the same books and starts."""
    rows = []
    windows = {}
    for members, size, hedge in books(ctx, g):
        key = tuple(members)
        if key not in windows:
            windows[key] = sim.Window(ctx.hr, members)
        w = windows[key]
        starts = np.random.default_rng(SEED).choice(w.starts, ACCOUNTS)
        # each book long and short, so a market's drift over the window favours neither side
        starts = np.concatenate([starts, starts])
        book = checks.pair_book(members, g.beta, members[0], members[1], size, hedge)
        n0 = np.vstack([np.tile(book, (ACCOUNTS, 1)), np.tile(-book, (ACCOUNTS, 1))])
        for tier in TIERS_USED:
            floor = TIER_FLOORS[tier]
            r_b = sim.simulate(w, ctx.specs, model.Params([g], 1.0), "bands", members, n0, starts, floor, ctx.level)
            for phi in (phis if hedge > 0 else (0.0,)):  # a lone leg is run at no floor, the rule's loosest
                r_p = sim.simulate(w, ctx.specs, model.Params([g], phi), "portfolio", members, n0, starts, floor, ctx.level)
                rows.append({"pair": "/".join(members), "size": size, "hedge": hedge, "tier": tier, "phi": phi, **summary(r_b, "bands"),
                             **summary(r_p, "port"), "saving": float(1 - (r_p["e0"] / r_b["e0"]).mean())})
    return rows


def summary(r: dict, tag: str) -> dict:
    gross = r["gross"].sum()
    return {f"{tag}_closed": float(r["closed_out"].mean()), f"{tag}_rate": float((r["past_balance"] > 1e-9).mean()),
            f"{tag}_size": float(r["past_balance"].sum() / gross), f"{tag}_house": float(r["house"].sum() / gross),
            f"{tag}_leverage": float((r["gross"] / r["e0"]).mean())}


def passes(rows: list[dict], ratio: float = RATE_RATIO) -> bool:
    rows = [r for r in rows if r["hedge"] > 0]
    return all(r["port_rate"] <= ratio * r["bands_rate"] + RATE_SLACK and r["port_size"] <= ratio * r["bands_size"] + SIZE_SLACK for r in rows)


def floor_share(rows: list[dict], ratio: float = RATE_RATIO) -> float:
    """The smallest grid share at which every tested book passes at `ratio`, or 1.0."""
    for phi in PHI_GRID:
        if passes([r for r in rows if r["phi"] == phi and r["hedge"] > 0], ratio):
            return phi
    return 1.0


def saving(rows: list[dict], phi: float) -> float:
    """The mean saving of the hedged books' initial requirement at tier 2 (no tier floor) at `phi`."""
    sel = [r["saving"] for r in rows if r["phi"] == phi and r["tier"] == 2 and r["hedge"] == 1.0]
    return float(np.mean(sel))


def run(ctx: Ctx, candidates: dict[str, list[str]]) -> dict:
    fits = fit_all(ctx, candidates)
    out = {"quantile": ctx.q, "horizon_s": ctx.h, "level": ctx.level, "horizon_scale": ctx.scale, "groups": {}}
    rows, table = {}, []
    for name, d in fits.items():
        g = to_group(name, d)
        rows[name] = evaluate(ctx, g)
        f = d["fit"]
        phi = float(floor_share(rows[name]))
        sv = saving(rows[name], phi)
        by = {str(r): float(floor_share(rows[name], r)) for r in (1.0, 1.5, 2.0, 3.0)}
        kept = phi < 1.0 and sv > MIN_SAVING
        out["groups"][name] = {
            "members": g.members, "kept": kept, "phi": phi, "saving_at_phi": float(sv), "phi_by_allowance": by,
            "reason": "kept" if kept else ("dropped: no floor share below 1 keeps its hedged books' loss past balance within the allowance" if phi >= 1.0
                                          else f"dropped: at its floor share {phi} its offsets save {sv:.0%} of the matched requirement, no more than {MIN_SAVING:.0%}"),
            "beta": g.beta, "rho": g.rho, "weekend": d["weekend"],
            "fit": {"hours": f["hours"], "beta_halves": f["beta_halves"], "r2": f["r2"],
                    "factor_move_hour": f["factor_hour"], "rho_move_hour": f["rho_hour"]},
        }
        for allowance, p in by.items():
            for r in rows[name]:
                if r["phi"] == p and r["hedge"] == 1.0:
                    table.append({"group": name, "allowance": float(allowance), "phi": p, "pair": r["pair"], "size": r["size"], "tier": r["tier"], "saving": r["saving"]})
        lone = [r for r in rows[name] if r["hedge"] == 0.0]
        out["groups"][name]["lone_unchanged"] = {"max_abs_rate_diff": max(abs(r["port_rate"] - r["bands_rate"]) for r in lone),
                                                 "max_abs_leverage_diff": max(abs(r["port_leverage"] - r["bands_leverage"]) for r in lone)}
    out["rows"] = [{"group": n, **r} for n in out["groups"] for r in rows[n]]
    out["saving_table"] = table
    return out
