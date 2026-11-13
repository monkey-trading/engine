"""Price paths at the trigger's resolution: 10 s bins over the cap study's hourly bars.

A loss-rate trigger reads the house's realised loss over windows of one to sixty minutes, so the
honest flow is settled on a 10 s path. Each open hour of a market's bars (`research/cap-calibration`,
log moves against the hour's open, with the gap from the last close) becomes 360 bins: a Brownian
bridge from the hour's open to its close whose variance is the hour's Parkinson variance, so the
hour's own volatility, its gaps and its sessions carry over. A closed hour holds the last price.
Each market's mean open-hour return is taken off its gaps, as the cap study does, so the history's
trend does not decide who wins.
"""

from dataclasses import dataclass

import numpy as np

BIN_S = 10.0
BINS_PER_HOUR = 360
CLOSED = np.iinfo(np.int32).min


@dataclass
class Path:
    """One market's log price per bin, relative to its first open, and whether it trades then."""

    x: np.ndarray  # (hours * 360,) log price at each bin's start
    is_open: np.ndarray  # (hours * 360,) bool
    hour_min: np.ndarray  # (hours,) lowest x within the hour
    hour_max: np.ndarray  # (hours,) highest x within the hour

    @property
    def bins(self) -> int:
        return len(self.x)


def bridge(n: int, sd: np.ndarray, rng: np.random.Generator) -> np.ndarray:
    """(hours, n) Brownian bridges from 0 to 0 over `n` steps, each hour's total variance `sd**2`."""
    steps = rng.standard_normal((len(sd), n)) * (np.asarray(sd)[:, None] / np.sqrt(n))
    walk = np.concatenate([np.zeros((len(sd), 1)), np.cumsum(steps, axis=1)], axis=1)
    frac = np.arange(n + 1) / n
    return (walk - frac * walk[:, -1:])[:, :n]


def from_bars(moves: np.ndarray, rng: np.random.Generator, demean: bool = True) -> Path:
    """A market's 10 s path from its hourly bars `moves` (hours, 4) int32: (gap, high, low, close)
    in millionths of log price against the hour's open, `CLOSED` for a closed hour."""
    is_open_h = moves[:, 0] != CLOSED
    m = np.where(is_open_h[:, None], moves, 0).astype(np.float64) / 1e6
    gap, hi, lo, cl = m[:, 0], m[:, 1], m[:, 2], m[:, 3]
    if demean and is_open_h.any():
        gap = gap - np.where(is_open_h, (gap + cl)[is_open_h].mean(), 0.0)
    log_close = np.cumsum(gap + cl)
    log_open = log_close - cl
    sd = np.where(is_open_h, (hi - lo) / np.sqrt(4 * np.log(2)), 0.0)
    frac = np.arange(BINS_PER_HOUR) / BINS_PER_HOUR
    x = log_open[:, None] + cl[:, None] * frac + bridge(BINS_PER_HOUR, sd, rng)
    # A closed hour holds the last close (the first hours, before any open, hold the first open).
    held = np.where(is_open_h, 0, 1)
    last_close = np.concatenate([[log_open[np.argmax(is_open_h)] if is_open_h.any() else 0.0], log_close[:-1]])
    x = np.where(held[:, None] == 1, last_close[:, None], x)
    is_open = np.repeat(is_open_h, BINS_PER_HOUR)
    return Path(x=x.ravel(), is_open=is_open, hour_min=x.min(axis=1), hour_max=x.max(axis=1))


def from_array(x: np.ndarray, is_open: np.ndarray | None = None) -> Path:
    """A path from a log price per bin, padded to whole hours with its last value."""
    n = -(-len(x) // BINS_PER_HOUR) * BINS_PER_HOUR
    padded = np.concatenate([x, np.full(n - len(x), x[-1])])
    open_ = np.ones(n, dtype=bool) if is_open is None else np.concatenate([is_open, np.zeros(n - len(x), dtype=bool)])
    h = padded.reshape(-1, BINS_PER_HOUR)
    return Path(x=padded, is_open=open_, hour_min=h.min(axis=1), hour_max=h.max(axis=1))


def next_open(is_open: np.ndarray) -> np.ndarray:
    """Each bin's first open bin at or after it; `len(is_open)` when none follows."""
    n = len(is_open)
    idx = np.where(is_open, np.arange(n), n)
    return np.minimum.accumulate(idx[::-1])[::-1]


class Crossings:
    """First crossings of a level on a `Path`, for close-outs: a sparse table of hourly minima and
    maxima finds the first hour that reaches the level, then its bins give the first bin."""

    def __init__(self, path: Path):
        self.path = path
        self.lo = [path.hour_min]
        self.hi = [path.hour_max]
        k = 1
        while len(self.lo[-1]) > k:
            lo, hi = self.lo[-1], self.hi[-1]
            self.lo.append(np.minimum(lo[:-k], lo[k:]))
            self.hi.append(np.maximum(hi[:-k], hi[k:]))
            k *= 2

    def _scan(self, start: np.ndarray, stop: np.ndarray, level: np.ndarray, below: np.ndarray) -> np.ndarray:
        """First bin in [start, stop] (each within one hour) at or past `level`, or -1."""
        x = self.path.x
        width = BINS_PER_HOUR
        idx = start[:, None] + np.arange(width)[None, :]
        inside = idx <= stop[:, None]
        v = x[np.minimum(idx, len(x) - 1)]
        hit = inside & np.where(below[:, None], v <= level[:, None], v >= level[:, None])
        first = hit.argmax(axis=1)
        return np.where(hit.any(axis=1), start + first, -1)

    def _first_hour(self, h1: np.ndarray, h2: np.ndarray, level: np.ndarray, below: np.ndarray) -> np.ndarray:
        """First hour in [h1, h2] whose range reaches `level`, or -1; descends the sparse table."""
        pos = h1.copy()
        for k in range(len(self.lo) - 1, -1, -1):
            span = 1 << k
            ok = pos + span - 1 <= h2
            at = np.minimum(pos, len(self.lo[k]) - 1)
            clear = np.where(below, self.lo[k][at] > level, self.hi[k][at] < level)
            pos = np.where(ok & clear, pos + span, pos)
        at = np.minimum(pos, len(self.lo[0]) - 1)
        reach = np.where(below, self.lo[0][at] <= level, self.hi[0][at] >= level)
        return np.where((pos <= h2) & reach, pos, -1)

    def first(self, start: np.ndarray, stop: np.ndarray, level: np.ndarray, below: np.ndarray, batch: int = 20_000) -> np.ndarray:
        """For each position, the first bin in (start, stop] where the path is at or below `level`
        (`below`) or at or above it, or -1 when it never is."""
        out = np.full(len(start), -1, dtype=np.int64)
        for a in range(0, len(start), batch):
            s, e, lv, b = start[a : a + batch] + 1, stop[a : a + batch], level[a : a + batch], below[a : a + batch]
            res = np.full(len(s), -1, dtype=np.int64)
            live = s <= e
            hs, he = s // BINS_PER_HOUR, e // BINS_PER_HOUR
            # The first hour's part.
            first_stop = np.minimum(e, (hs + 1) * BINS_PER_HOUR - 1)
            sel = np.flatnonzero(live)
            res[sel] = self._scan(s[sel], first_stop[sel], lv[sel], b[sel])
            # Whole hours between.
            sel = np.flatnonzero(live & (res < 0) & (he - hs >= 2))
            if len(sel):
                hc = self._first_hour(hs[sel] + 1, he[sel] - 1, lv[sel], b[sel])
                got = hc >= 0
                if got.any():
                    g = sel[got]
                    res[g] = self._scan(hc[got] * BINS_PER_HOUR, hc[got] * BINS_PER_HOUR + BINS_PER_HOUR - 1, lv[g], b[g])
            # The last hour's part.
            sel = np.flatnonzero(live & (res < 0) & (he > hs))
            if len(sel):
                res[sel] = self._scan(he[sel] * BINS_PER_HOUR, e[sel], lv[sel], b[sel])
            out[a : a + batch] = res
        return out
