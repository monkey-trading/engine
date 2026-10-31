"""Stress-trigger calibration: fast-move thresholds and window, divergence shares, and utilisation
levels, from the market statistics and the #111 Monte Carlo.

The engine's automatic stress level rises at once when any reading meets a level's trigger and
steps down only after `stress_cooldown_ms` (`stress.rs`). The launch triggers are absolute
dev values (#118); this module calibrates them so each trigger fires when the house's risk
actually rises, not on noise:

- *Fast move*: the trigger reads the largest distance of the index mid from its trailing window's
  lowest or highest mid, in bps (`record_move`). Each market's per-second volatility comes from its
  hourly bars' Parkinson range; minute-scale paths simulated under it give each candidate
  threshold's share of time at each stress level, with the engine's bucket window and cooldown
  hysteresis, and the volatility percentile a firing hour selects. A threshold is chosen so the
  flow-weighted time at level 1 stays under `MOVE_L1_TIME` and at level 2 under `MOVE_L2_TIME`,
  and every market's time at level 1 stays under `MOVE_L1_MAX_TIME`; the most protective candidate
  (lowest thresholds) passing those bounds is taken, and the median volatility percentile of the
  firing hours is reported (it rises mechanically with the threshold, so it cannot choose).
- *Divergence*: a reading of `d` bps means the venue mids sit `d` apart. With the protocol's two
  fresh sources the index mid is their midpoint, and a fill at the index mid loses up to `d/2` bps
  if the mark snaps to the other venue (`d/4` expected, uniform). With more fresh sources the index
  is their median, which can sit closer to one extreme, so `d/2` understates the worst case — the
  chosen shares are conservative under it. A share `s` of a market's bound `B` fires at
  `d = ceil(s*B/10000)` (the engine truncates the share). The chosen share is the lowest whose
  worst-case stale-fill loss at fire covers the level's minimum spread on the tightest-bound
  market (`d/2 >= min_spread`); the expected-cost rule (`d/4 >= min_spread`) is reported too and is
  unreachable on the tightest-bound markets.
- *Utilisation*: the house's exposure in a market over its cap, from the Monte Carlo's hourly
  books. Each candidate level's fire rate (share of open hours at or above it, per floor family)
  and the house's mean exposure on those hours decide the levels: level 2 sits just inside the
  cap (widening is the last protection before refusals start); level 1 is the lowest candidate
  whose flow-weighted fire rate stays under `UTIL_L1_TIME` on the launch-relevant floor families
  and under `UTIL_L1_MAX_TIME` on any market with at least 1% of the flow. The per-fire risk is
  the exposure at fire times the market's p99 next-mark gap (`gap_tables`).
"""

import math
from dataclasses import replace

import numpy as np

from model import prices, simulate

BPS = 1e4
MOVE_BUCKETS = 10  # stress.rs MOVE_BUCKETS

# Choice bounds: flow-weighted shares of trading time a trigger may hold a level ("not on noise"),
# and the worst single market's level-1 share.
MOVE_L1_TIME = 0.01
MOVE_L2_TIME = 0.001
MOVE_L1_MAX_TIME = 0.05
UTIL_L1_TIME = 0.02  # flow-weighted fire rate bound per launch-relevant floor family
UTIL_L1_MAX_TIME = 0.05  # bound for any single market with at least 1% of flow

# The stress levels' spreads, which this calibration takes as given (decision #1 s5).
LEVEL_MIN_SPREAD_BPS = (2, 10)

MOVE_WINDOWS_MS = (30_000, 60_000, 120_000)
MOVE_CANDIDATES_BPS = ((50, 150), (60, 150), (75, 200), (100, 250), (150, 300))
UTIL_CANDIDATES_BPS = (5000, 6000, 7000, 7500, 8000, 8500, 9000, 9500, 9800)
UTIL_FAMILIES = {"uniform_1M": 1e6, "uniform_10M": 1e7}


def engine_move_measures(mids: np.ndarray, window_s: int) -> np.ndarray:
    """The engine's move reading at one-second cadence: for each second, the larger of the current
    mid's distance in bps from the trailing window's lowest and highest mid, with the engine's
    bucket compaction (`record_move`): one bucket per `window_s/10` seconds holding its low and
    high, a bucket dropped once its newest mid is more than `window_s` old."""
    width = max(window_s // MOVE_BUCKETS, 1)
    n = mids.shape[-1]
    flat = mids.reshape(-1, n)
    out = np.empty_like(flat)
    for row in range(flat.shape[0]):
        buckets = []
        for t in range(n):
            mid = flat[row, t]
            while buckets and t - buckets[0][1] > window_s:
                buckets.pop(0)
            if buckets and t - buckets[-1][0] < width:
                first, _, low, high = buckets[-1]
                buckets[-1] = (first, t, min(low, mid), max(high, mid))
            else:
                buckets.append((t, t, mid, mid))
            low = min(b[2] for b in buckets)
            high = max(b[3] for b in buckets)
            out[row, t] = max((mid - low) / low * BPS, (high - mid) / high * BPS)
    return out.reshape(mids.shape)


def stress_levels(measure: np.ndarray, t1: float, t2: float, cooldown_s: int) -> np.ndarray:
    """The automatic level at each second with the engine's hysteresis (`step_auto`): up at once
    when the reading meets a threshold, down only `cooldown_s` after the last change."""
    flat = measure.reshape(-1)
    out = np.empty(flat.shape, dtype=np.int8)
    auto, since = 0, -cooldown_s
    for i, m in enumerate(flat):
        target = 2 if m >= t2 else 1 if m >= t1 else 0
        if target > auto or (target < auto and i - since >= cooldown_s):
            auto, since = target, i
        out[i] = auto
    return out.reshape(measure.shape)


def _levels_from_targets(targets: np.ndarray, cooldown_s: int) -> np.ndarray:
    """The hysteresis level for a target series, in time proportional to the events: the level
    changes only at target changes and at cooldown lapses after a rise. Each span between events
    fills with the constant level it carries, matching `stress_levels` second by second."""
    n = len(targets)
    change = np.nonzero(np.diff(targets, prepend=targets[:1]))[0]
    out = np.empty(n, dtype=np.int8)
    auto, since, pos, ci = 0, -cooldown_s, 0, 0
    while pos < n:
        nxt = change[ci] if ci < len(change) else n
        tau = int(targets[pos])
        if tau > auto:
            auto, since = tau, pos
            out[pos:nxt] = auto
            pos, ci = nxt, ci + 1
        elif tau == auto:
            out[pos:nxt] = auto
            pos, ci = nxt, ci + 1
        else:
            lapse = since + cooldown_s
            if lapse >= nxt:
                out[pos:nxt] = auto
                pos, ci = nxt, ci + 1
            else:
                if lapse > pos:
                    out[pos:lapse] = auto
                auto, since = tau, max(lapse, pos)
                pos = max(lapse, pos)
    return out


def _crossing_events(measure: np.ndarray, t1: float, t2: float, cooldown_s: int) -> np.ndarray:
    """`stress_levels` through `_levels_from_targets`, per simulated path: each row is an
    independent hour, so the cooldown must not leak across rows."""
    return np.stack(
        [
            _levels_from_targets(np.where(row >= t2, 2, np.where(row >= t1, 1, 0)).astype(np.int8), cooldown_s)
            for row in measure.reshape(-1, measure.shape[-1])
        ]
    ).reshape(measure.shape)


def move_tables(bars: np.ndarray, symbols: list[str], mix: np.ndarray, hours_per_market: int, seed: int) -> list[dict]:
    """Per market and candidate (window, thresholds): the share of seconds at each stress level
    with hysteresis, and the median volatility percentile of the hours a level-1 firing falls in.
    Each open hour is simulated as one minute-scale Brownian path at the hour's Parkinson vol, so
    the tables measure what the trigger reads in a typical hour of each volatility regime."""
    is_open = bars[..., 0] != prices.CLOSED
    parkinson = np.where(is_open, (bars[..., 1] - bars[..., 2]).astype(np.float64) / 1e6 / np.sqrt(4 * np.log(2)), 0.0)
    sigma_sec = parkinson / np.sqrt(3600)
    rng = np.random.default_rng(seed)
    rows = []
    warmup_s = 2 * max(w // 1000 for w in MOVE_WINDOWS_MS)
    for m, symbol in enumerate(symbols):
        hours = np.nonzero(is_open[:, m])[0]
        take = hours[rng.integers(0, len(hours), size=min(hours_per_market, len(hours)))]
        sigma = sigma_sec[take, m]
        # Two window-lengths of warm-up give every hour a full window from its first recorded
        # second, as the engine's window persists across hours; warm-up readings are discarded.
        draws = rng.standard_normal((len(take), warmup_s + 3600)) * sigma[:, None]
        mids = np.exp(np.cumsum(draws, axis=1))
        sig_min = np.median(sigma) * np.sqrt(60) * BPS
        for window_s in (w // 1000 for w in MOVE_WINDOWS_MS):
            measure = engine_move_measures(mids, window_s)[:, warmup_s:]
            q = np.quantile(measure, [0.5, 0.9, 0.99, 0.999])
            for t1, t2 in MOVE_CANDIDATES_BPS:
                levels = _crossing_events(measure, t1, t2, cooldown_s=300)
                fired = levels >= 1
                vol_pct = float(np.mean([np.mean(sigma[h] > sigma) for h in np.unique(np.nonzero(fired.reshape(len(take), -1))[0])])) if fired.any() else None
                rows.append(
                    {
                        "symbol": symbol,
                        "window_s": window_s,
                        "mix": float(mix[m]),
                        "sigma_min_bps": round(float(sig_min), 3),
                        "q50": round(float(q[0]), 3),
                        "q90": round(float(q[1]), 3),
                        "q99": round(float(q[2]), 3),
                        "q999": round(float(q[3]), 3),
                        "t1": t1,
                        "t2": t2,
                        "time_l1": round(float((levels >= 1).mean()), 6),
                        "time_l2": round(float((levels >= 2).mean()), 6),
                        "vol_pct_l1": None if vol_pct is None else round(vol_pct, 3),
                    }
                )
    return rows


def choose_move(rows: list[dict], window_s: int) -> dict:
    """The most protective (t1, t2) at `window_s` passing every bound: flow-weighted time at
    level 1 under MOVE_L1_TIME and at level 2 under MOVE_L2_TIME, and every market's level-1 time
    under MOVE_L1_MAX_TIME. Selection quality (the volatility percentile firing hours land in) is
    reported, not bounded: it rises mechanically with the threshold, so it cannot choose."""
    pool = [r for r in rows if r["window_s"] == window_s]
    candidates = sorted({(r["t1"], r["t2"]) for r in pool})
    feasible = []
    for t1, t2 in candidates:
        cand = [r for r in pool if (r["t1"], r["t2"]) == (t1, t2)]
        total_mix = sum(r["mix"] for r in cand)
        l1 = sum(r["time_l1"] * r["mix"] for r in cand) / total_mix
        l2 = sum(r["time_l2"] * r["mix"] for r in cand) / total_mix
        worst = max(r["time_l1"] for r in pool if (r["t1"], r["t2"]) == (t1, t2))
        if l1 <= MOVE_L1_TIME and l2 <= MOVE_L2_TIME and worst <= MOVE_L1_MAX_TIME:
            pcts = [
                (r["vol_pct_l1"], r["mix"])
                for r in pool
                if (r["t1"], r["t2"]) == (t1, t2) and r["vol_pct_l1"] not in (None, "") and r["mix"] > 0
            ]
            mean_pct = sum(p * w for p, w in pcts) / sum(w for _, w in pcts) if pcts else None
            feasible.append((t1, t2, mean_pct))
    if not feasible:
        raise ValueError(f"no move candidate at window {window_s}s passes the bounds")
    t1, t2, mean_pct = min(feasible, key=lambda c: (c[0], c[1]))
    return {"window_s": window_s, "move_bps_l1": t1, "move_bps_l2": t2, "mean_vol_pct_l1": mean_pct}


def divergence_tables(genesis: dict, launch_shares: tuple = (6000, 8000), calibrated: dict | None = None) -> list[dict]:
    """Per market: the divergence in bps, and the expected (uniform) and worst-case stale-fill loss
    at fire, for the launch and the calibrated shares of its divergence bound."""
    calibrated = calibrated or choose_divergence(genesis)
    spread1, spread2 = LEVEL_MIN_SPREAD_BPS
    rows = []
    for market in genesis["markets"]:
        bound = market["price_rules"]["max_divergence_bps"]
        shares = ((launch_shares[0], launch_shares[1]), (calibrated["divergence_share_l1"], calibrated["divergence_share_l2"]))
        for label, (s1, s2) in (("launch", shares[0]), ("calibrated", shares[1])):
            # The engine truncates the share (stress.rs `divergence_share_bps`), so a share s
            # first fires at ceil(s*B/10000) bps of divergence.
            d1, d2 = math.ceil(s1 * bound / 10000), math.ceil(s2 * bound / 10000)
            rows.append(
                {
                    "symbol": market["symbol"],
                    "bound_bps": bound,
                    "share": label,
                    "divergence_share_l1": s1,
                    "divergence_share_l2": s2,
                    "divergence_bps_l1": round(d1, 2),
                    "divergence_bps_l2": round(d2, 2),
                    "expected_cost_l1": round(d1 / 4, 2),
                    "worst_cost_l1": round(d1 / 2, 2),
                    "spread_l1": spread1,
                    "expected_cost_l2": round(d2 / 4, 2),
                    "worst_cost_l2": round(d2 / 2, 2),
                    "spread_l2": spread2,
                }
            )
    return rows


def choose_divergence(genesis: dict) -> dict:
    """The chosen level-1 share: the lowest whose worst-case stale-fill loss at fire covers the
    level's minimum spread on the tightest-bound market. The engine truncates the share
    (`divergence_share_bps`), so a share `s` on a bound `B` first fires at `ceil(s*B/10000)` bps;
    the loss there is `ceil(s*B/10000)/2`, and the lowest share with `ceil(s*B/10000) >= 2*spread`
    is `floor((2*spread-1)*10000/B) + 1`. Level 2's cost criterion is infeasible on the tightest
    markets (it needs a 20-bps reading where no bound reaches it); the fallback is 9500, the share
    that fires one bps inside the widest bound. The expected-cost rule (`d/4 >= min_spread`) is
    reported in `divergence_tables` and is unreachable on the tightest markets."""
    bounds = [m["price_rules"]["max_divergence_bps"] for m in genesis["markets"] if m["price_rules"]["max_divergence_bps"] > 0]
    tightest = min(bounds)

    def lowest_share(spread_bps: int) -> int | None:
        share = (2 * spread_bps - 1) * 10_000 // tightest + 1
        return share if share <= 10_000 else None

    return {
        "divergence_share_l1": lowest_share(LEVEL_MIN_SPREAD_BPS[0]),
        "divergence_share_l2": lowest_share(LEVEL_MIN_SPREAD_BPS[1]) or 9500,
        "tightest_bound_bps": tightest,
        "level2_cost_feasible": lowest_share(LEVEL_MIN_SPREAD_BPS[1]) is not None,
    }


def gap_tables(bars: np.ndarray, symbols: list[str]) -> dict[str, dict]:
    """Per market, the p99 and p999 of the next open hour's absolute log move from an open hour's
    close to the next open hour's close, in bps: the gap a market's open book can take in one
    mark-to-market step, from the hourly history."""
    is_open = bars[..., 0] != prices.CLOSED
    # The close's log level: each open bar adds its gap and close move; closed bars leave it flat,
    # so a closure's jump lands on the reopening bar (as in `prices.build`).
    log_close = np.cumsum(np.where(is_open, (bars[..., 0] + bars[..., 3]).astype(np.float64) / 1e6, 0.0), axis=0)
    out = {}
    for m, symbol in enumerate(symbols):
        hours = np.nonzero(is_open[:, m])[0]
        # The book's mark moves from one open hour's close to the next open hour's close,
        # whenever that hour comes (a closure freezes the mark in between).
        gaps = [abs(log_close[b, m] - log_close[a, m]) for a, b in zip(hours[:-1], hours[1:])]
        q99, q999 = np.quantile(gaps, [0.99, 0.999])
        out[symbol] = {"gap_p99_bps": round(float(q99) * BPS, 2), "gap_p999_bps": round(float(q999) * BPS, 2)}
    return out


def utilisation(scenario, markets, bars, flow, floors_by_family: dict[str, np.ndarray], gaps: dict[str, dict], workers: int) -> list[dict]:
    """The Monte Carlo at each floor family with utilisation instrumentation on: per market and
    candidate level, the share of open hours at or above the level and the house's mean absolute
    exposure on those hours."""
    from model.experiments import NO_TOTAL_LIMIT

    families = list(floors_by_family)
    lanes = np.array([floors_by_family[f] for f in families])
    task = (
        replace(scenario, name="utilisation", utilisation_stats=True),
        markets,
        bars,
        flow,
        lanes,
        np.full(len(families), NO_TOTAL_LIMIT),
    )
    (out,) = simulate.run_many([task], workers)
    return utilisation_rows(out, markets.symbols, families, gaps)


def utilisation_rows(out: dict, symbols: list[str], families: list[str], gaps: dict[str, dict]) -> list[dict]:
    """The utilisation table from instrumented runs, batches summed (`simulate.run_many`
    concatenates the leading batch axis). Fire rates share all open hours; the exposure and
    hole rows cover the hours at or above the level."""
    hist = out["util_hist"].sum(axis=0)  # (102, markets, lanes) open-hour counts
    expo = out["util_expo"].sum(axis=0)  # (102, markets, lanes) USD of absolute exposure
    rows = []
    for lane, family in enumerate(families):
        for m, symbol in enumerate(symbols):
            n_open = hist[:, m, lane].sum()
            if n_open <= 0:
                continue
            at_or_above_count = np.cumsum(hist[::-1, m, lane])[::-1]
            at_or_above_expo = np.cumsum(expo[::-1, m, lane])[::-1]
            gap_p99 = gaps[symbol]["gap_p99_bps"]
            for level in UTIL_CANDIDATES_BPS:
                bucket = min(level // 100, 100)
                n_ge = at_or_above_count[bucket]
                expo_ge = at_or_above_expo[bucket]
                mean_expo = expo_ge / n_ge if n_ge > 0 else 0.0
                rows.append(
                    {
                        "family": family,
                        "symbol": symbol,
                        "level_bps": level,
                        "open_hours": int(n_open),
                        "fire_rate": round(float(n_ge / n_open), 6),
                        "mean_exposure_usd": round(float(mean_expo), 0),
                        "gap_p99_bps": gap_p99,
                        "p99_hole_usd": round(float(mean_expo * gap_p99 / BPS), 0),
                    }
                )
    return rows


def choose_utilisation(rows: list[dict], flow_by_symbol: dict[str, float]) -> dict:
    """Level 2 just inside the cap: widening is the last protection before refusals start. Level 1
    is the lowest candidate whose flow-weighted fire rate stays under UTIL_L1_TIME, and every
    market with at least 1% of flow under UTIL_L1_MAX_TIME, on the launch-relevant floor families
    (the house-cash 1% choice and the uniform $10M; the tiny $1M family's caps keep utilisation
    high whatever the level, which the table reports rather than calibrates to)."""
    flow = {s: w for s, w in flow_by_symbol.items() if w > 0}
    total = sum(flow.values())
    families = sorted({r["family"] for r in rows} - {"uniform_1M"})
    eligible = [s for s, w in flow.items() if w / total >= 0.01]
    for level in sorted({r["level_bps"] for r in rows if r["level_bps"] < 9500}):
        fire = {f: 0.0 for f in families}
        worst = 0.0
        for r in rows:
            if r["level_bps"] != level or r["family"] == "uniform_1M" or r["symbol"] not in flow:
                continue
            fire[r["family"]] += r["fire_rate"] * flow[r["symbol"]]
            if r["symbol"] in eligible:
                worst = max(worst, r["fire_rate"])
        fire = {f: w / total for f, w in fire.items()}
        if max(fire.values(), default=1.0) <= UTIL_L1_TIME and worst <= UTIL_L1_MAX_TIME:
            return {
                "utilisation_bps_l1": level,
                "utilisation_bps_l2": 9500,
                "fire_rate_by_family": {f: round(v, 5) for f, v in fire.items()},
                "worst_market_fire_rate": round(worst, 5),
            }
    raise ValueError("no utilisation candidate passes the bounds")
