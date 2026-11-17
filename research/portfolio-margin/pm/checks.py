"""Book generators and the tail-coverage measure (joint sampling), shared by the stages.

Joint sampling: a book's loss over the horizon is drawn from the hours every member was open, each
hour's return vector (both signs, so a long and a short book fare alike) scaled by the horizon's
ratio to the hour. Marginal quantiles at a level do not give a portfolio's quantile; this keeps the
members' joint distribution, tails included, as the hourly data has it.
"""

import numpy as np


def pair_book(members, beta, a: str, b: str, size: float, hedge: float) -> np.ndarray:
    """Long `size` of `a`, short `b` so that `hedge` of a's loading is offset."""
    n = np.zeros(len(members))
    n[members.index(a)] = size
    n[members.index(b)] = -hedge * beta[a] * size / beta[b]
    return n


def basket_book(members, beta, rng, size: float, hedge: float) -> np.ndarray:
    """A random long/short basket of the members with the net loading at `1 - hedge` of the longs'."""
    k = len(members)
    w = rng.lognormal(0, 0.5, k)
    sign = np.where(np.arange(k) % 2 == 0, 1.0, -1.0)[rng.permutation(k)]
    n = sign * w
    b = np.array([beta[m] for m in members])
    long_load, short_load = (b * np.maximum(n, 0)).sum(), -(b * np.minimum(n, 0)).sum()
    if long_load == 0 or short_load == 0:
        return n * size
    n = np.where(n < 0, n * (hedge * long_load / short_load), n)
    return n * size / np.abs(n).max()


def tail_excess(x: np.ndarray, n: np.ndarray, equity: float, cost: float) -> tuple[float, float]:
    """(rate, mean excess) at which the book's loss over the sampled moves `x` (rows: hours) and the
    closing cost exceed `equity`."""
    loss = -(x @ n) + cost
    ex = np.maximum(loss - equity, 0.0)
    return float((ex > 0).mean()), float(ex.mean())
