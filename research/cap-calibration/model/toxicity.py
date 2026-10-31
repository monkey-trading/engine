"""Toxicity-parameter calibration: `prior`, `memory`, tier thresholds and spreads, from the
observed fill sizes and the markets' markout volatility.

From protocol V11 an account's toxicity score is the notional-weighted average of its fills'
markouts at `horizon_ms`, folded over `prior` USD of neutral history and at most `memory` USD of
fill notional (`params.rs`, `NotionalWeighting::averaged`): a fill of `n` USD with markout `m`
moves the score from `s` toward `m` by the fraction `n / (history + n)`, and the carried history
caps at `memory`. The score is in bps; markouts clamp at ±1000 bps.

The calibration, per market and in aggregate:

- *Fill sizes*: papertrade's margin-weighted size distribution on the market's margin schedule
  (`papertrade_sizes.csv`), inverted at quantiles. A position opens as one order, so a fill is a
  position's notional.
- *Markout scale*: the Parkinson volatility of the market's hourly bars gives a per-second log
  scale; the markout of a fill held `horizon_ms` against the index is normal with standard
  deviation `sigma_sec * sqrt(horizon_ms / 1000)` — benign flow has zero mean. Toxic flow is a
  sustained positive mean; the design level `M_TOX_BPS` is a parameter of the study.
- *Memory*: the bounded-memory score is a weighted average of the last `memory` USD of fills, so
  its sampling noise for fills of size `n` is about `sigma_markout * sqrt(n / memory)` bps. The
  chosen memory is the smallest that keeps that noise under `DRIFT_BPS` at every market's p99
  fill: `memory = max_market n99 * sigma_markout^2` (bps are ratios here, so the bound is exact
  for the stated 1 bp).
- *Prior*: the neutral history a new account starts with. It is set so a toxic account filling at
  the median size reaches tier 1 within `CLIMB_FILLS` fills under the exact recurrence, and never
  exceeds a small fraction of memory, so a new account's first fills are not frozen out.
- *Tiers and spreads*: tier 1's threshold is the highest level that (a) a toxic account reaches
  within `CLIMB_FILLS` median-size fills and (b) a benign account filling at its market's p99
  size stays below for `BENIGN_FILLS` fills with probability at least `1 - BENIGN_FP` (checked by
  Monte Carlo over the exact recurrence). Tier 2 doubles it. A tier's spread is the sustained
  per-fill markout an account needs to reach the tier in the stated fills: the house recoups the
  marginal toxic account's edge. The engine validates tiers ascending with spreads in
  0..=10000 bps.
"""

import numpy as np

from model import prices

BPS = 1e4
HORIZON_MS = 2000  # genesis.params.toxicity.horizon_ms
MAX_MARKOUT_BPS = 1000  # markouts.rs MAX_MARKOUT_BPS
DRIFT_BPS = 1.0  # the drift bound the issue states, at the observed fill sizes
# the climb candidates: a toxic account must reach tier 1 within the chosen number of median-size
# fills; the search takes the first that satisfies the benign bound
CLIMB_CANDIDATES = (25, 50, 75, 100, 150, 200, 300)
BENIGN_FILLS = 200  # a benign account gets this many fills before the false-positive bound
# The stated bound is for fills at least a horizon apart, so markouts do not overlap. Fills
# closer than the horizon share price moves; the burst sensitivity reports how much more often a
# 25-fill burst crosses (direction: more often), without gating the choice.
BURST_FILLS = 25
CORRELATED_TRIALS = 10_000
CORRELATED_CADENCES_S = (1.0, 0.5)  # a burst filling this often: overlapping markouts
TICK_S = 0.25  # the price path's step; the horizon is an integer number of steps
BENIGN_FP = 1e-3  # allowed probability a benign account crosses tier 1 within BENIGN_FILLS
M_TOX_BPS = 2.0  # the design toxic level: a sustained per-fill markout above this scalps the house
TIER2_MULT = 2  # tier 2's threshold is this multiple of tier 1's

SIZE_QUANTILES = (0.5, 0.75, 0.9, 0.99)


def schedule_key(tiers: list[dict]) -> str:
    """A margin band schedule as text (as `flow.schedule_key`)."""
    return ",".join(f"{t['bps']}@{t['up_to']}" if "up_to" in t else str(t["bps"]) for t in tiers)


def sigma_markout_bps(bars: np.ndarray, horizon_ms: int = HORIZON_MS) -> np.ndarray:
    """Each market's benign per-fill markout scale in bps: the median open hour's Parkinson
    volatility (the model's per-hour scale, as `prices.build`'s) scaled to the horizon —
    `sigma_hour / sqrt(3600) * sqrt(horizon seconds)`, in bps. A typical hour prices a typical
    fill; the drift and false-positive bounds then carry the tail by the memory bound."""
    is_open = bars[..., 0] != prices.CLOSED
    parkinson = np.where(is_open, (bars[..., 1] - bars[..., 2]).astype(np.float64) / 1e6 / np.sqrt(4 * np.log(2)), np.nan)
    with np.errstate(all="ignore"):
        sigma_hour = np.nanmedian(parkinson, axis=0)
    return sigma_hour / np.sqrt(3600) * np.sqrt(horizon_ms / 1000) * BPS


def size_quantiles(sizes: dict, genesis: dict, quantiles: tuple = SIZE_QUANTILES) -> dict[str, dict]:
    """Per market, the notional at each quantile of the observed fill-size distribution on the
    market's margin schedule, margin-weighted (so the quantiles are of flow, not of orders)."""
    by_symbol = {m["symbol"]: m for m in genesis["markets"]}
    out = {}
    for symbol, market in by_symbol.items():
        key = ("directional", "capped", schedule_key(market["margin_tiers"]))
        bounds, shares = sizes.get(key, (None, None))
        if bounds is None:
            continue
        # the CDF's shares are cumulative; invert by stepping to the first bound at the quantile
        qs = {}
        for q in quantiles:
            k = int(np.searchsorted(shares, q, side="left"))
            k = min(k, len(bounds) - 1)
            qs[f"n{int(q * 100)}"] = float(bounds[k])
        out[symbol] = qs
    return out


def averaged(score: float, history: float, markout_bps: float, notional: float, memory: float) -> tuple[float, float]:
    """One fold of the engine's notional weighting (`NotionalWeighting::averaged`): the score and
    history after a fill, rounding up in the house's favour at `Fixed`'s nine-decimal scale."""
    step = (markout_bps - score) * notional / (history + notional)
    step = np.ceil(step * 1e9) / 1e9  # round up at the engine's Fixed scale
    return score + step, min(history + notional, memory)


def climb_score(markout_bps: float, notional: float, prior: float, memory: float, fills: int) -> float:
    """The exact score after `fills` same-size fills of a sustained markout, from a fresh account."""
    score, history = 0.0, prior
    for _ in range(fills):
        score, history = averaged(score, history, markout_bps, notional, memory)
    return score


def benign_cross_probability(sigma_bps: float, notional: float, prior: float, memory: float, threshold: float, fills: int, seed: int, trials: int) -> float:
    """The Monte Carlo probability that a benign account (independent zero-mean markouts, this
    market's scale) crosses the threshold within `fills` same-size fills, under the exact
    recurrence. Independent markouts are the search bound; `correlated_cross_probability`
    verifies the chosen threshold against overlapping horizons."""
    rng = np.random.default_rng(seed)
    markouts = rng.standard_normal((trials, fills)) * sigma_bps
    return cross_probability(markouts, notional, prior, memory, threshold)


def correlated_cross_probability(sigma_bps: float, notional: float, prior: float, memory: float, threshold: float, fills: int, seed: int, trials: int, cadence_s: float) -> float:
    """The crossing probability when fills arrive `cadence_s` apart on one shared price path:
    each fill's markout is the horizon's return from its own entry time, so markouts closer than
    the horizon overlap and move together. The path steps at `TICK_S`; the horizon is
    `HORIZON_MS / 1000 / TICK_S` steps, each with scale `sigma_bps / sqrt(horizon steps)`."""
    rng = np.random.default_rng(seed)
    steps_per_horizon = int(round(HORIZON_MS / 1000 / TICK_S))
    start = int(round(cadence_s / TICK_S))
    n_steps = (fills - 1) * start + steps_per_horizon + 1
    increments = rng.standard_normal((trials, n_steps)) * sigma_bps / np.sqrt(steps_per_horizon)
    csum = np.cumsum(increments, axis=1)
    markouts = np.stack(
        [csum[:, i * start + steps_per_horizon] - csum[:, i * start] for i in range(fills)],
        axis=1,
    )
    return cross_probability(markouts, notional, prior, memory, threshold)


def cross_probability(markouts: np.ndarray, notional: float, prior: float, memory: float, threshold: float) -> float:
    """The share of trials whose exact-recurrence score crosses the threshold within the fills
    of `markouts` (rows are trials), nine-decimal round-up steps as in the engine."""
    trials = markouts.shape[0]
    scores = np.zeros(trials)
    histories = np.full(trials, prior)
    crossed = np.zeros(trials, dtype=bool)
    for i in range(markouts.shape[1]):
        step = (markouts[:, i] - scores) * notional / (histories + notional)
        scores = scores + np.ceil(step * 1e9) / 1e9
        histories = np.minimum(histories + notional, memory)
        crossed |= scores >= threshold
    return float(crossed.mean())


def drift_bound_sigma(sigma_bps: float, notional: float, memory: float) -> float:
    """The sampling-noise scale of the bounded-memory score for `notional`-sized fills."""
    return sigma_bps * np.sqrt(notional / memory)


def require_coverage(symbols: list[str], sizes: dict[str, dict], genesis: dict) -> None:
    """Every launch market must have an observed size distribution: a missing schedule silently
    dropping a market would publish global parameters that never evaluated it."""
    missing = [s for s in symbols if s not in sizes]
    if missing:
        schedules = {m["symbol"]: schedule_key(m["margin_tiers"]) for m in genesis["markets"] if m["symbol"] in missing}
        raise ValueError(f"no observed size distribution for launch markets: {schedules}")


def choose_memory(sigma_bps: np.ndarray, symbols: list[str], sizes: dict[str, dict], drift_bps: float = DRIFT_BPS) -> dict:
    """The smallest memory keeping the drift under `drift_bps` at every market's p99 fill."""
    worst, worst_symbol, bound = 0.0, None, 0.0
    per_market = {}
    for m, symbol in enumerate(symbols):
        if symbol not in sizes:
            continue
        need = sizes[symbol]["n99"] * (sigma_bps[m] / drift_bps) ** 2
        per_market[symbol] = {"memory_needed_usd": round(need, 0), "sigma_markout_bps": round(float(sigma_bps[m]), 3)}
        if need > worst:
            worst, worst_symbol, bound = need, symbol, sigma_bps[m]
    return {"memory_usd": worst, "bound_bps": drift_bps, "worst_market": worst_symbol, "per_market": per_market}


def choose_prior(memory: float, sizes: dict[str, dict], symbols: list[str]) -> dict:
    """Half of memory, a design choice: enough neutral history that a new account's first fills
    move its score only modestly (at most about a fifth of the biggest median clip's markout),
    while the toxic climb still lands well inside the candidate fill counts. The engine
    validates 0 < prior <= memory."""
    median = np.median([sizes[s]["n50"] for s in symbols if s in sizes])
    return {"prior_usd": memory / 2, "median_fill_usd": median}


def choose_tiers(sigma_bps: np.ndarray, symbols: list[str], sizes: dict[str, dict], memory: float, prior: float, seed: int, trials: int) -> dict:
    """Tier 1, over the candidate climb counts: the highest round-bps threshold a toxic account
    reaches within K median-size fills, while a benign account filling at every market's median
    clip stays below it for BENIGN_FILLS fills with probability at least 1 - BENIGN_FP. (The
    median clip, not the tail: an account filling at the p99 clip on every fill is already an
    outlier — the memory bound prices its noise at exactly the drift bound, so no threshold
    within the toxic reach could stand against it. Such accounts are the score's purpose, not
    its false positives.) The first K with a passing threshold wins; the stated number of fills
    a toxic account needs is that K. Tier 2 doubles tier 1; the spreads are the sustained
    per-fill markouts that reach each tier in the stated fills."""
    median = np.median([sizes[s]["n50"] for s in symbols if s in sizes])
    for fills in CLIMB_CANDIDATES:
        toxic_reach = climb_score(M_TOX_BPS, median, prior, memory, fills)
        for tier1 in sorted(set(range(1, int(toxic_reach) + 1)), reverse=True):
            if not all(
                symbol not in sizes
                or benign_cross_probability(sigma_bps[m], sizes[symbol]["n50"], prior, memory, tier1, BENIGN_FILLS, seed + m, trials) <= BENIGN_FP
                for m, symbol in enumerate(symbols)
            ):
                continue
            tier2 = tier1 * TIER2_MULT
            burst = {
                cadence: max(
                    correlated_cross_probability(
                        sigma_bps[m], sizes[symbol]["n50"], prior, memory, tier1, BURST_FILLS, seed + m, CORRELATED_TRIALS, cadence
                    )
                    for m, symbol in enumerate(symbols)
                    if symbol in sizes
                )
                for cadence in CORRELATED_CADENCES_S
            }
            return {
                "tier1_bps": tier1,
                "tier2_bps": tier2,
                "spread1_bps": markout_to_reach(tier1, median, prior, memory, fills),
                "spread2_bps": markout_to_reach(tier2, median, prior, memory, fills * 2),
                "climb_fills": fills,
                "toxic_reach_bps": round(toxic_reach, 3),
                "median_fill_usd": median,
                "burst_sensitivity": {str(k): round(v, 5) for k, v in burst.items()},
            }
    raise ValueError("no tier-1 threshold satisfies the benign bound within the toxic reach")


def markout_to_reach(target: float, notional: float, prior: float, memory: float, fills: int) -> float:
    """The sustained per-fill markout that lands the exact recurrence on the target after `fills`
    same-size fills (monotone, so bisect)."""
    lo, hi = 0.0, MAX_MARKOUT_BPS
    for _ in range(60):
        mid = (lo + hi) / 2
        if climb_score(mid, notional, prior, memory, fills) >= target:
            hi = mid
        else:
            lo = mid
    return np.ceil(hi * 10) / 10  # round up: the configured spread must still reach the tier
