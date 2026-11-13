"""Honest client flow, position by position, and what each close realises against the house.

The cap study (#111, #134) carries a live house-counterparty venue's flow over by margin and
settles it hour by hour in cohorts. A loss-rate trigger reads the house's realised loss over one to
sixty minutes, which hourly cohorts cannot give, so this module draws the same flow as single
positions on a 10 s path (`paths.py`):

- each flow class (kind, hold class, leverage bin) brings its share of the market's margin, at the
  class's leverage on the market's margin bands, as positions arriving at random over the market's
  open time;
- a position's notional comes from the kind's size profile on the market's bands, counted by
  position rather than by margin; its hold from a lognormal with the class's measured mean hold and
  mean square-root hold; its side from the class's long share plus the market's hourly AR(1)
  deviation, which leans on the last hour's return, as in the cap study;
- hedged flow opens pairs: two accounts, opposite sides, the same notional and hold, ending
  together when either leg is closed out;
- every fill pays the market's size spread through its position-notional bands, on the opening and
  the closing slice (`V52`); a position is closed out once its equity falls below half its margin,
  and loses at most its balance; a winning close pays the profit fee at its hold.

Each close's realised loss to the house is the client's realised PnL at the spread-adjusted fill
prices less the fee it pays: positive when the house pays a winner, negative when it collects from
a loser. `net` sums them; `gross` sums only the positive ones, the house's payments to winners.
"""

from dataclasses import dataclass

import numpy as np

from trigger.paths import BIN_S, BINS_PER_HOUR, Crossings, Path, next_open

CLOSEOUT_SHARE = 0.5  # `closeout_level_bps` 5000: closed out below half the margin


@dataclass(frozen=True)
class FlowClass:
    kind: str  # "directional" or "hedged"
    notional_per_s: float  # USD of notional opened per open second, both legs of a pair counted
    mu: float  # log-seconds: the lognormal hold's location
    sigma: float
    min_hold_s: float
    max_hold_s: float
    long_share: float
    leverage: float  # notional per unit of margin at this class's leverage on the market's bands


@dataclass(frozen=True)
class Sizes:
    """A kind's position notionals, by position: log-uniform within each bin."""

    lo: np.ndarray
    hi: np.ndarray
    prob: np.ndarray

    @property
    def mean(self) -> float:
        return float((self.prob * (self.hi - self.lo) / np.log(self.hi / self.lo)).sum())

    def draw(self, rng: np.random.Generator, n: int) -> np.ndarray:
        i = rng.choice(len(self.prob), size=n, p=self.prob)
        return np.exp(rng.uniform(np.log(self.lo[i]), np.log(self.hi[i])))


@dataclass(frozen=True)
class MarketFlow:
    symbol: str
    margin_tiers: list  # the genesis's margin bands: [{"up_to": USD, "bps": ...}, ..., {"bps": ...}]
    spread_tiers: list  # [(up_to USD or inf, rate as a fraction), ...]
    classes: tuple
    sizes: dict  # kind -> Sizes


def lognormal_hold(mean_h: float, mean_sqrt_h: float) -> tuple[float, float]:
    """(mu, sigma) in log-seconds of the lognormal whose mean and mean square root are the class's."""
    m1, mh = np.log(mean_h * 3600.0), np.log(mean_sqrt_h * 60.0)  # sqrt(3600) = 60
    s2 = max(4.0 * (m1 - 2.0 * mh), 0.0)
    return float(m1 - s2 / 2.0), float(np.sqrt(s2))


def staircase(notional, tiers: list) -> np.ndarray:
    """The size spread a fill of `notional` pays through position-notional bands from zero, as a
    share of the notional: each slice at its band's rate (`spread_staircase`)."""
    notional = np.asarray(notional, dtype=float)
    cost = np.zeros_like(notional)
    lo = 0.0
    for up_to, rate in tiers:
        cost += np.clip(notional - lo, 0.0, up_to - lo) * rate
        lo = up_to
    return cost / np.maximum(notional, 1e-12)


def margin_for(notional, margin_tiers: list) -> np.ndarray:
    """The margin `notional` needs under the bands, each slice at its band's rate."""
    tiers = [(float(t.get("up_to", np.inf)), t["bps"] / 1e4) for t in margin_tiers]
    return staircase(notional, tiers) * np.asarray(notional, dtype=float)


def count_sizes(bounds: np.ndarray, shares: np.ndarray, margin_tiers: list) -> Sizes:
    """A size profile by position from the cap study's margin-weighted one: each bin's margin share
    over the margin its typical position needs."""
    lo, hi = bounds[:-1], bounds[1:]
    margin_share = np.diff(shares)
    keep = (margin_share > 0) & (hi > lo)
    lo, hi, margin_share = lo[keep], hi[keep], margin_share[keep]
    weight = margin_share / margin_for(np.sqrt(lo * hi), margin_tiers)
    return Sizes(lo=lo, hi=hi, prob=weight / weight.sum())


@dataclass
class Closes:
    """Realised loss to the house per bin."""

    net: np.ndarray
    gross: np.ndarray
    exposure: np.ndarray  # the change in the house's open notional per bin, at entry prices
    qty: np.ndarray  # the change in the house's open quantity per bin, in units of the path's price
    bad: np.ndarray  # bad debt per bin: losses past the account's balance, which the house bears

    @classmethod
    def empty(cls, bins: int) -> "Closes":
        return cls(*(np.zeros(bins) for _ in range(5)))

    def unrealised(self, x: np.ndarray) -> np.ndarray:
        """The house's unrealised PnL on open client positions at each bin's price."""
        return np.cumsum(self.qty) * np.exp(x) - np.cumsum(self.exposure)

    def marked(self, x: np.ndarray) -> np.ndarray:
        """The house's loss per bin marked to market: realised loss less the change in unrealised PnL."""
        return self.net - np.diff(self.unrealised(x), prepend=0.0)


def settle(path: Path, cross: Crossings, opened: np.ndarray, hold_bins: np.ndarray, notional: np.ndarray, side: np.ndarray,
           mf: MarketFlow, leverage: np.ndarray, fee_fn, opens_next: np.ndarray):
    """Directional positions that close on the path: (open bin, close bin, client quantity in units of the
    path's price, client notional signed by side, the house's realised loss at the close, bad debt)."""
    n = path.bins
    planned = opens_next[np.minimum(opened + hold_bins, n - 1)]
    on_path = (opened + hold_bins < n) & (planned < n)
    opened, planned, notional, side, leverage = opened[on_path], planned[on_path], notional[on_path], side[on_path], leverage[on_path]
    spread = staircase(notional, mf.spread_tiers)
    balance = np.maximum(margin_for(notional, mf.margin_tiers), notional / leverage)
    adverse = np.maximum(CLOSEOUT_SHARE * balance / notional - spread, 1e-6)
    x0 = path.x[opened]
    long = side > 0
    level = np.where(long, x0 + np.log1p(-np.minimum(adverse, 0.999)), x0 + np.log1p(adverse))
    hit = cross.first(opened, planned, level, long)
    closed = np.where(hit >= 0, hit, planned)
    raw = side * notional * np.expm1(path.x[closed] - x0) - 2 * spread * notional
    pnl = np.maximum(raw, -balance)
    fee = fee_fn((closed - opened) * BIN_S) * np.maximum(pnl, 0.0)
    return opened, closed, notional * side * np.exp(-x0), notional * side, pnl - fee, pnl - raw


def settle_pairs(path: Path, cross: Crossings, opened: np.ndarray, hold_bins: np.ndarray, notional: np.ndarray, mf: MarketFlow,
                 leverage: np.ndarray, fee_fn, opens_next: np.ndarray):
    """Hedged pairs that close on the path: (close bin, the house's loss on the long leg and on the short
    leg, bad debt)."""
    n = path.bins
    planned = opens_next[np.minimum(opened + hold_bins, n - 1)]
    on_path = (opened + hold_bins < n) & (planned < n)
    opened, planned, notional, leverage = opened[on_path], planned[on_path], notional[on_path], leverage[on_path]
    spread = staircase(notional, mf.spread_tiers)
    balance = np.maximum(margin_for(notional, mf.margin_tiers), notional / leverage)
    adverse = np.maximum(CLOSEOUT_SHARE * balance / notional - spread, 1e-6)
    x0 = path.x[opened]
    big = np.iinfo(np.int64).max
    hit_long = cross.first(opened, planned, x0 + np.log1p(-np.minimum(adverse, 0.999)), np.ones(len(opened), dtype=bool))
    hit_short = cross.first(opened, planned, x0 + np.log1p(adverse), np.zeros(len(opened), dtype=bool))
    first = np.minimum(np.where(hit_long >= 0, hit_long, big), np.where(hit_short >= 0, hit_short, big))
    closed = np.where(first < big, first, planned)
    move = np.expm1(path.x[closed] - x0)
    fee_share = fee_fn((closed - opened) * BIN_S)
    legs, bad = [], 0.0
    for side in (1.0, -1.0):
        raw = side * notional * move - 2 * spread * notional
        pnl = np.maximum(raw, -balance)
        legs.append(pnl - fee_share * np.maximum(pnl, 0.0))
        bad = bad + pnl - raw
    return closed, legs[0], legs[1], bad


def deviation(returns: np.ndarray, ar1: float, slope: float, resid_sd: float, rng: np.random.Generator) -> np.ndarray:
    """The market's hourly long-share deviation: AR(1), leaning on the last hour's log return."""
    dev = np.zeros(len(returns))
    eps = rng.standard_normal(len(returns)) * resid_sd
    for t in range(1, len(returns)):
        dev[t] = ar1 * dev[t - 1] + slope * returns[t - 1] + eps[t]
    return dev


def simulate(path: Path, mf: MarketFlow, dev: np.ndarray, fee_fn, rng: np.random.Generator, chunk_hours: int = 168) -> Closes:
    """Every honest position opened on `path`, settled; per-bin realised loss to the house."""
    n = path.bins
    out = Closes.empty(n)
    cross = Crossings(path)
    opens_next = next_open(path.is_open)
    for c0 in range(0, n, chunk_hours * BINS_PER_HOUR):
        c1 = min(n, c0 + chunk_hours * BINS_PER_HOUR)
        open_bins = np.flatnonzero(path.is_open[c0:c1]) + c0
        if len(open_bins) == 0:
            continue
        for cls in mf.classes:
            sizes = mf.sizes[cls.kind]
            legs = 2 if cls.kind == "hedged" else 1
            rate = cls.notional_per_s * BIN_S / (legs * sizes.mean)
            counts = rng.poisson(rate, size=len(open_bins))
            opened = np.repeat(open_bins, counts)
            if len(opened) == 0:
                continue
            notional = sizes.draw(rng, len(opened))
            hold = np.clip(np.exp(rng.normal(cls.mu, cls.sigma, len(opened))), cls.min_hold_s, cls.max_hold_s)
            hold_bins = np.maximum(1, np.rint(hold / BIN_S).astype(np.int64))
            leverage = np.full(len(opened), cls.leverage)
            if cls.kind == "hedged":
                closed, long_leg, short_leg, bad = settle_pairs(path, cross, opened, hold_bins, notional, mf, leverage, fee_fn, opens_next)
                out.net += np.bincount(closed, weights=long_leg + short_leg, minlength=n)
                out.bad += np.bincount(closed, weights=bad, minlength=n)
                out.gross += np.bincount(closed, weights=np.maximum(long_leg, 0) + np.maximum(short_leg, 0), minlength=n)
            else:
                share = np.clip(cls.long_share + dev[opened // BINS_PER_HOUR], 0.0, 1.0)
                side = np.where(rng.random(len(opened)) < share, 1.0, -1.0)
                start, closed, units, client, loss, bad = settle(path, cross, opened, hold_bins, notional, side, mf, leverage, fee_fn, opens_next)
                out.net += np.bincount(closed, weights=loss, minlength=n)
                out.bad += np.bincount(closed, weights=bad, minlength=n)
                out.gross += np.bincount(closed, weights=np.maximum(loss, 0.0), minlength=n)
                # The house holds the other side of each client position while it is open.
                out.exposure += np.bincount(start, weights=-client, minlength=n) - np.bincount(closed, weights=-client, minlength=n)
                out.qty += np.bincount(start, weights=-units, minlength=n) - np.bincount(closed, weights=-units, minlength=n)
    return out
