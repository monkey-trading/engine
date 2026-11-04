"""The sampled hours' price paths, and the moves the attack models read from them.

A path is one sampled hour's log price on a 50 ms grid (`derive/grids.py`), from three minutes
before the hour to two after it; the models measure over the hour itself, `ANALYSIS`, and use the
margins for look-back and look-ahead windows. The index proxy is Coinbase's last trade: the engine
prices BTC-USD, ETH-USD and SOL-USD from the midpoint of Coinbase's and Kraken's book mids, and
`derive/calibrate.py` compares the two on the live recording.
"""

import json
from dataclasses import dataclass
from pathlib import Path

import numpy as np

DATA = Path(__file__).resolve().parents[1] / "data" / "derived"


@dataclass
class Hour:
    market: str
    hour: int
    stratum: object  # an int stratum, or "stress"
    x: np.ndarray  # log index proxy on the grid
    kraken: np.ndarray  # log last Kraken trade on the grid
    step_s: float
    lead: int  # grid steps before the hour

    @property
    def analysis(self) -> slice:
        return slice(self.lead, self.lead + int(round(3600 / self.step_s)))

    def steps(self, seconds: float) -> int:
        return int(round(seconds / self.step_s))


def load_hours(data: Path = DATA) -> list[Hour]:
    z = np.load(data / "grids.npz")
    meta = json.loads((data / "grids.json").read_text())
    step_s = float(z["step_ms"]) / 1000
    lead = int(round(float(z["lead_s"]) / step_s))
    cb = np.log(np.cumsum(z["coinbase"].astype(np.int64), axis=1) / 100.0)
    kr = np.log(np.cumsum(z["kraken"].astype(np.int64), axis=1) / 100.0)
    rows = [m for m in meta if m["venue"] == "coinbase"]
    return [Hour(m["market"], m["hour"], m["stratum"], cb[i], kr[i], step_s, lead) for i, m in enumerate(rows)]


def population(sample_path: Path = DATA / "sample.json") -> dict[str, tuple[dict[int, int], int]]:
    """Each market's hours per stratum in the year, and the year's hours the candles cover."""
    sample = json.loads(sample_path.read_text())
    return {m: ({k: s["hours"] for k, s in enumerate(v["strata"])}, v["year_hours"]) for m, v in sample["markets"].items()}


def window_max(x: np.ndarray, width: int) -> np.ndarray:
    """`out[i] = max(x[i : i + width + 1])` for every `i` the window fits, in linear time: each
    window spans at most two blocks of `width + 1`, so it is the larger of a block's suffix
    maximum and the next block's prefix maximum (van Herk, Gil and Werman)."""
    x = np.asarray(x, dtype=float)
    w = width + 1
    n = len(x)
    if w <= 1:
        return x.copy()
    blocks = -(-n // w)
    pad = np.full(blocks * w, -np.inf)
    pad[:n] = x
    b = pad.reshape(blocks, w)
    prefix = np.maximum.accumulate(b, axis=1).ravel()
    suffix = np.maximum.accumulate(b[:, ::-1], axis=1)[:, ::-1].ravel()
    m = n - width
    return np.maximum(suffix[:m], prefix[width : width + m])


def window_min(x: np.ndarray, width: int) -> np.ndarray:
    return -window_max(-np.asarray(x, dtype=float), width)


def resample(x: np.ndarray, every: int) -> np.ndarray:
    return x[::every]


def divergence_clamped(x: np.ndarray, other: np.ndarray, bound_bps: float) -> np.ndarray:
    """`x` held at its last value wherever it sits more than `bound_bps` from `other`, in bps of
    `other`: what the engine's index does when two sources diverge past the divergence bound,
    which leaves the market with no price until they agree again. `other` is Kraken's last trade,
    which lags Kraken's book, so this also holds `x` through some genuine fast moves."""
    gap = np.abs(np.expm1(x - other)) * 1e4
    priced = gap <= bound_bps
    if priced.all():
        return x
    idx = np.where(priced, np.arange(len(x)), 0)
    np.maximum.accumulate(idx, out=idx)
    held = x[idx]
    # Before the first priced step there is nothing to hold; keep the path there.
    first = int(np.argmax(priced)) if priced.any() else len(x)
    held[:first] = x[:first]
    return held
