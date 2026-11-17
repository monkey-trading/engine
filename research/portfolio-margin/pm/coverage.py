"""Coverage of the final requirement under joint sampling, and the lone and large positions.

A book is covered when the move over the horizon, with the fills' typical spreads, costs it no more
than its equity at the close-out trigger, `level * R` (at the market's bands, R as `model.requirement`
gives it, with the cap, the floor share, closing costs at ceilings and the weekend values over a
weekend gap, where the threshold is R in full). The moves are the hours every member was open, both
signs, each hour's return vector scaled by the horizon's ratio to the hour (`calibrate`): the joint
distribution as the data has it, not the members' marginal quantiles added. A book's rate is the
share of sampled moves that exceed its equity; it meets the standard if that is at most 1 - q.
"""

import numpy as np

from pm import calibrate, checks, model
from pm.context import Ctx, SEED
from pm.groups import test_sizes


def book_dict(members, n):
    return {m: np.array([x]) for m, x in zip(members, n)}


def typical_cost(specs, members, n) -> float:
    return float(sum(model.spread_cost(specs[m], np.array([abs(x)]))[0] for m, x in zip(members, n)))


def books_for(ctx: Ctx, g: model.Group, rng) -> list[tuple[str, list[str], np.ndarray]]:
    out = []
    mem = g.members[:3] if len(g.members) > 3 else g.members
    for size in test_sizes(ctx.specs[g.members[0]]):
        for m in g.members:
            n = np.zeros(len(g.members))
            n[g.members.index(m)] = size
            out.append(("lone", g.members, n))
        for hedge in (0.5, 1.0):
            for i, a in enumerate(mem):
                for b in mem[i + 1 :]:
                    out.append((f"pair_h{hedge}", g.members, checks.pair_book(g.members, g.beta, a, b, size, hedge)))
        out.append(("basket", g.members, checks.basket_book(g.members, g.beta, rng, size, 1.0)))
    return out


def measure(ctx, g, params, rule, members, n, x_by_horizon, weekend_x):
    book = book_dict(members, n)
    pp = float(model.per_position(ctx.specs, book)[0])
    R = pp if rule == "bands" else float(model.requirement(ctx.specs, params, book)[0])
    cost = typical_cost(ctx.specs, members, n)
    gross = float(np.abs(n).sum())
    row = {"rule": rule, "gross": gross, "pp": pp, "R": R, "R_over_pp": R / pp}
    for tag, x in x_by_horizon.items():
        rate, ex = checks.tail_excess(x, n, ctx.level * R, cost)
        row[f"rate_{tag}"], row[f"excess_{tag}"] = rate, ex / gross
    if weekend_x is not None and len(weekend_x):
        # An account passes the gap-start check at the weekend requirement in full (today: only for a market
        # with a weekend rate) and the close-out level's share before it: the least equity a survivor holds.
        Rw = sum(model.staircase(ctx.specs[m].bands, book[m], ctx.specs[m].weekend)[0] for m in members if ctx.specs[m].weekend > 0) if rule == "bands" \
            else float(model.requirement(ctx.specs, params, book, 0.0, weekend=True)[0])
        rate, ex = checks.tail_excess(weekend_x, n, max(ctx.level * R, Rw), cost)
        row["rate_weekend"], row["excess_weekend"] = rate, ex / gross
    return row


def run(ctx: Ctx, params: model.Params) -> list[dict]:
    rng = np.random.default_rng(SEED)
    rows = []
    for g in params.groups:
        _, r = ctx.hr.joint_hours(g.members)
        sym = np.vstack([r, -r])
        xs = {f"h{int(ctx.h)}s": sym * ctx.scale, "h3600s": sym}
        wk = calibrate.weekend_events(ctx.hr, g.members)
        wx = np.vstack([wk, -wk]) if len(wk) else None
        for kind, members, n in books_for(ctx, g, rng):
            for rule in ("bands", "portfolio"):
                rows.append({"group": g.name, "kind": kind, "books": "/".join(f"{m}:{x:.0f}" for m, x in zip(members, n) if x),
                             **measure(ctx, g, params, rule, members, n, xs, wx)})
    return rows


def summarise(rows: list[dict], q: float) -> list[dict]:
    out = []
    keys = sorted({(r["group"], r["kind"].split("_")[0] if r["kind"].startswith("pair") else r["kind"], r["rule"]) for r in rows})
    for g, kind, rule in keys:
        sel = [r for r in rows if r["group"] == g and r["rule"] == rule and (r["kind"] == kind or r["kind"].startswith(kind + "_"))]
        row = {"group": g, "kind": kind, "rule": rule, "books": len(sel), "mean_R_over_pp": float(np.mean([r["R_over_pp"] for r in sel]))}
        for k in sel[0]:
            if k.startswith("rate_"):
                v = np.array([r[k] for r in sel if k in r])
                row[f"mean_{k}"] = float(v.mean())
                row[f"share_meeting_{k[5:]}"] = float((v <= 1 - q).mean())
        out.append(row)
    return out


def lone_leverage(ctx: Ctx, params: model.Params, label: str = "proposal") -> list[dict]:
    """A lone position in each grouped market, across its bands, at each tier's floor: its leverage under
    the bands and under the rule, against the launch maximum."""
    rows = []
    sizes = np.geomspace(1e4, 2e8, 41)
    for g in params.groups:
        for m in g.members:
            spec = ctx.specs[m]
            launch_max = max(max(up_to / model.staircase(spec.bands, np.array([up_to]))[0] for up_to, _ in spec.bands if np.isfinite(up_to)), 1 / spec.bands[-1][1])
            for tier, floor in ((0, 0.05), (1, 0.02), (2, 0.0)):
                pp = model.per_position(ctx.specs, {m: sizes}, floor)
                R = model.requirement(ctx.specs, params, {m: sizes}, floor)
                lev_pp, lev_R = sizes / pp, sizes / R
                i = int(np.argmax(lev_R / lev_pp))
                rows.append({"params": label, "phi": params.phi, "group": g.name, "market": m, "tier": tier, "launch_max_leverage": launch_max,
                             "max_leverage_bands": float(lev_pp.max()), "max_leverage_rule": float(lev_R.max()),
                             "rule_over_launch": float(lev_R.max() / launch_max), "max_ratio_to_own_bands": float((lev_R / lev_pp).max()),
                             "at_size_usd": float(sizes[i]), "rule_leverage_there": float(lev_R[i]), "bands_leverage_there": float(lev_pp[i])})
    return rows


def ceiling_shortfall(ctx: Ctx) -> list[dict]:
    """Markets whose capped requirement, at a permitted close-out level, falls short of closing at the ceiling.
    The cap is the per-position requirement; a close-out at `level` leaves `level * R` to pay the close, so a
    market falls short where `level * rate < ceiling`, per unit of notional in a band."""
    rows = []
    for s, spec in ctx.specs.items():
        row = {"market": s, "class": spec.asset_class, "ceiling_bps": spec.ceiling * 1e4}
        lo = 0.0
        for i, (up_to, rate) in enumerate(spec.bands):
            row[f"band{i + 1}_bps"] = rate * 1e4
            row[f"band{i + 1}_up_to"] = up_to
        for level in (0.25, 0.5, 0.75):
            r = np.array([rate for _, rate in spec.bands])
            row[f"short_at_{level}"] = "|".join(str(i + 1) for i in np.flatnonzero(level * r < spec.ceiling))
        row["level_needed_first_band"] = spec.ceiling / spec.bands[0][1]
        rows.append(row)
    return rows
