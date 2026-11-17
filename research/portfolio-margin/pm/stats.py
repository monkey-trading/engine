"""Small statistics shared by the stages."""

import numpy as np


def wquantile(x: np.ndarray, w: np.ndarray, q: float) -> float:
    """The weighted empirical quantile: the smallest value whose cumulative weight reaches `q`."""
    order = np.argsort(x)
    x, w = x[order], w[order]
    c = np.cumsum(w)
    return float(x[min(np.searchsorted(c, q * c[-1]), len(x) - 1)])


def quantile(x: np.ndarray, q: float) -> float:
    """The empirical quantile, linear between order statistics."""
    return float(np.quantile(x, q))


def winsor(x: np.ndarray, p: float = 0.01) -> np.ndarray:
    lo, hi = np.quantile(x, [p, 1 - p], axis=0)
    return np.clip(x, lo, hi)


def corr(a: np.ndarray, b: np.ndarray) -> float:
    if len(a) < 3 or a.std() == 0 or b.std() == 0:
        return float("nan")
    return float(np.corrcoef(a, b)[0, 1])


def coexceed(a: np.ndarray, b: np.ndarray, p: float = 0.05) -> float:
    """P(b beyond its own p-tail | a beyond its p-tail), same side, averaged over the two sides:
    `p` if independent."""
    out = []
    for sign in (1, -1):
        x, y = sign * a, sign * b
        hit = x >= np.quantile(x, 1 - p)
        if hit.sum() == 0:
            return float("nan")
        out.append(float((y[hit] >= np.quantile(y, 1 - p)).mean()))
    return float(np.mean(out))
