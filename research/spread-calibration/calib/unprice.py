"""#549's unpriced-spell measure at the calibrated spreads.

#549 measured paired positions across a market's unpriced spells at a zero first tier
(`research/rogue-operator/model/unprice.py`). With a first tier `s1`, every fill of the pair pays
`s1` on top of the stress level's minimum: both legs on opening and on closing. This reuses that
module's measures with its spread function raised by `s1`, in the worker process that runs them.
The profit fee stays at the standard 10%: a pair that stands long enough before a spell pays the
standard fee under the proposed schedule, which is the case most favourable to the pair.
Margins are the leverage each volume tier allows (`calib/carrot.py`): 1% at full leverage, 2% at
tier 1's half, 5% at a fresh wallet's fifth; SOL-USD's 10% is its own first band.
"""

MARGINS = {"BTC-USD": (0.01, 0.02, 0.05), "ETH-USD": (0.01, 0.02, 0.05), "SOL-USD": (0.10,)}


def hour_rows(h, s1: float) -> list[dict]:
    import numpy as np

    from model import hourly, stress, unprice

    base_spread = unprice.spread_at
    unprice.spread_at = lambda move: base_spread(move) + s1
    try:
        span = h.analysis
        every = h.steps(1.0)
        level = stress.min_spread(h.x[::every], 1.0)
        open_cost = 2 * (np.repeat(level, every)[: len(h.x)] + s1)
        rows = []
        for m in MARGINS[h.market]:
            for spell in hourly.SPELLS_S:
                best, _ = unprice.best_spell(h.x, span.start, span.stop - span.start, h.steps(spell), m, open_cost)
                rows.append({**hourly.base(h), "attack": "unprice", "kind": "best", "margin": m, "spell_s": spell,
                             "trigger": None, "payoff": best, "spells": int(best > 0), "paying": int(best > 0),
                             "unpriced_s": float(spell if best > 0 else 0)})
                for trig in hourly.TRIGGERS:
                    out = unprice.causal_spells(h.x, span.start, span.stop - span.start, h.step_s, h.steps(spell),
                                                h.steps(3), m, trig, open_cost)
                    rows.append({**hourly.base(h), "attack": "unprice", "kind": "causal", "margin": m, "spell_s": spell,
                                 "trigger": trig, **out})
        return rows
    finally:
        unprice.spread_at = base_spread
