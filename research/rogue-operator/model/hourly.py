"""Every attack measured on one sampled hour: the unit of work `run.py` spreads over its workers.

Each function returns flat rows, one per configuration, in per-unit terms (a dollar of the
operator's position, a dollar a side of its pairs) or in dollars of client flow at the flow scale
given, so `run.py` scales and totals them over the year.
"""

import numpy as np

from model import hold, latency, skim, stress, unprice
from model.flow import MarketFlow
from model.paths import Hour

DECIDE_S = 0.25
# Latency arbitrage: lags (s), extra spreads (bps), with and without laundering the toxicity score.
LAGS_S = (0.25, 0.5, 1, 2, 3, 5, 8, 15)
EXTRA_BPS = (0, 0.5, 1, 2, 3, 5, 10)
DECIDE_SENSITIVITY_S = (0.1, 1.0)
# Skimming: lags and delays (s); the monotone schedule only at the delays an order can take unseen.
SKIM_LAGS_S = (0.25, 1, 2, 3, 5, 8)
SKIM_DELAYS_S = (0, 1, 3, 10, 30)
SEQUENCED = ((1, 0), (2, 0), (3, 0), (3, 1))
# The held sync: holds (s) at a 3 s lag, and the 5 s hold at shorter lags; a start every 10 s.
HOLDS_S = (1, 2, 5, 10, 30)
HOLD_LAGS_S = (0.25, 1, 3)
HOLD_EVERY_S = 10
# Unpricing: spell lengths (s) and the causal rule's 5 s triggers.
SPELLS_S = (5, 15, 30, 60, 120, 300)
# Margin rates the pairs' legs post on BTC-USD and ETH-USD: genesis (100x), and 50x and 20x.
PAIR_MARGINS = (0.01, 0.02, 0.05)
TRIGGERS = (0.001, 0.002, 0.004)
MARGIN = {"BTC-USD": 0.01, "ETH-USD": 0.01, "SOL-USD": 0.10}
# Momentum: after the trailing minute moved at least `theta`, the move over the next `ahead`.
DRIFT_THETA_BPS = (25, 50, 100)
DRIFT_AHEAD_S = (5, 30, 60, 300)


def base(h: Hour) -> dict:
    return {"market": h.market, "hour": h.hour, "stratum": h.stratum}


def latency_rows(h: Hour) -> list[dict]:
    rows = []
    for lag in LAGS_S:
        for extra in EXTRA_BPS:
            for launder in (True, False):
                out = latency.arbitrage(h.x, h.step_s, h.lead, DECIDE_S, lag, extra, launder)
                rows.append({**base(h), "attack": "latency", "lag_s": lag, "extra_bps": extra, "launder": launder, "decide_s": DECIDE_S, "stress": True, **out})
    for decide in DECIDE_SENSITIVITY_S:
        out = latency.arbitrage(h.x, h.step_s, h.lead, decide, 3, 0, True)
        rows.append({**base(h), "attack": "latency", "lag_s": 3, "extra_bps": 0, "launder": True, "decide_s": decide, "stress": True, **out})
    out = latency.arbitrage(h.x, h.step_s, h.lead, DECIDE_S, 3, 0, True, stress=False)
    rows.append({**base(h), "attack": "latency", "lag_s": 3, "extra_bps": 0, "launder": True, "decide_s": DECIDE_S, "stress": False, **out})
    return rows


def skim_rows(h: Hour, flows: dict[float, MarketFlow]) -> list[dict]:
    rows = []
    span = h.analysis
    for scale, f in flows.items():
        rng = np.random.default_rng([h.hour, int(scale * 10), 549])
        at, side = skim.arrivals(rng, f.fills_per_second, span.start, span.stop - span.start, h.step_s)
        size = f.sample_sizes(rng, len(at))
        notional = float(size.sum())
        for lag in SKIM_LAGS_S:
            for delay in SKIM_DELAYS_S:
                lg, dl = h.steps(lag), h.steps(delay)
                for kind, fn in (("bound", skim.bound), ("causal", skim.causal)):
                    if scale != 1.0:
                        continue  # per fill the same at any flow; only the schedule depends on it
                    g = fn(h.x, at, side, lg, dl)
                    rows.append({**base(h), "attack": "skim", "kind": kind, "scale": scale, "lag_s": lag, "delay_s": delay,
                                 "fills": len(at), "notional": notional, "usd": float((size * g).sum()),
                                 "mean_bps": float(g.mean() * 1e4), "sd_bps": float(g.std() * 1e4)})
        for lag, delay in SEQUENCED:
            g = skim.schedule(h.x, at, side, size, h.steps(lag), h.steps(delay))
            rows.append({**base(h), "attack": "skim", "kind": "sequenced", "scale": scale, "lag_s": lag, "delay_s": delay,
                         "fills": len(at), "notional": notional, "usd": float((size * g).sum()),
                         "mean_bps": float(g.mean() * 1e4), "sd_bps": float(g.std() * 1e4)})
    return rows


def hold_rows(h: Hour) -> list[dict]:
    rows = []
    span = h.analysis
    starts = np.arange(span.start, span.stop, h.steps(HOLD_EVERY_S))
    configs = [(3, H) for H in HOLDS_S] + [(lag, 5) for lag in HOLD_LAGS_S if lag != 3]
    for lag, hold_s in configs:
        out = hold.option(h.x, starts, h.steps(lag), h.steps(hold_s))
        rows.append({**base(h), "attack": "hold", "lag_s": lag, "hold_s": hold_s, "uses": len(starts),
                     "held": float(out["held"].mean()), "without": float(out["without"].mean()),
                     "now": float(out["now"].mean()), "kill_share": float(out["kill"].mean())})
    return rows


def unprice_rows(h: Hour) -> list[dict]:
    rows = []
    span = h.analysis
    margins = (MARGIN[h.market],) if h.market == "SOL-USD" else PAIR_MARGINS
    # Opening a pair pays the stress minimum on both legs, at the level of that moment.
    every = h.steps(1.0)
    level_spread = stress.min_spread(h.x[::every], 1.0)
    open_cost = 2 * np.repeat(level_spread, every)[: len(h.x)]
    for m in margins:
        for spell in SPELLS_S:
            best, _ = unprice.best_spell(h.x, span.start, span.stop - span.start, h.steps(spell), m, open_cost)
            rows.append({**base(h), "attack": "unprice", "kind": "best", "margin": m, "spell_s": spell, "trigger": None,
                         "payoff": best, "spells": int(best > 0), "paying": int(best > 0), "unpriced_s": float(spell if best > 0 else 0)})
            for trig in TRIGGERS:
                out = unprice.causal_spells(h.x, span.start, span.stop - span.start, h.step_s, h.steps(spell), h.steps(3), m, trig, open_cost)
                rows.append({**base(h), "attack": "unprice", "kind": "causal", "margin": m, "spell_s": spell, "trigger": trig, **out})
    return rows


def drift_rows(h: Hour) -> list[dict]:
    """Momentum after a fast minute, on a 1 s grid: the count of such seconds and the sum of the
    next move in the direction of the minute's."""
    rows = []
    every = h.steps(1.0)
    x = h.x[::every]
    lo = h.lead // every
    hi = lo + 3600
    t = np.arange(lo, hi)
    past = x[t] - x[t - 60]
    for theta in DRIFT_THETA_BPS:
        fast = np.abs(np.expm1(past)) * 1e4 >= theta
        for ahead in DRIFT_AHEAD_S:
            ok = fast & (t + ahead < len(x))
            nxt = (x[t[ok] + ahead] - x[t[ok]]) * np.sign(past[ok])
            rows.append({**base(h), "attack": "drift", "theta_bps": theta, "ahead_s": ahead,
                         "events": int(ok.sum()), "sum_bps": float(nxt.sum() * 1e4), "sum_sq_bps": float((nxt**2).sum() * 1e8)})
    return rows


def divergence_rows(h: Hour) -> list[dict]:
    """How often the venues' last trades sit past the divergence triggers, on a 1 s grid: a check on
    leaving the divergence trigger out. Kraken's last trade lags its book, so this overstates."""
    every = h.steps(1.0)
    lo = h.lead // every
    c, k = h.x[::every][lo : lo + 3600], h.kraken[::every][lo : lo + 3600]
    gap = np.abs(np.expm1(c - k)) * 1e4
    return [{**base(h), "attack": "divergence", "level1_share": float((gap >= 16).mean()),
             "level2_share": float((gap >= 24).mean()), "p99_bps": float(np.percentile(gap, 99))}]


def all_rows(args) -> list[dict]:
    h, flows = args
    return latency_rows(h) + skim_rows(h, flows) + hold_rows(h) + unprice_rows(h) + drift_rows(h) + divergence_rows(h)
