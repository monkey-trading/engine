"""#549's paired positions across unpriced moves, at the launch rules, through tier-2 accounts.

#549 measured an operator that unprices BTC-USD or ETH-USD as a fast move starts, holding a long in
one account and a short in another, so the losing leg's close-out runs only after the move, past its
balance (`research/rogue-operator/model/unprice.py`). The trigger rule is #549's: unprice once the
venues have moved `trigger` in 5 s, counting the move from the engine's last index `lag` (3 s)
earlier, and price again once the move pays `margin / (1 - fee) + 0.3%` or at the spell's end. As
#734 found, the engine keeps a source's quote only if it is no older than the one it holds
(`marks.rs`, `store_quotes`), so once the market has priced again no later index shows an older
price: a spell's move counts from no earlier than the last repricing.

At the launch rules (`hour_rows`):
- **Accounts.** At D4's 1% margin each account must be tier 2 or above (D5). Each leg stays within
  its tier's best-price size, so every fill pays the market's first band `s1`.
- **The fee.** The winning leg pays the fee past the hold curve, the base less the tier's discount
  (9% at tier 2, 8% at tier 3), as if it had stood 15 minutes or more before the spell (#697's
  choice, the least it can pay).
- **The opening** (`OPENS`). As #549 runs the rule, each spell opens a new pair just before it, at the
  stress level the engine is at then (`trigger`). A pair that stood 15 minutes would have opened in
  a calm market at level 0 (`standing`), but the rule then needs a standing pair for every spell, as
  many as the spells in 15 minutes; that variant is an upper bound.
- **The close** (`CLOSES`). From V52 a stress level multiplies the size spread, and the fill also
  pays the level's minimum (`execution/mod.rs`). `engine` takes the level the engine sets at the
  repricing (`stress.rs`): the move window keeps only mids from the trailing 60 s, so once the market
  has been unpriced longer than that the repricing reads no move, and a raised level steps down
  only once its 300 s cooldown has passed since it last changed; `close_level` reads the move as
  the move from the last index, the least the window can show. `move` takes the level the whole
  move reads as, #697's and #734's convention, which the engine reaches only for spells inside the
  window. Both legs close at that level, the loser's close-out included (D2).
- **Each leg** loses at most its balance, the requirement plus its opening fill's spread, which its
  equity had to cover (`check_risk`): `leg = max(pnl - s_open - s_close - fee, -(m + s_open))`, the
  fee on the realised PnL at the spread-adjusted fills (`gaps.leg_results`).

Every other choice takes the least the pair can pay under the trigger rule. `check_697` reruns
#697's measure (the move's level for every close, the band unmultiplied, every move from 3 s before
its spell, the opening at the trigger's level, tier 3's fee), and `check_734` #734's (the band
multiplied, the move from the last repricing, #549's payoff form), to tie the three together.
"""

import numpy as np

from caps.gaps import leg_results

FEE_TIERS = (2, 3)  # 1% margin needs tier 2 or above (D5)
OPENS = ("trigger", "standing")
CLOSES = ("engine", "move")
MARGIN = 0.01  # D4: 1% on BTC-USD and ETH-USD
MARKETS = ("BTC-USD", "ETH-USD")
SPELL_S, TRIGGER = 300, 0.004  # the rule #549 and #697 report: unpriced up to 300 s once the venues move 40 bps in 5 s


def stress_terms(params: dict) -> dict:
    """The genesis's move triggers (bps), each level's minimum spread and size multiplier, the move
    window and the cooldown (s)."""
    levels = params["stress_levels"]
    return {"move_bps": tuple(lvl["trigger"]["move_bps"] for lvl in levels[1:]),
            "min_spread": tuple(lvl["min_spread_bps"] / 1e4 for lvl in levels),
            "multiplier": tuple(lvl["size_spread_multiplier_bps"] / 1e4 for lvl in levels),
            "window_s": params["stress_move_window_ms"] / 1000, "cooldown_s": params["stress_cooldown_ms"] / 1000}


def level_spread(level, s1: float, terms: dict):
    """One fill's spread at stress `level` for a slice in the first band `s1`."""
    level = np.asarray(level)
    return np.asarray(terms["min_spread"])[level] + s1 * np.asarray(terms["multiplier"])[level]


def target_level(reading_bps, terms: dict):
    """The level a move reading triggers."""
    reading = np.asarray(reading_bps, dtype=float)
    level = np.zeros(reading.shape, dtype=np.int64)
    for k, threshold in enumerate(terms["move_bps"], start=1):
        level[reading >= threshold] = k
    return level


def levels_since(x_s: np.ndarray, terms: dict) -> tuple[np.ndarray, np.ndarray]:
    """The automatic level at each second of a one-second path (#549's `stress.levels`, the genesis's
    triggers and cooldown), and the seconds since it last changed."""
    from model import stress

    level = stress.levels(x_s, 1.0, move_bps=terms["move_bps"], cooldown_s=terms["cooldown_s"])
    since = np.zeros(len(level))
    for i in range(1, len(level)):
        since[i] = 0.0 if level[i] != level[i - 1] else since[i - 1] + 1.0
    return level, since


def close_level(move: float, unpriced_s: float, level0: int, since0_s: float, terms: dict) -> int:
    """The level at the repricing after `unpriced_s` without a price, from `level0`, held for
    `since0_s` before the spell: the move reads only while the last mids are inside the window, a
    rise is at once, and a fall waits for the cooldown."""
    reading = move * 1e4 if unpriced_s <= terms["window_s"] else 0.0
    target = int(target_level(reading, terms))
    if target >= level0 or since0_s + unpriced_s >= terms["cooldown_s"]:
        return target
    return level0


def causal_spells(x, start: int, length: int, step_s: float, spell: int, lag: int, w: int, trigger: float, goal: float, fee: float, s1: float,
                  terms: dict, level: np.ndarray, since: np.ndarray, opens: str = "trigger", closes: str = "engine") -> dict:
    """#549's causal rule (`model/unprice.py`, `causal_spells`) on one hour at the launch rules: the
    market is unpriced once the move over the last `w` steps reaches `trigger`, and priced again once
    the move from the last index, `lag` steps back but no earlier than the last repricing, reaches
    `goal` or after `spell` steps; every spell the rule starts, one after another, each with a new
    pair opened as `opens` says and closed at the level `closes` says. Per unit of notional a side;
    `level` and `since` are per second (`levels_since`)."""
    every = int(round(1.0 / step_s))
    end = start + length
    fired = np.flatnonzero(np.abs(np.expm1(x[start:end] - x[start - w : end - w])) >= trigger) + start
    payoffs, seconds, levels = [], 0.0, []
    k, priced_at = 0, None
    while k < len(fired):
        i = int(fired[k])
        last = i - lag if priced_at is None else max(i - lag, priced_at)
        path = np.abs(np.expm1(x[i : i + spell + 1] - x[last]))
        hit = np.flatnonzero(path >= goal)
        stop = int(hit[0]) if len(hit) else len(path) - 1
        unpriced = (i + stop - last) * step_s
        sec = min(last // every, len(level) - 1)
        if closes == "engine":
            closing = close_level(float(path[stop]), unpriced, int(level[sec]), float(since[sec]), terms)
        else:
            closing = int(target_level(float(path[stop]) * 1e4, terms))
        opening = int(level[sec]) if opens == "trigger" else 0
        s_open, s_close = float(level_spread(opening, s1, terms)), float(level_spread(closing, s1, terms))
        winner, _ = leg_results(path[stop], s_open, s_close, MARGIN, fee, 0.0)
        loser, _ = leg_results(-path[stop], s_open, s_close, MARGIN, fee, 0.0)
        payoffs.append(float(winner + loser))
        levels.append(closing)
        seconds += unpriced
        priced_at = i + stop
        k = int(np.searchsorted(fired, i + stop + 1))
    return {"payoff": float(np.sum(payoffs)), "spells": len(payoffs), "unpriced_s": seconds,
            "paying": int(np.sum(np.array(payoffs) > 0)) if payoffs else 0,
            "closing_levels": [int(np.sum(np.array(levels) == lv)) for lv in range(len(terms["min_spread"]))]}


def hour_rows(h, s1: float, fees: dict, terms: dict) -> list[dict]:
    """The causal rule's rows for one sampled hour, per fee tier in `fees` ({tier: fee}), spell and
    trigger."""
    from model import hourly
    from model.unprice import TARGET_OVER_BREAKEVEN, TRIGGER_WINDOW_S

    span = h.analysis
    w = h.steps(TRIGGER_WINDOW_S)
    every = h.steps(1.0)
    level, since = levels_since(h.x[::every], terms)
    rows = []
    for tier, fee in fees.items():
        for opens in OPENS:
            for closes in CLOSES:
                for spell in hourly.SPELLS_S:
                    for trig in hourly.TRIGGERS:
                        out = causal_spells(h.x, span.start, span.stop - span.start, h.step_s, h.steps(spell), h.steps(3), w, trig,
                                            MARGIN / (1 - fee) + TARGET_OVER_BREAKEVEN, fee, s1, terms, level, since, opens, closes)
                        rows.append({**hourly.base(h), "tier": tier, "fee": fee, "opens": opens, "closes": closes, "spell_s": spell, "trigger": trig, **out})
    return rows


def check_697(h, s1: float) -> float:
    """#697's measure (`research/spread-calibration/calib/unprice.py`) at 1% margin, tier 3's 8% fee,
    300 s and 40 bps: per unit of notional a side."""
    from model import stress, unprice

    base_spread, base_fee = unprice.spread_at, unprice.PROFIT_FEE
    unprice.spread_at = lambda move: base_spread(move) + s1
    unprice.PROFIT_FEE = 0.08
    try:
        span = h.analysis
        every = h.steps(1.0)
        open_cost = 2 * (np.repeat(stress.min_spread(h.x[::every], 1.0), every)[: len(h.x)] + s1)
        out = unprice.causal_spells(h.x, span.start, span.stop - span.start, h.step_s, h.steps(SPELL_S), h.steps(3), MARGIN, TRIGGER, open_cost)
        return out["payoff"]
    finally:
        unprice.spread_at, unprice.PROFIT_FEE = base_spread, base_fee


def check_734(h, s1: float, terms: dict) -> dict:
    """#734's measure without its loss trigger (`research/loss-rate-trigger/trigger/pairs.py`) on one
    hour: #549's rule at 300 s and 40 bps, tier 3's 8% fee, each fill at `level_spread`, closes at the
    level the move reads, each move from no earlier than the last repricing, and #549's payoff form
    `(1 - f)(J - s_close) - min(J + s_close, m) - 2 s_open`. Per unit of notional a side, for pairs
    reopened before each spell at the level at the trigger's second (`trigger`; `causal_spells` takes
    the engine's lagged index's) and opened ahead at level 0 (`standing`)."""
    from model.unprice import TARGET_OVER_BREAKEVEN, TRIGGER_WINDOW_S

    fee = 0.08
    span, every, lag, w, spell = h.analysis, h.steps(1.0), h.steps(3), h.steps(TRIGGER_WINDOW_S), h.steps(SPELL_S)
    level, _ = levels_since(h.x[::every], terms)
    goal = MARGIN / (1 - fee) + TARGET_OVER_BREAKEVEN
    x, start, end = h.x, span.start, span.stop
    fired = np.flatnonzero(np.abs(np.expm1(x[start:end] - x[start - w : end - w])) >= TRIGGER) + start
    out = {}
    for opens in OPENS:
        total, k, priced_at = 0.0, 0, None
        while k < len(fired):
            i = int(fired[k])
            last = i - lag if priced_at is None else max(i - lag, priced_at)
            path = np.abs(np.expm1(x[i : i + spell + 1] - x[last]))
            hit = np.flatnonzero(path >= goal)
            stop = int(hit[0]) if len(hit) else len(path) - 1
            move = float(path[stop])
            close = float(level_spread(int(target_level(move * 1e4, terms)), s1, terms))
            opening = float(level_spread(int(level[min(i // every, len(level) - 1)]) if opens == "trigger" else 0, s1, terms))
            total += (1 - fee) * (move - close) - min(move + close, MARGIN) - 2 * opening
            priced_at = i + stop
            k = int(np.searchsorted(fired, i + stop + 1))
        out[opens] = total
    return out


def tables(rows: list[dict], pair_notional: float) -> list[dict]:
    """Per fee tier and rule: the year a day on BTC-USD and ETH-USD with its interval, from #549's
    stratified estimates over the sampled hours, and the sum over 2025-10-10's four stress hours,
    each as the house's loss at `pair_notional` a side, with the closing levels' counts there."""
    from model.report import Year, select

    y = Year()
    sampled = [r for r in rows if r["stratum"] != "stress"]
    out = []
    for tier, opens, closes in sorted({(r["tier"], r["opens"], r["closes"]) for r in rows}):
        for spell, trig in sorted({(r["spell_s"], r["trigger"]) for r in rows}):
            sel = select(sampled, tier=tier, opens=opens, closes=closes, spell_s=spell, trigger=trig)
            est = y.total(sel, lambda r, m: r["payoff"] * pair_notional, MARKETS)
            stress = [r for r in rows if r["stratum"] == "stress" and r["tier"] == tier and r["opens"] == opens and r["closes"] == closes
                      and r["spell_s"] == spell and r["trigger"] == trig]
            out.append({"tier": tier, "fee": sel[0]["fee"], "opens": opens, "closes": closes, "spell_s": spell, "trigger_bps_5s": trig * 1e4, "per_day": est.year / 365,
                        "per_day_lo": est.year_lo / 365, "per_day_hi": est.year_hi / 365,
                        "stress_hours": sum(r["payoff"] for r in stress) * pair_notional,
                        "stress_spells": sum(r["spells"] for r in stress),
                        "stress_closing_levels": [sum(r["closing_levels"][lv] for r in stress) for lv in range(len(stress[0]["closing_levels"]))]})
    return out
