"""The `groups` stage: fit every candidate group, find the floor share each needs, keep or drop it.

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
    return model.Group(name, d["members"], d["fit"]["beta"], d["fit"]["factor"], d["fit"]["rho"], w.get("factor", 0.0), w.get("rho", {}))


def pair_books(g: model.Group) -> list[tuple[str, str]]:
    return [(g.members[0], g.members[1])] + ([(g.members[0], g.members[2])] if len(g.members) > 2 else [])


def books(ctx: Ctx, g: model.Group):
    """(members, size, hedge) of the books tested: each pair hedged in full and each leg alone."""
    for a, b in pair_books(g):
        for size in test_sizes(ctx.specs[a]):
            for hedge in (1.0, 0.0):  # in full, and the leg alone
                yield [a, b], size, hedge


def evaluate(ctx: Ctx, g: model.Group, phis=PHI_GRID) -> list[dict]:
    """One row per (book, tier, phi): the rule's and the bands' loss past balance on the same books and starts."""
    rng = np.random.default_rng(SEED)
    rows = []
    windows = {}
    for members, size, hedge in books(ctx, g):
        key = tuple(members)
        w = windows.setdefault(key, sim.Window(ctx.hr, members))
        starts = np.random.default_rng(SEED).choice(w.starts, ACCOUNTS)
        n0 = np.tile(checks.pair_book(members, g.beta, members[0], members[1], size, hedge), (ACCOUNTS, 1))
        for tier in TIERS_USED:
            floor = TIER_FLOORS[tier]
            r_b = sim.simulate(w, ctx.specs, model.Params([g], 1.0), "bands", members, n0, starts, floor, ctx.level)
            for phi in phis:
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
    return all(r["port_rate"] <= ratio * r["bands_rate"] + RATE_SLACK and r["port_size"] <= ratio * r["bands_size"] + SIZE_SLACK for r in rows)


def floor_share(rows: list[dict], ratio: float = RATE_RATIO) -> float:
    """The smallest grid share at which every tested book passes at `ratio`, or 1.0."""
    for phi in PHI_GRID:
        if passes([r for r in rows if r["phi"] == phi], ratio):
            return phi
    return 1.0


def saving(rows: list[dict], phi: float) -> float:
    """The mean saving of the hedged books' initial requirement at tier 2 (no tier floor) at `phi`."""
    sel = [r["saving"] for r in rows if r["phi"] == phi and r["tier"] == 2 and r["hedge"] == 1.0]
    return float(np.mean(sel))


def run(ctx: Ctx, candidates: dict[str, list[str]]) -> dict:
    fits = fit_all(ctx, candidates)
    out = {"quantile": ctx.q, "horizon_s": ctx.h, "level": ctx.level, "horizon_scale": ctx.scale, "groups": {}}
    rows = {}
    for name, d in fits.items():
        g = to_group(name, d)
        rows[name] = evaluate(ctx, g)
        f = d["fit"]
        out["groups"][name] = {
            "members": g.members, "kept": True, "reason": "", "phi_min": float(floor_share(rows[name])),
            "phi_min_by_allowance": {str(r): float(floor_share(rows[name], r)) for r in (1.0, 1.5, 2.0, 3.0)},
            "beta": g.beta, "factor": g.factor, "rho": g.rho, "weekend": d["weekend"],
            "fit": {"hours": f["hours"], "beta_halves": f["beta_halves"], "r2": f["r2"],
                    "factor_move_hour": f["factor_hour"], "rho_move_hour": f["rho_hour"]},
        }
    # D5 sets one share: the largest the kept groups need, and a group whose saving at that share
    # is too small to be worth its parameters leaves, the one that forces the share up most first, which can
    # lower the share, until none is left to go.
    kept = set(out["groups"])
    while True:
        phi = max([out["groups"][n]["phi_min"] for n in kept], default=1.0)
        failing = [n for n in kept if out["groups"][n]["phi_min"] >= 1.0 or saving(rows[n], phi) <= MIN_SAVING]
        if not failing:
            break
        gone = {max(failing, key=lambda n: out["groups"][n]["phi_min"])}  # the one forcing the share up most
        for n in gone:
            sv = saving(rows[n], phi)
            out["groups"][n]["reason"] = (
                "dropped: no floor share below 1 keeps its books' loss past balance within the allowance" if out["groups"][n]["phi_min"] >= 1.0
                else f"dropped: at the proposal's floor share {phi} its offsets save {sv:.0%} of the per-position requirement, no more than {MIN_SAVING:.0%}")
        kept -= gone
    out["phi"] = float(max([out["groups"][n]["phi_min"] for n in kept], default=1.0))
    for n, g in out["groups"].items():
        g["kept"] = n in kept
        g["reason"] = g["reason"] or "kept"
        g["saving_at_phi"] = float(saving(rows[n], out["phi"]))
    out["rows"] = [{"group": n, **r} for n in out["groups"] for r in rows[n]]
    return out
