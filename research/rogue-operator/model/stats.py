"""The stratified estimator: a year's total from hours sampled by volatility stratum.

Each market's year of hours is split into strata by hourly volatility (`derive/sample.py`), and a
few hours are drawn from each. A quantity measured on each sampled hour, `y`, estimates the
year's total as `sum_k N_k * mean(y_k)`, where `N_k` is the stratum's count of hours: unbiased
whatever the strata, and much tighter than a simple random sample when `y` is concentrated in the
busiest hours. Its uncertainty is a stratified bootstrap: each replicate redraws each stratum's
sampled hours with replacement, and the 95% interval is the 2.5th and 97.5th percentiles of the
replicate totals. With ten or twelve hours a stratum the interval is approximate, and the top
stratum's spread dominates it.
"""

from dataclasses import dataclass

import numpy as np

HOURS_PER_YEAR = 8760
BOOTSTRAP = 10_000


@dataclass
class Estimate:
    """A year's total and its interval, and the same per day."""

    year: float
    year_lo: float
    year_hi: float

    @property
    def day(self) -> float:
        return self.year * 24 / HOURS_PER_YEAR

    @property
    def day_lo(self) -> float:
        return self.year_lo * 24 / HOURS_PER_YEAR

    @property
    def day_hi(self) -> float:
        return self.year_hi * 24 / HOURS_PER_YEAR

    def scaled(self, factor: float) -> "Estimate":
        return Estimate(self.year * factor, self.year_lo * factor, self.year_hi * factor)

    def __add__(self, other: "Estimate") -> "Estimate":
        # Only for totals over independent samples; `combine` is used where draws are shared.
        return Estimate(self.year + other.year, self.year_lo + other.year_lo, self.year_hi + other.year_hi)


def stratified_total(values: dict[int, np.ndarray], population: dict[int, int], year_hours: int) -> float:
    """The year's total of `values`, per hour sampled in each stratum, scaled from the
    `year_hours` the candles cover to a whole year."""
    total = sum(population[k] * float(np.mean(v)) for k, v in values.items())
    return total * HOURS_PER_YEAR / year_hours


def bootstrap_totals(values: dict[int, np.ndarray], population: dict[int, int], year_hours: int, rng, draws: int = BOOTSTRAP) -> np.ndarray:
    """Replicate totals: each stratum's sampled hours redrawn with replacement."""
    totals = np.zeros(draws)
    for k, v in values.items():
        v = np.asarray(v, dtype=float)
        idx = rng.integers(0, len(v), size=(draws, len(v)))
        totals += population[k] * v[idx].mean(axis=1)
    return totals * HOURS_PER_YEAR / year_hours


def estimate(values: dict[int, np.ndarray], population: dict[int, int], year_hours: int, seed: int = 0) -> Estimate:
    rng = np.random.default_rng(seed)
    point = stratified_total(values, population, year_hours)
    reps = bootstrap_totals(values, population, year_hours, rng)
    lo, hi = np.percentile(reps, [2.5, 97.5])
    return Estimate(point, float(lo), float(hi))


def ratio(parts: list[tuple[dict[int, np.ndarray], dict[int, np.ndarray], dict[int, int], int]], seed: int = 0) -> Estimate:
    """The ratio of two year totals, such as dollars skimmed over dollars of notional, summed over
    markets; each replicate redraws the same hours for the numerator and the denominator. The
    `year` fields hold the ratio itself, not a total."""
    rng = np.random.default_rng(seed)
    num = den = 0.0
    rnum = np.zeros(BOOTSTRAP)
    rden = np.zeros(BOOTSTRAP)
    for n_vals, d_vals, pop, year_hours in parts:
        num += stratified_total(n_vals, pop, year_hours)
        den += stratified_total(d_vals, pop, year_hours)
        for k in n_vals:
            nv, dv = np.asarray(n_vals[k], dtype=float), np.asarray(d_vals[k], dtype=float)
            idx = rng.integers(0, len(nv), size=(BOOTSTRAP, len(nv)))
            rnum += pop[k] * nv[idx].mean(axis=1) * HOURS_PER_YEAR / year_hours
            rden += pop[k] * dv[idx].mean(axis=1) * HOURS_PER_YEAR / year_hours
    reps = rnum / np.where(rden == 0, np.nan, rden)
    lo, hi = np.nanpercentile(reps, [2.5, 97.5])
    return Estimate(num / den if den else float("nan"), float(lo), float(hi))


def combine(parts: list[tuple[dict[int, np.ndarray], dict[int, int], int]], seed: int = 0) -> Estimate:
    """The sum over markets of their year totals, each market's strata bootstrapped on its own."""
    rng = np.random.default_rng(seed)
    point = sum(stratified_total(v, p, n) for v, p, n in parts)
    reps = sum(bootstrap_totals(v, p, n, rng) for v, p, n in parts)
    lo, hi = np.percentile(reps, [2.5, 97.5])
    return Estimate(point, float(lo), float(hi))
