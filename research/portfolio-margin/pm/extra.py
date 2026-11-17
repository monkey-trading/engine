"""The checks that stand apart: crisis days, the alternatives, the bounds and the outages."""

import numpy as np

from pm import calibrate, checks, daily, horizon, model, sim
from pm.context import SEED, TIER_FLOORS, Ctx
from pm.groups import test_sizes


def crisis_days(ctx: Ctx, params: model.Params, top: int = 12) -> list[dict]:
    """Each kept group's worst days of the long history (by the factor, and by a pair's hedge residual):
    the day's move of the factor and of a loading-hedged pair per unit of its long leg, against the
    pair's requirement and its equity at the trigger, at the first band's size."""
    dates, syms, x = daily.load()
    rows = []
    for g in params.groups:
        cols = [syms.index(m) for m in g.members]
        d, r = daily.returns_dated(dates, x, cols)
        f = r.mean(axis=1)
        a, b = g.members[0], g.members[1]
        size = ctx.specs[a].bands[0][0]
        book = {a: np.array([size]), b: np.array([-g.beta[a] * size / g.beta[b]])}
        R = float(model.requirement(ctx.specs, params, book)[0])
        pp = float(model.per_position(ctx.specs, book)[0])
        pair = r[:, 0] - (g.beta[a] / g.beta[b]) * r[:, 1]
        order = {"factor": np.argsort(-np.abs(f))[:top], "hedge": np.argsort(-np.abs(pair))[:top]}
        for why, idx in order.items():
            for i in idx:
                loss = abs(pair[i]) * size
                rows.append({"group": g.name, "why": why, "date": d[i], "factor_move": float(f[i]), "pair_move_per_long_leg": float(pair[i]),
                             "pair_loss_over_trigger_equity": loss / (ctx.level * R), "pair_loss_over_R": loss / R, "pair_loss_over_pp": loss / pp})
        cal = {"group": g.name, "why": "history", "date": f"{d[0]}..{d[-1]}", "factor_move": float(np.quantile(np.abs(f), 0.995)),
               "pair_move_per_long_leg": float(np.quantile(np.abs(pair), 0.995))}
        rows.append(cal)
    return rows


def alternatives(ctx: Ctx, params: model.Params) -> list[dict]:
    """On hedged pairs: per-position margin, a spread credit on it (half of the smaller leg's), the factor
    grid, and a full historical scenario requirement (the horizon's empirical quantile loss with the
    closing cost, over the level), with each one's loss past balance rate at the horizon and the hour."""
    rows = []
    for g in params.groups:
        _, rr = ctx.hr.joint_hours(g.members)
        sym = np.vstack([rr, -rr])
        xs = {"h": sym * ctx.scale, "hour": sym}
        a, b = g.members[0], g.members[1]
        for size in test_sizes(ctx.specs[a])[:3]:
            n = checks.pair_book(g.members, g.beta, a, b, size, 1.0)
            book = {m: np.array([v]) for m, v in zip(g.members, n)}
            pp = float(model.per_position(ctx.specs, book)[0])
            legs = [float(model.per_position(ctx.specs, {m: book[m]})[0]) for m in (a, b)]
            cost = sum(float(model.spread_cost(ctx.specs[m], np.abs(book[m]))[0]) for m in (a, b))
            loss = -(xs["h"] @ n) + cost
            scenario = float(np.quantile(loss, ctx.q)) / ctx.level
            req = {"bands": pp, "spread_credit_50pct": pp - 0.5 * min(legs), "factor_grid": float(model.requirement(ctx.specs, params, book)[0]),
                   "scenario_grid": min(pp, scenario)}
            for name, R in req.items():
                row = {"group": g.name, "pair": f"{a}/{b}", "size": size, "requirement_name": name, "R": R, "R_over_pp": R / pp}
                for tag, x in xs.items():
                    rate, ex = checks.tail_excess(x, n, ctx.level * R, cost)
                    row[f"rate_{tag}"], row[f"excess_{tag}"] = rate, ex / np.abs(n).sum()
                rows.append(row)
    return rows


def bounds(ctx: Ctx, params: model.Params) -> list[dict]:
    """Candidate stricter bounds for D11: each kept group's shocks at stricter standards, and the multiple
    of its shocks at which a hedged pair at the first band's size needs its per-position requirement."""
    tab = horizon.table()
    rows = []
    for g in params.groups:
        _, r = ctx.hr.joint_hours(g.members)
        f, beta, e = calibrate.fit(r)
        a, b = g.members[0], g.members[1]
        size = ctx.specs[a].bands[0][0]
        book = {a: np.array([size]), b: np.array([-g.beta[a] * size / g.beta[b]])}
        pp = float(model.per_position(ctx.specs, book)[0])
        shock_part = float(model.model_term(ctx.specs, params, book)[0] - model.closing_cost(ctx.specs, book, ctx.level)[0])
        k = (pp - float(model.closing_cost(ctx.specs, book, ctx.level)[0])) / shock_part
        for q, h in ((ctx.q, ctx.h), (0.999, ctx.h), (ctx.q, 300), (0.999, 300), (ctx.q, 3600)):
            s = horizon.ratio(tab, h, q) / ctx.level
            row = {"group": g.name, "q": q, "horizon_s": h}
            for i, m in enumerate(g.members[:6]):
                row[f"rho_{m}"] = calibrate.up(np.quantile(np.abs(e[:, i]), q) * s / abs(beta[i]))
            row["multiple_to_per_position_pair"] = k
            rows.append(row)
    return rows


def outage(ctx: Ctx, params: model.Params, lengths=(4, 24, 72, 168), accounts: int = 600) -> list[dict]:
    """One member shut for `L` hours (an outage, a halt or a blocked corporate action) while the other trades:
    a loading-hedged pair held by one account and its opposite by another, opened at one of many starts.
    The shut leg counts at its last mark and cannot close; the other leg closes by its threshold, leaving
    the shut leg alone. Reported: the pair's house result per gross (positive is the house's gain, with the
    clients' losses capped at their balances) over all starts (the mean) and at the
    start an operator picking the timing would choose (the lowest), and the share of starts with a loss past balance."""
    rows = []
    rng = np.random.default_rng(SEED)
    for g in params.groups:
        a, b = g.members[0], g.members[1]
        members = [a, b]
        w = sim.Window(ctx.hr, members)
        starts = rng.choice(w.starts, accounts)
        size = ctx.specs[a].bands[0][0]
        n0 = np.tile(checks.pair_book(members, g.beta, a, b, size, 1.0), (accounts, 1))
        for rule in ("bands", "portfolio"):
            for tier, floor in ((0, 0.05), (2, 0.0)):
                for L in lengths:
                    res = [sim.simulate_shut(w, ctx.specs, params, rule, members, s * n0, starts, floor, L, ctx.level) for s in (1.0, -1.0)]
                    pair = res[0]["house"] + res[1]["house"]
                    gross = res[0]["gross"] + res[1]["gross"]
                    rows.append({"group": g.name, "rule": rule, "tier": tier, "outage_hours": L,
                                 "house_per_gross_mean": float((pair / gross).mean()), "house_per_gross_worst": float((pair / gross).min()),
                                 "past_balance_share": float(((res[0]["past_balance"] + res[1]["past_balance"]) > 1e-9).mean()),
                                 "past_balance_per_gross_mean": float(((res[0]["past_balance"] + res[1]["past_balance"]) / gross).mean())})
    return rows


def implied_horizon(ctx: Ctx) -> list[dict]:
    """For every market, the horizon over which the `q` quantile of its move equals the close-out level's
    share of its first margin band, i.e. what today's lone-position band stands for, taking a move over
    `s` seconds as the hour's times the square root of `s / 3600` (the grids' ratio is below it)."""
    rows = []
    for s, spec in ctx.specs.items():
        r = ctx.hr.hour_ret[:, ctx.hr.index[s]]
        q1h = float(np.quantile(np.abs(r[np.isfinite(r)]), ctx.q))
        move = ctx.level * spec.bands[0][1]
        rows.append({"market": s, "class": spec.asset_class, "first_band_bps": spec.bands[0][1] * 1e4, "move_at_trigger_bps": move * 1e4,
                     "hour_quantile_bps": q1h * 1e4, "implied_horizon_s": 3600 * min((move / q1h) ** 2, 10.0)})
    return rows
