"""The profit fee a class of short holds pays in expectation, with the spread inside the gain.

The engine charges the profit fee on each close's realised PnL at the spread-adjusted fill prices
(`execution/mod.rs`, `book`), at the fee its hold sets (#614, decision D7). For a zero-edge position
held `h` hours in a market whose hour has standard deviation `sigma`, the client's realised PnL per
unit of notional is `sigma * sqrt(h) * Z - 2 s`, `s` being the spread each fill pays, so its
expected fee is

    fee(h) * sigma * sqrt(h) * psi(2 s / (sigma * sqrt(h))),    psi(u) = E[max(Z - u, 0)].

Over a class's holds, weighted by notional, that is `sigma * T(2 s / sigma)` with
`T(v) = E[fee(h) * sqrt(h) * psi(v / sqrt(h))]`. With no spread and a flat fee it is the flat fee
on `sigma * E[sqrt(h)] / sqrt(2 pi)`, the expected gain the model charged before (`GAIN_PER_SD`).

The derived flow records two moments of each class's holds, the notional-weighted mean hold and
mean square root of the hold, not the distribution, so `T` cannot be integrated exactly. Two
tables stand in for it:

- `bound`: the least `T(v)` over every distribution of holds in the class's range that has both
  moments. It is a linear programme with three equality constraints, so an optimum sits on at most
  three holds, and enumerating every three-hold support on a fine grid finds it. It is a lower
  bound on the class's fee whatever its holds are, which is the conservative side for house
  income.
- `central`: the distribution closest in relative entropy to the measured within-hour holding
  profile (`papertrade_holding.csv`, holds log-uniform inside each bucket) that has both moments:
  an estimate, not a bound.
"""

import math
from itertools import combinations

import numpy as np

HOUR_S = 3600.0
GRID_POINTS = 160  # holds on the bound's grid, log-spaced from `GRID_LOW_S` to an hour
GRID_LOW_S = 0.01
PROFILE_POINTS = 40  # holds per profile bucket in `central`
PROFILE_LOW_S = 0.5  # the profile's first bucket starts at zero; its holds start here, as `calib/fee.py` takes them
# The tables' argument, v = 2 s / sigma in sqrt-hour units: zero, then log-spaced. A fill's spread is
# 0.1 to 12 bps and an open hour's sigma 2 to 500 bps, so v is 4e-4 to 12 in any market the study
# prices; linear interpolation in log v between these points is within 0.5% of the integral.
V = np.concatenate([[0.0], np.exp(np.linspace(np.log(1e-5), np.log(1e2), 600))])
V_CHUNK = 50  # values of v at a time in the bound's minimum, to bound memory

_erf = np.vectorize(math.erf, otypes=[float])


def psi(u):
    """E[max(Z - u, 0)] for a standard normal Z."""
    u = np.asarray(u, dtype=float)
    pdf = np.exp(-0.5 * u * u) / math.sqrt(2 * math.pi)
    tail = 0.5 * (1.0 - _erf(u / math.sqrt(2.0)))
    return pdf - u * tail


def weights(fee, hold_s: np.ndarray, v: np.ndarray = V) -> np.ndarray:
    """`fee(h) * sqrt(h) * psi(v / sqrt(h))` for each v (rows) and hold (columns), h in hours."""
    h = np.asarray(hold_s, dtype=float) / HOUR_S
    root = np.sqrt(h)
    share = fee.bps(hold_s) / 1e4
    v = np.asarray(v, dtype=float)[:, None]
    with np.errstate(divide="ignore", invalid="ignore"):
        w = share * root * psi(np.where(root > 0, v / np.where(root > 0, root, 1.0), np.inf))
    return np.where(root > 0, w, 0.0)


def hold_grid(min_hold_s: float = 0.0, max_hold_s: float = HOUR_S, points: int = GRID_POINTS) -> np.ndarray:
    """The bound's holds, in seconds: zero (or `min_hold_s`) and a log-spaced grid to `max_hold_s`."""
    lo = max(min_hold_s, GRID_LOW_S)
    grid = np.exp(np.linspace(np.log(lo), np.log(max_hold_s), points))
    return grid if min_hold_s > 0 else np.concatenate([[0.0], grid])


def bound_supports(hold_s: np.ndarray, mean_hold_h: float, mean_sqrt_hold_h: float) -> tuple[np.ndarray, np.ndarray]:
    """Every distribution on at most three of `hold_s` with total 1, mean hold `mean_hold_h` and
    mean square-root hold `mean_sqrt_hold_h`: the linear programme's basic feasible solutions, as
    (index triples, probabilities)."""
    h = np.asarray(hold_s, dtype=float) / HOUR_S
    r = np.sqrt(h)
    idx = np.array(list(combinations(range(len(h)), 3)), dtype=np.int64)
    a = np.stack([np.ones(idx.shape), h[idx], r[idx]], axis=1)  # (triples, 3 rows, 3 columns)
    b = np.array([1.0, mean_hold_h, mean_sqrt_hold_h])
    det = np.linalg.det(a)
    ok = np.abs(det) > 1e-14
    p = np.full(idx.shape, -1.0)
    p[ok] = np.linalg.solve(a[ok], np.broadcast_to(b, (ok.sum(), 3))[..., None])[..., 0]
    feasible = ok & (p >= -1e-12).all(axis=1)
    if not feasible.any():
        raise ValueError(f"no distribution on the grid has mean {mean_hold_h} h and mean root {mean_sqrt_hold_h}")
    return idx[feasible], np.clip(p[feasible], 0.0, None)


def single_hold(mean_hold_h: float, mean_sqrt_hold_h: float) -> bool:
    """Whether the moments allow one distribution only, every hold at the mean: the square of the
    mean root reaches the mean, which is Jensen's bound."""
    return mean_sqrt_hold_h**2 >= mean_hold_h * (1 - 1e-9)


def bound_table(fee, mean_hold_h: float, mean_sqrt_hold_h: float, min_hold_s: float = 0.0) -> np.ndarray:
    """The least `T(v)` at each of `V` over the distributions `bound_supports` finds."""
    if single_hold(mean_hold_h, mean_sqrt_hold_h):
        return weights(fee, np.array([mean_hold_h * HOUR_S]))[:, 0]
    grid = hold_grid(min_hold_s)
    idx, p = bound_supports(grid, mean_hold_h, mean_sqrt_hold_h)
    w = weights(fee, grid)  # (v, holds)
    return np.concatenate([np.min((w[i : i + V_CHUNK, idx] * p[None]).sum(axis=2), axis=1) for i in range(0, len(V), V_CHUNK)])


def profile_holds(holding: tuple, min_hold_s: float = 0.0) -> tuple[np.ndarray, np.ndarray]:
    """The within-hour holding profile as holds (seconds) and notional shares, log-uniform inside
    each bucket, cut to `min_hold_s` and above."""
    holds, mass = [], []
    for lo, hi, share in holding:
        lo, hi = max(lo, PROFILE_LOW_S, min_hold_s), min(hi, HOUR_S)
        if hi <= lo or share <= 0:
            continue
        edges = np.exp(np.linspace(np.log(lo), np.log(hi), PROFILE_POINTS + 1))
        holds.append(np.sqrt(edges[:-1] * edges[1:]))
        mass.append(np.full(PROFILE_POINTS, share / PROFILE_POINTS))
    holds, mass = np.concatenate(holds), np.concatenate(mass)
    return holds, mass / mass.sum()


def tilt(holds_s: np.ndarray, prior: np.ndarray, mean_hold_h: float, mean_sqrt_hold_h: float) -> np.ndarray:
    """The distribution on `holds_s` closest to `prior` in relative entropy with both moments:
    `prior * exp(a h + b sqrt(h))` normalised, (a, b) by Newton's method on the convex dual."""
    x = np.stack([holds_s / HOUR_S / mean_hold_h, np.sqrt(holds_s / HOUR_S) / mean_sqrt_hold_h], axis=1)
    target = np.ones(2)
    theta = np.zeros(2)

    def dual(th):
        z = x @ th
        top = z.max()
        return top + np.log((prior * np.exp(z - top)).sum()) - th @ target

    for _ in range(200):
        z = x @ theta
        q = prior * np.exp(z - z.max())
        q /= q.sum()
        mean = q @ x
        grad = mean - target
        if np.abs(grad).max() < 1e-8:
            return q
        cov = (x - mean).T @ ((x - mean) * q[:, None])
        step = np.linalg.solve(cov + 1e-12 * np.eye(2), grad)
        size, now = 1.0, dual(theta)
        while dual(theta - size * step) > now - 1e-4 * size * grad @ step and size > 1e-8:
            size /= 2
        theta = theta - size * step
    raise ValueError(f"no tilt of the profile has mean {mean_hold_h} h and mean root {mean_sqrt_hold_h}")


def central_table(fee, holding: tuple, mean_hold_h: float, mean_sqrt_hold_h: float, min_hold_s: float = 0.0) -> np.ndarray:
    """`T(v)` at each of `V` under the profile tilted to the class's moments."""
    if single_hold(mean_hold_h, mean_sqrt_hold_h):
        return weights(fee, np.array([mean_hold_h * HOUR_S]))[:, 0]
    holds, prior = profile_holds(holding, min_hold_s)
    q = tilt(holds, prior, mean_hold_h, mean_sqrt_hold_h)
    return weights(fee, holds) @ q


def unit_fee(table: np.ndarray, sigma, two_spread):
    """The expected fee per unit of notional, `sigma * T(2 s / sigma)`, for arrays of the hour's
    standard deviation and of both fills' spread; zero in an hour with no variation."""
    sigma = np.asarray(sigma, dtype=float)
    live = sigma > 0
    v = np.where(live, np.asarray(two_spread, dtype=float) / np.where(live, sigma, 1.0), 1.0)
    near = table[0] + (table[1] - table[0]) * v / V[1]  # linear from v = 0 to the first log point
    far = np.interp(np.log(np.maximum(v, V[1])), np.log(V[1:]), table[1:], right=0.0)
    return np.where(live, sigma * np.where(v < V[1], near, far), 0.0)
