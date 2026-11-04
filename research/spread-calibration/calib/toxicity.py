"""The toxicity score as a stick only: measured over time, so neither round trips nor size dilute it.

Every account starts at the best price; a rising score only adds to its spread (#614). The score:
- each fill's markout `m` is its edge against the engine's index `HORIZON_S` later, signed so that
  a fill the index then moves toward scores positive, in units of the market's markout spread at
  that moment, `sigma` (the rms of the index's `HORIZON_S` changes over the trailing hour of
  sequenced marks, floored at the year's calibrated value): a z-unit per fill;
- a fill's weight is its notional, capped at `WEIGHT_CAP` times the account's decayed mean fill
  notional at the moment it fills, so one huge fill cannot add more noise than a few ordinary ones;
- a fill that opens and a fill that closes the same quantity at the same index within the horizon
  is a round trip: neither scores nor counts as volume (`calib/carrot.py`);
- the account's weighted edge `E` and weight `W2` (the sum of squared weights) decay with time,
  half-life `HALF_LIFE_DAYS`: `E <- E * decay + w * m`, `W2 <- W2 * decay + w^2`;
- the score is `E / sqrt(W2)`: how many standard errors the account's recent edge sits above zero.
  Tiers apply only once the account's decayed fill count reaches `MIN_FILLS`, so one lucky fill
  in the markouts' fat tail cannot reach a tier.

Edgeless fills still add to `W2` and so lower the score, but every fill now pays a spread, and the
cap stops a few huge fills doing the work: `dilution_cost_share` gives the steady-state cost of
holding a toxic account's score under a tier with edgeless fills as a share of the edge it keeps
earning, which is far above one at the calibrated spreads.

The calibration measures, on the sampled hours, random fills' markouts (an honest account's noise)
and the latency strategy's fills' markouts, both in z-units, and from them the fills a toxic account
takes to reach each tier and the chance an honest account's score ever reaches one in a window of
fills (`honest_reach`), from the measured fat-tailed markouts rather than a normal tail.
"""

import numpy as np

HORIZON_S = 5.0
HALF_LIFE_DAYS = 7.0
MIN_FILLS = 20
WEIGHT_CAP = 4.0
Z_TIERS = ((4.0, 2.0), (8.0, 5.0))  # (score, extra spread bps)


def hour_sigma(x: np.ndarray, step_s: float, start: int, length: int) -> float:
    """The rms of the index's `HORIZON_S` changes over `[start, start + length)`, in log terms."""
    k = int(round(HORIZON_S / step_s))
    d = x[start + k : start + length] - x[start : start + length - k]
    return float(np.sqrt(np.mean(d * d)))


def random_markouts(x: np.ndarray, step_s: float, start: int, length: int, rng, n: int = 2000) -> np.ndarray:
    """Markouts at `HORIZON_S` of `n` random fills in `[start, start + length)` with random sides:
    an honest account's noise, in log terms."""
    k = int(round(HORIZON_S / step_s))
    at = rng.integers(start, start + length - k, size=n)
    side = np.where(rng.random(n) < 0.5, 1.0, -1.0)
    return side * (x[at + k] - x[at])


def fills_to_tier(edge_z: float, z: float) -> float:
    """Equal fills an account whose mean markout is `edge_z` z-units takes to reach score `z`, with
    no decay in between (`z = edge_z * sqrt(n)`), and at least `MIN_FILLS`."""
    if edge_z <= 0:
        return float("inf")
    return float(max(MIN_FILLS, (z / edge_z) ** 2))


def honest_reach(markouts_z: np.ndarray, fills: int, rng, accounts: int = 20000, size_sd: float = 1.0) -> dict:
    """The share of edgeless accounts whose score reaches each tier at any fill among `fills`
    (from the `MIN_FILLS`-th), drawing their markouts from the measured `markouts_z` and their fill
    sizes from a lognormal with log-sd `size_sd`, weights capped as the score caps them. Decay is
    left out, which only makes the window longer than the half-life allows, so this is an upper
    bound for windows of more than a half-life's fills."""
    m = rng.choice(markouts_z, size=(accounts, fills))
    n = np.exp(size_sd * rng.standard_normal((accounts, fills)))
    mean = np.cumsum(n, axis=1) / np.arange(1, fills + 1)
    prior = np.concatenate([n[:, :1], mean[:, :-1]], axis=1)
    w = np.minimum(n, WEIGHT_CAP * prior)
    z = np.cumsum(w * m, axis=1) / np.sqrt(np.cumsum(w * w, axis=1))
    peak = z[:, MIN_FILLS - 1 :].max(axis=1)
    return {str(t): float((peak >= t).mean()) for t, _ in Z_TIERS}


def dilution_cost_share(edge_z: float, fills_per_day: float, spread_bps: float, sigma_bps: float, z: float,
                        cap: float = WEIGHT_CAP) -> dict:
    """The steady-state cost of holding an account's score at `z` with edgeless fills, as a share of
    the edge it earns, with the attack's fills as the unit of notional. With decay rate `lam`, the
    attack's decayed edge settles at `E = edge * f / lam` and its squared weight at `f / lam`; the
    score stays at `z` while `W2 >= (E / z)^2`, so edgeless fills must add `K = lam * (E / z)^2` of
    squared weight a day: `n` fills of size `r` with `n * r^2 = K`, each paying the spread on `r`.
    The cost `K / r` falls with `r`, which the weight cap bounds by `cap` times the mean fill,
    itself raised by the edgeless fills: `r * (f + n) <= cap * (f + n * r)`, so
    `r = cap / 2 + sqrt(cap^2 / 4 + (cap - 1) * K / f)`. Against `edge * sigma * f` earned a day,
    the share is `spread * K / (r * edge * sigma * f)`."""
    lam = np.log(2) / HALF_LIFE_DAYS
    k = lam * (edge_z * fills_per_day / lam / z) ** 2
    r = cap / 2 + np.sqrt(cap * cap / 4 + (cap - 1) * k / fills_per_day)
    share = spread_bps * k / (r * edge_z * sigma_bps * fills_per_day)
    return {"size_ratio": float(r), "fills_per_day": float(k / (r * r)), "share": float(share)}
