"""Benefits earned with volume: what a toxic account forfeits by moving to a fresh wallet.

The stick (`calib/toxicity.py`) widens a caught account's spread; a fresh wallet starts at the best
price, so moving to one sheds it. The carrot makes that move cost something: five benefits grow
with an account's counted volume over the last 30 days, and a fresh wallet starts without them.

**Counted volume** is the notional of fills that change the account's position, but for:
- a round trip (a fill opening and a fill closing the same quantity at the same index within the
  score's horizon), which counts nothing, as it scores nothing;
- the part of a position closed within `MIN_HOLD_S` of its open time (`calib/fee.py`'s clock),
  which counts nothing on either fill, so churn and wash trades held for seconds earn no tier.
In a house-counterparty venue every fill is against the house, so wash volume is an account
trading with itself over time, or two accounts holding opposite positions: each pays both spreads,
holds at least `MIN_HOLD_S`, and pays the holding-time fee on the side that wins
(`farming_cost_bps`). Those edgeless fills also add to the score's weight, which costs many times
the edge they hide (`toxicity.dilution_cost_share`).

**Tiers** (`TIERS`) and the benefit at each:
1. requests a second, from the fresh-account budget (#482) up to and past the production
   per-account limit;
2. the best-price size bound, as a share of the market's first spread tier's bound: fills above it
   pay the second tier;
3. maximum leverage, as a share of the market's: a fresh wallet gets a fifth, so a pair across an
   unpriced move needs five times the move it needs at full leverage (#549, `run.py attacks`);
4. a discount on the profit fee's 10% floor, and on the financing markup;
5. a boost on MONKEY emissions.

A tier also needs time: a day's counted volume counts at most a `DAYS`-th of a tier's threshold
toward it, so tier k takes counted volume on at least `DAYS[k]` days of the 30.

The thresholds: a wallet the stick catches has `toxicity.MIN_FILLS` fills of toxic life, under a
day for any strategy that trades often enough to be caught, and a day counts at most a
`DAYS[1]`-th of tier 1's threshold, so a recycled wallet stays at tier 0 whatever its fill size.
Tier 1's $1M is a few days of an ordinary account's trading; above it, each threshold is set so
that farming it with edgeless round trips costs more than a month of the tier's best-price benefit
for an account trading at the threshold with fills at the tier's bound (`tests/test_carrot.py`).
"""

from dataclasses import dataclass

MIN_HOLD_S = 60.0
HALF_LIFE_RATE = 0.6931471805599453 / 7.0  # the score's decay a day (`toxicity.HALF_LIFE_DAYS`)
CAPITAL_COST_PER_YEAR = 0.10
WALLET_COST_USD = 10.0  # a deposit and a withdrawal on chain, and screening


@dataclass(frozen=True)
class Tier:
    volume: float  # counted 30-day volume, USD
    requests_per_s: float
    best_price_share: float  # of the market's first spread tier's bound
    leverage_share: float  # of the market's maximum leverage
    fee_floor: float  # the profit fee's floor, from 10%
    financing_discount: float  # of the financing markup
    emissions_boost: float


DAYS = (0, 2, 5, 10)
TIERS = (
    Tier(0.0, 2.0, 0.25, 0.20, 0.10, 0.0, 1.00),
    Tier(1e6, 5.0, 0.50, 0.50, 0.10, 0.0, 1.10),
    Tier(25e6, 10.0, 1.00, 1.00, 0.09, 0.10, 1.25),
    Tier(250e6, 20.0, 2.50, 1.00, 0.08, 0.20, 1.50),
)


def tier_for(volume: float, days: float = 30.0) -> int:
    """The tier `volume` of counted volume earns when it is spread over `days` days."""
    return max(i for i, t in enumerate(TIERS) if volume >= t.volume and days >= DAYS[i])


def toxic_life_volume(first_bound: float, min_fills: int) -> float:
    """The most counted volume a fresh wallet's toxic life earns at tier 0's best-price bound."""
    return min_fills * TIERS[0].best_price_share * first_bound


def farming_cost_bps(spread_bps: float, sigma_1s_bps: float, fee: float, hold_s: float = MIN_HOLD_S) -> float:
    """The expected cost, in bps of counted volume (both fills), of earning volume with edgeless
    round trips held `hold_s`: a spread on each fill, and the fee on the winning side's expected
    gain, `sigma * sqrt(hold) / sqrt(2 pi)`, against no refund on the losing side, shared over the
    round trip's two fills."""
    from math import pi, sqrt

    return spread_bps + fee * sigma_1s_bps * sqrt(hold_s) / sqrt(2 * pi) / 2


@dataclass(frozen=True)
class Trader:
    """A toxic account the stick has caught: its counted volume a month, its fill size and hold, its
    profit a month at holds where the fee is at its floor, its financing a month, and its gross
    realised losses a month (what emissions are shared by)."""

    name: str
    volume: float
    fill: float
    hold_s: float
    floor_profit: float = 0.0
    financing: float = 0.0
    losses: float = 0.0


def benefit_values(trader: Trader, tier: Tier, market: dict, emissions_per_loss: float) -> dict:
    """Each benefit's value a month at `tier` against tier 0, for `trader` in `market` (its first
    spread tier's bound and the bps of its first two tiers, and its first margin rate):
    - requests: none for these traders, whose fill rates sit far under tier 0's limit;
    - best price: the second tier's step on the part of each fill between the two bounds;
    - leverage: the capital cost of the extra margin tier 0's leverage needs on one position;
    - fee and financing: the floor's discount on its floor-fee profits, and the financing discount;
    - emissions: the boost on what its losses mint, valued at `emissions_per_loss` a dollar."""
    base = TIERS[0]
    bound, s1, s2, margin = market["bound"], market["s1_bps"], market["s2_bps"], market["margin"]
    above0 = max(0.0, trader.fill - base.best_price_share * bound)
    above = max(0.0, trader.fill - tier.best_price_share * bound)
    best_price = (s2 - s1) / 1e4 * (above0 - above) / trader.fill * trader.volume
    extra_margin = trader.fill * margin * (1 / base.leverage_share - 1 / tier.leverage_share)
    leverage = extra_margin * CAPITAL_COST_PER_YEAR / 12
    fee = (base.fee_floor - tier.fee_floor) * trader.floor_profit + tier.financing_discount * trader.financing
    emissions = (tier.emissions_boost - base.emissions_boost) * trader.losses * emissions_per_loss
    out = {"requests": 0.0, "best_price": best_price, "leverage": leverage, "fee_and_financing": fee,
           "emissions": emissions}
    out["total"] = sum(out.values())
    return out


def recycling(trader: Trader, add_on_bps: float, market: dict, min_fills: int, emissions_per_loss: float) -> dict:
    """A month of recycling against keeping the wallet. The keeper pays the stick's add-on on its
    volume and holds the tier its counted volume earns; the recycler moves to a fresh wallet every
    `min_fills` fills, so it pays no add-on, holds the tier one toxic life earns, and pays for each
    wallet. Gains the add-on; forfeits the difference in benefits and the wallets."""
    keep = tier_for(trader.volume)
    life = min(trader.volume, min_fills * trader.fill)
    recycled = tier_for(life, days=1)  # a toxic life lasts under a day
    gain = add_on_bps / 1e4 * trader.volume
    kept = benefit_values(trader, TIERS[keep], market, emissions_per_loss)
    held = benefit_values(trader, TIERS[recycled], market, emissions_per_loss)
    wallets = trader.volume / (min_fills * trader.fill)
    benefits = {k: kept[k] - held[k] for k in kept}
    forfeited = benefits["total"] + wallets * WALLET_COST_USD
    return {"trader": trader.name, "kept_tier": keep, "recycled_tier": recycled, "gain": gain,
            "forfeited": forfeited, "benefits": benefits, "wallets": wallets, "recycling_pays": gain > forfeited}


def best_responses(year: dict, schedule: dict, market: dict, add_on_bps: float, min_fills: int, cap: float, tier_fee=None) -> dict:
    """The latency strategy's best day for a trader that keeps its wallet against one that recycles,
    over holds, thresholds and fill sizes up to `cap`. `year` maps (hold, theta) to year sums
    (positive, negative, trades) per unit of size at the first tier's spread. A fill above the
    tier's best-price bound pays the second tier on the excess and a caught keeper pays the stick's
    add-on; both widen each trade's result by a cost `c`, which lowers the fee-adjusted result by at
    least `(1 - fee) * c` (its slope is `1 - fee` on gains and one on losses), so the takes here
    are upper bounds. The keeper's tier is the one its counted volume earns (holds of `MIN_HOLD_S`
    or more, every day); the recycler's is tier 0, and it pays for a wallet every `min_fills` fills.
    `tier_fee(hold, tier)`, when given, is the fee a tier pays at a hold (#614, decision D7), in place
    of `schedule`'s, which every tier pays alike."""
    from calib import fee

    def fee_for(hold, tier):
        return float(tier_fee(hold, tier)) if tier_fee else float(fee.fee_at(schedule, hold))

    bound, step = market["bound"], (market["s2_bps"] - market["s1_bps"]) / 1e4

    def take(pos, neg, n, f, size, best, extra):
        over = max(0.0, size - best)
        return (((1 - f) * pos - neg) * size - (1 - f) * n * (2 * step * over + 2 * extra * size)) / 365

    out = {}
    for who in ("keeper", "recycler"):
        best = {"per_day": 0.0}
        for (hold, theta), (pos, neg, n) in year.items():
            for size in sorted({TIERS[0].best_price_share * bound, bound, TIERS[-1].best_price_share * bound, cap}):
                if size > cap:
                    continue
                counted = 2 * size * n / 365 * 30 if hold >= MIN_HOLD_S else 0.0
                tier = tier_for(counted) if who == "keeper" else 0
                # The stick catches a keeper only once its decayed fill count reaches `min_fills`.
                caught = 2 * n / 365 / HALF_LIFE_RATE >= min_fills
                extra = add_on_bps / 1e4 if who == "keeper" and caught else 0.0
                wallets = 2 * n / 365 / min_fills if who == "recycler" else 0.0
                f = fee_for(hold, tier)
                value = take(pos, neg, n, f, size, TIERS[tier].best_price_share * bound, extra) - wallets * WALLET_COST_USD
                if value > best["per_day"]:
                    best = {"per_day": value, "hold_s": hold, "theta_bps": theta, "size": size, "tier": tier,
                            "trades_per_day": n / 365, "counted_per_month": counted}
        out[who] = best
    # What recycling gains and forfeits at the recycler's best: the add-on it no longer pays, against
    # the keeper's tier's best price, leverage and fee floor, and the wallets.
    r = out["recycler"]
    if r["per_day"] > 0:
        f = fee_for(r["hold_s"], 0)
        n = r["trades_per_day"]
        counted = 2 * r["size"] * n * 30 if r["hold_s"] >= MIN_HOLD_S else 0.0
        kept = TIERS[tier_for(counted)]
        gain = (1 - f) * n * 2 * add_on_bps / 1e4 * r["size"]
        over0 = max(0.0, r["size"] - TIERS[0].best_price_share * bound)
        over_k = max(0.0, r["size"] - kept.best_price_share * bound)
        best_price = (1 - f) * n * 2 * step * (over0 - over_k)
        leverage = r["size"] * market["margin"] * (1 / TIERS[0].leverage_share - 1 / kept.leverage_share) * CAPITAL_COST_PER_YEAR / 365
        wallets = 2 * n / min_fills * WALLET_COST_USD
        out["at_recycler_best"] = {"kept_tier": TIERS.index(kept), "gain_per_day": gain, "best_price_per_day": best_price,
                                   "leverage_per_day": leverage, "wallets_per_day": wallets,
                                   "forfeited_per_day": best_price + leverage + wallets}
    return out
