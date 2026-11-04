"""Client flow at launch: the #111 and #134 flow model's daily margin, market mix, leverage and sizes.

`research/cap-calibration` carries the on-chain behaviour of a live house-counterparty venue over
to the launch markets by margin: each day brings that venue's steady-state opened margin (the mean
of its last seven full days), each market takes its share of the #111 market mix, and each
position's notional is what its margin buys at the market's maximum leverage under its margin
bands. This module reads that study's committed `data/derived/` and gives, for the crypto markets
this study measures, the opening notional a day, the notional of all fills (each position opens
and closes once), and the count-weighted fill-size distribution. BTC-USD's and ETH-USD's bands (1%
to $2M, 2% to $10M, 5% above) are approximated by the 1%-to-$2M-then-5% profile, as #134's
`retained` stage does. Orders the exposure caps refuse are not removed: at the floors #111
recommends they are 1% to 2% of opening notional.
"""

import csv
from dataclasses import dataclass
from pathlib import Path

import numpy as np

CAP_STUDY = Path(__file__).resolve().parents[2] / "cap-calibration" / "data" / "derived"
SCHEDULES = {"BTC-USD": "100@2000000,500", "ETH-USD": "100@2000000,500", "SOL-USD": "1000@1000000,2000"}
# Each market's first margin rate and band edges for converting notional to margin.
BANDS = {
    "BTC-USD": [(2_000_000, 0.01), (10_000_000, 0.02), (float("inf"), 0.05)],
    "ETH-USD": [(2_000_000, 0.01), (10_000_000, 0.02), (float("inf"), 0.05)],
    "SOL-USD": [(1_000_000, 0.10), (float("inf"), 0.20)],
}


@dataclass
class MarketFlow:
    market: str
    opening_notional_per_day: float
    fill_notional_per_day: float
    fill_sizes: np.ndarray  # notional of each size bin's fills
    fill_weights: np.ndarray  # count share of each size bin

    @property
    def mean_fill(self) -> float:
        return float((self.fill_sizes * self.fill_weights).sum())

    @property
    def fills_per_second(self) -> float:
        return self.fill_notional_per_day / self.mean_fill / 86400

    def sample_sizes(self, rng, n: int) -> np.ndarray:
        return rng.choice(self.fill_sizes, size=n, p=self.fill_weights)


def margin_for(notional: np.ndarray, bands) -> np.ndarray:
    notional = np.asarray(notional, dtype=float)
    out = np.zeros_like(notional)
    lo = 0.0
    for hi, rate in bands:
        out += np.clip(notional - lo, 0, hi - lo) * rate
        lo = hi
    return out


def steady_daily_margin(derived: Path = CAP_STUDY) -> float:
    with (derived / "papertrade_daily.csv").open() as fh:
        full = [float(r["opened_margin_usd"]) for r in csv.DictReader(fh) if r["full_day"] == "True"]
    return float(np.mean(full[-7:]))


def opening_notional_by_class(scale: float = 1.0, derived: Path = CAP_STUDY) -> dict[str, float]:
    """Opening notional a day of the index, commodity and share markets, at each class's margin
    schedule in the cap study: the futures and US shares are what a Databento feed prices."""
    margin = steady_daily_margin(derived) * scale
    with (derived / "market_mix.csv").open() as fh:
        mix = {r["symbol"]: float(r["share"]) for r in csv.DictReader(fh)}
    with (derived / "papertrade_leverage.csv").open() as fh:
        lev_rows = list(csv.DictReader(fh))
    classes = {
        "indices": (["US500", "US100", "US30", "DE40", "UK100", "JP225"], "50@5000000,100@25000000,500"),
        "commodities": (["XAU-USD", "XAG-USD", "WTI", "BRENT"], "100@2000000,500"),
        "shares": ([s for s in mix if s not in SCHEDULES and "-" not in s and s not in
                    ("US500", "US100", "US30", "DE40", "UK100", "JP225", "WTI", "BRENT")], "1000@500000,2000@2000000,5000"),
    }
    out = {}
    for name, (symbols, schedule) in classes.items():
        sel = [r for r in lev_rows if r["mapping"] == "capped" and r["schedule"] == schedule]
        per_margin = sum(float(r["notional_usd"]) for r in sel) / sum(float(r["margin_usd"]) for r in sel)
        out[name] = margin * sum(mix[s] for s in symbols) * per_margin
    return out


def market_flows(scale: float = 1.0, derived: Path = CAP_STUDY) -> dict[str, MarketFlow]:
    margin = steady_daily_margin(derived) * scale
    with (derived / "market_mix.csv").open() as fh:
        mix = {r["symbol"]: float(r["share"]) for r in csv.DictReader(fh)}
    with (derived / "papertrade_flow.csv").open() as fh:
        rows = list(csv.DictReader(fh))
    kind_share = {k: sum(float(r["margin_usd"]) for r in rows if r["kind"] == k) for k in ("directional", "hedged")}
    total = sum(kind_share.values())
    with (derived / "papertrade_leverage.csv").open() as fh:
        lev_rows = list(csv.DictReader(fh))
    with (derived / "papertrade_sizes.csv").open() as fh:
        size_rows = list(csv.DictReader(fh))
    out = {}
    for market, schedule in SCHEDULES.items():
        per_margin = {}
        for kind in kind_share:
            sel = [r for r in lev_rows if r["kind"] == kind and r["mapping"] == "capped" and r["schedule"] == schedule]
            per_margin[kind] = sum(float(r["notional_usd"]) for r in sel) / sum(float(r["margin_usd"]) for r in sel)
        m = margin * mix[market]
        opening = sum(m * kind_share[k] / total * per_margin[k] for k in kind_share)
        # Count weights: each size bin's margin share over the margin one position of that size posts.
        bounds, weights = None, 0
        for kind in kind_share:
            sel = [r for r in size_rows if r["kind"] == kind and r["mapping"] == "capped" and r["schedule"] == schedule]
            b = np.array([float(r["notional_upto_usd"]) for r in sel])
            share = np.diff(np.array([float(r["margin_share"]) for r in sel]), prepend=0.0)
            mid = np.sqrt(b * np.concatenate([[b[0]], b[:-1]]))
            count = share * kind_share[kind] / total / np.maximum(margin_for(mid, BANDS[market]), 1e-9)
            bounds, weights = mid, weights + count
        keep = weights > 0
        out[market] = MarketFlow(market, opening, 2 * opening, bounds[keep], weights[keep] / weights[keep].sum())
    return out
