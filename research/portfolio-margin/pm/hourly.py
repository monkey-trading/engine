"""The cap study's hourly bars for the launch markets, as the returns this study reads.

`hourly_bars.npz` (`research/cap-calibration`): per UTC hour of 102 weeks and per market, the gap, high,
low and close of the hour as log returns in millionths, `CLOSED` for a closed hour. Here:

- `hour_ret[t, m]`: the hour's return, open to close, plus its gap when the previous hour was open
  as well (the cleaned bars leave such gaps at zero but for the daily break's), NaN when closed. A
  hour that follows a closure carries its in-bar move only; the closure's gap is read apart.
- `closure_gaps(m)`: each closure of a market as (kind, last open hour, reopening hour, gap), the
  gap being the log return from the last close to the reopening's open. The kinds are
  `caps.gaps.KINDS`: a daily break (under 3 hours), a closure (3 to 40) and a weekend (over 40).
"""

import sys

import numpy as np

from pm.common import CAP, LAUNCH, UNIT, launch_symbols

for p in (CAP, LAUNCH):
    if str(p) not in sys.path:
        sys.path.append(str(p))

CLOSED = np.iinfo(np.int32).min


class Hourly:
    def __init__(self):
        from model.prices import load_bars

        bars, symbols = load_bars(CAP / "data" / "derived" / "hourly_bars.npz")
        keep = [symbols.index(s) for s in launch_symbols()]
        self.bars = bars[:, keep]
        self.symbols = [symbols[i] for i in keep]
        self.index = {s: i for i, s in enumerate(self.symbols)}
        self.is_open = self.bars[..., 0] != CLOSED
        gap = np.where(self.is_open, self.bars[..., 0], 0).astype(float) / UNIT
        close = np.where(self.is_open, self.bars[..., 3], 0).astype(float) / UNIT
        prev_open = np.vstack([np.zeros((1, len(self.symbols)), bool), self.is_open[:-1]])
        r = close + np.where(prev_open, gap, 0.0)
        self.hour_ret = np.where(self.is_open, r, np.nan)
        self.hours = self.bars.shape[0]

    def closure_gaps(self, symbol: str) -> list[tuple[str, int, int, float]]:
        from caps.gaps import KINDS, closures, log_levels

        m = self.index[symbol]
        log_open, log_close = log_levels(self.bars, m)
        out = []
        for last, reopen in closures(self.is_open[:, m]):
            length = reopen - last - 1
            kind = next(k for k, lo, hi in KINDS if lo <= length < hi)
            out.append((kind, last, reopen, float(log_open[reopen] - log_close[last])))
        return out

    def joint_hours(self, symbols: list[str]) -> tuple[np.ndarray, np.ndarray]:
        """(hour indices, returns) of the hours every one of `symbols` has a return."""
        cols = [self.index[s] for s in symbols]
        r = self.hour_ret[:, cols]
        ok = np.isfinite(r).all(axis=1)
        return np.flatnonzero(ok), r[ok]
