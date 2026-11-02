"""Hourly price paths by block bootstrap of historical weeks.

A path is a sequence of whole historical weeks, sampled in blocks of consecutive weeks so that
volatility clusters survive. By default every market takes the same historical weeks, which keeps
the cross-market correlation, the weekend and holiday gaps, and the session calendar of the
history. Returns are log returns, so `vol_scale` scales every move, gaps included.
"""

from dataclasses import dataclass
from pathlib import Path

import numpy as np

CLOSED = np.iinfo(np.int32).min
WEEK = 168


@dataclass
class Paths:
    """Price levels per (path, hour, market), relative to 1.0 before the first hour."""

    open: np.ndarray
    high: np.ndarray
    low: np.ndarray
    close: np.ndarray
    is_open: np.ndarray  # bool: the market has a bar this hour
    parkinson: np.ndarray  # the bar's Parkinson volatility, zero when closed
    fx: np.ndarray | None = None  # USD per unit of the quote currency, relative; 1 for USD markets


def load_bars(path: Path) -> tuple[np.ndarray, list[str]]:
    data = np.load(path)
    return data["bars"], [str(s) for s in data["symbols"]]


def sample_weeks(rng: np.random.Generator, n_paths: int, n_weeks: int, history_weeks: int, block: int) -> np.ndarray:
    """(n_paths, n_weeks) indices of historical weeks, in circular blocks of `block` weeks."""
    blocks = -(-n_weeks // block)
    starts = rng.integers(0, history_weeks, size=(n_paths, blocks))
    weeks = (starts[:, :, None] + np.arange(block)) % history_weeks
    return weeks.reshape(n_paths, -1)[:, :n_weeks]


def drift_per_open_hour(bars: np.ndarray) -> np.ndarray:
    """Each market's mean log return per open hour over the history."""
    is_open = bars[..., 0] != CLOSED
    moves = np.where(is_open, bars[..., 0].astype(np.float64) + bars[..., 3], 0.0) / 1e6
    return moves.sum(axis=0) / is_open.sum(axis=0)


def _moves(bars: np.ndarray, weeks: np.ndarray, hours: int, drift: np.ndarray | None) -> tuple[np.ndarray, np.ndarray]:
    """(is_open, moves) of the sampled weeks: per (path, hour, market), the log moves
    (gap, high, low, close) against the open, with the drift taken off every open hour's gap.
    `weeks` is (paths, n_weeks), or (paths, n_weeks, markets) to give each market its own."""
    n_markets = bars.shape[1]
    by_week = bars.reshape(-1, WEEK, n_markets, 4)
    if weeks.ndim == 2:
        sampled = by_week[weeks]  # (paths, n_weeks, WEEK, markets, 4)
    else:
        market = np.arange(n_markets)
        sampled = by_week[weeks[:, :, None, :], np.arange(WEEK)[None, None, :, None], market]
    sampled = sampled.reshape(weeks.shape[0], -1, n_markets, 4)[:, :hours]
    is_open = sampled[..., 0] != CLOSED
    moves = np.where(is_open[..., None], sampled, 0).astype(np.float64) / 1e6
    if drift is not None:
        moves[..., 0] -= np.where(is_open, drift, 0.0)
    return is_open, moves


def build(
    bars: np.ndarray,
    weeks: np.ndarray,
    hours: int,
    vol_scale: float = 1.0,
    drift: np.ndarray | None = None,
    own_weeks: np.ndarray | None = None,
    shared: np.ndarray | None = None,
) -> Paths:
    """Price levels for the sampled weeks. `weeks` is (paths, n_weeks), or (paths, n_weeks, markets)
    to give each market its own weeks. `drift`, per market, is taken off every open hour's gap, which
    moves the whole bar and keeps its open inside its range, before `vol_scale` scales the moves.

    With `own_weeks` (paths, n_weeks, markets) and `shared` (markets), market m's moves are
    `shared[m]` times its moves in `weeks` plus `sqrt(1 - shared[m]**2)` times its moves in its
    own weeks: a market whose returns correlate `shared[m]` with the market that `weeks` gives
    the same history. Its sessions are those of `weeks`. A market with `shared` 1 is unchanged.
    The gap and the close are mixed linearly, so their variance is each source's; the high and
    low combine in quadrature, so the bar's range stays at its history's scale."""
    is_open, moves = _moves(bars, weeks, hours, drift)
    if own_weeks is not None:
        _, own = _moves(bars, own_weeks, hours, drift)
        own = np.where(is_open[..., None], own, 0.0)
        a = np.asarray(shared, dtype=np.float64)[None, None, :, None]
        b = np.sqrt(np.maximum(1.0 - a * a, 0.0))
        mixed = a * moves + b * own
        # The ranges combine in quadrature, keeping the bar's range at its history's scale, and
        # are widened where needed to contain the open and the close.
        mixed[..., 1] = np.maximum(np.sqrt((a[..., 0] * moves[..., 1]) ** 2 + (b[..., 0] * own[..., 1]) ** 2), np.maximum(mixed[..., 3], 0.0))
        mixed[..., 2] = np.minimum(-np.sqrt((a[..., 0] * moves[..., 2]) ** 2 + (b[..., 0] * own[..., 2]) ** 2), np.minimum(mixed[..., 3], 0.0))
        moves = mixed
    moves *= vol_scale
    gap, hi, lo, cl = moves[..., 0], moves[..., 1], moves[..., 2], moves[..., 3]
    log_close = np.cumsum(gap + cl, axis=1)
    prev_close = np.concatenate([np.zeros_like(log_close[:, :1]), log_close[:, :-1]], axis=1)
    log_open = prev_close + gap
    parkinson = np.where(is_open, (hi - lo) / np.sqrt(4 * np.log(2)), 0.0)
    return Paths(
        open=np.exp(log_open),
        high=np.exp(log_open + hi),
        low=np.exp(log_open + lo),
        close=np.exp(log_close),
        is_open=is_open,
        parkinson=parkinson,
    )


def select(paths: Paths, columns: np.ndarray, fx_columns: np.ndarray, fx_invert: np.ndarray) -> Paths:
    """The markets at `columns` of `paths`, with each market's FX path: the close of its FX
    column (USD per unit of EUR or GBP), its reciprocal for yen (the column is USD-JPY), or 1
    for a USD market (-1)."""
    fx = np.ones(paths.close.shape[:2] + (len(columns),))
    for i, (col, invert) in enumerate(zip(fx_columns, fx_invert)):
        if col >= 0:
            fx[..., i] = 1.0 / paths.close[..., col] if invert else paths.close[..., col]
    pick = lambda a: a[..., columns]  # noqa: E731
    return Paths(pick(paths.open), pick(paths.high), pick(paths.low), pick(paths.close), pick(paths.is_open), pick(paths.parkinson), fx)


def next_open_hour(is_open: np.ndarray) -> np.ndarray:
    """(paths, hours + 1, markets) int: the first open hour at or after each hour; `hours` when the
    market does not open again within the path."""
    n_paths, hours, n_markets = is_open.shape
    out = np.full((n_paths, hours + 1, n_markets), hours, dtype=np.int32)
    for t in range(hours - 1, -1, -1):
        out[:, t] = np.where(is_open[:, t], t, out[:, t + 1])
    return out
