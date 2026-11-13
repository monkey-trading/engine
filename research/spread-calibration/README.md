# Spread, fee and toxicity calibration

## Summary

#549 measured what an operator can take within the rules the enclave enforces, and found the
defences did not hold at genesis values: a zero first spread tier left the price-lag bound without
effect, round trips at that tier diluted the toxicity score for free, and unpriced spells were
unbounded. The owner's decision #614 responds with four changes, and this study calibrates them on
#549's year of venue trades and the #111 and #134 client flow and house model:

1. **Spread tiers** for every launch market, from its own move over the price-lag window. No
   market quotes a zero spread: crypto's best tiers are 2 to 3 bps, FX and indices 1 bp, metals and
   oil 2 to 3 bps, shares 2 to 3 bps, each doubling and quadrupling with size.
2. **A profit fee that falls with holding time**: 100% of gains on holds up to 5 minutes, falling
   to the standard 10% at 30 minutes. The target client, who holds 15 minutes or more, pays 2.9%
   more of its gross profit; holds under 15 minutes keep about a tenth of their gains.
3. **A toxicity score that only worsens a price** and that neither round trips nor size can dilute:
   time-decayed markouts in units of the market's own markout spread, with each fill's weight
   capped. Diluting it under a tier costs 8 to 11 times the edge it hides.
4. **Benefits earned with volume**: request limits, larger best-price sizes, maximum leverage,
   fee and financing discounts, and an emissions boost, in four tiers of counted volume that a
   fresh wallet starts without.

With spread income the operator's honest share, the launch criterion's yardstick, rises from
$169k a day (#134) to $483k on target flow and $1.77M on the current flow. Against it, at the
proposed values, every attack class #549 measured, and all of them together, takes less a day on
either flow, with two exceptions to note:

- **Paired positions across unpriced moves** still take $19.1M over 2025-10-10's four stress hours
  at 1% margin. They stop paying, stress day included, at 2% margin: tier 1's leverage, or a cap on
  how long a market stays unpriced.
- **Altering the unauthenticated Databento quotes** ($360k a day at current flow) passes only
  because the share grew. No pricing parameter reaches it; authenticating the quotes does.

The retained NAV stays at $107M on both flows: the exposure caps' refusals set it, not house
cash. At 100x, BTC-USD and ETH-USD alone go from a 40% chance of NAV below zero at genesis pricing
to none with the spreads and fee. The tables are in `results/report.md`. The changes are
proposals: this study changes no genesis value.

## Owner decisions

The owner answered this study's proposals on #614 on 2026-11-12. Where an answer differs from a
proposal in this README, the answer stands:

- **Spreads, the score and the volume tiers** (proposals 1, 3 and 4): adopted, with the hold
  measured as below. Closing fills pay the size spread through the same bands as opening fills, and
  close-outs pay it too, without the toxicity add-on.
- **The holding-time fee** (proposal 2): 100% of gains on holds up to 1 s, falling to 10% at 15
  minutes, in place of the 5-minute proposal. In the frontier it costs target flow nothing extra,
  takes 33% of the gross profit on holds under 15 minutes, and leaves the operator's latency take at
  $276k a day and the fastest honest client's at $79k. A cap on how fast the house can lose bounds
  both.
- **BTC-USD and ETH-USD** (proposal 5): a 1% minimum margin, so tiers 2 and 3 reach 100x. Unpriced
  pairs, about $3k a day and $19.1M over 2025-10-10's four stress hours, are bounded by that cap and
  re-run at 1%.
- **Leverage by tier:** a margin floor of 5% at tier 0 and 2% at tier 1, none above, on the initial
  requirement only, in place of the table's shares of the maximum.
- **FX and index first tiers** (proposal 7): spread rates in tenths of a basis point.
- **Resting-order limits:** 25, 50, 100 and 250 at tiers 0 to 3, subject to their calibration.
- **Databento** (proposal 6): the engine now prices only from sources over TLS (#622), and #673
  settles second sources.

The figures in this study are before a correction to the house income model, which charges the
churn profit fee on the price gain at each cohort's mean hold and adds both fills' spread income
beside it. The engine charges the fee on realised PnL after the spread, so the model overstates
house income and the operator's honest share. #697 makes the correction and re-states the headline
at the owner's values.

## The question

#615 asks, for the owner:

- spread tiers for every launch market;
- a toxicity score that cannot be diluted, measured over time with same-index round trips
  excluded, and that only worsens pricing (every account, fresh wallets too, starts on the best
  price);
- a profit fee schedule that falls with holding time, how a hold is measured on adds, partial
  closes, flips and close-outs, and what it costs honest flow, where the target client holds 15 to
  30 minutes or longer;
- the volume benefits' thresholds and the benefit at each tier, how volume is counted, and what a
  toxic account forfeits by moving to a fresh wallet against what that gains it;
- #549's attack classes re-run against the operator's honest share;
- #134's retained NAV re-run with spread income, on the current flow and on target flow;
- #500's 100x question for BTC-USD and ETH-USD, with close-out gap bad debt and paired positions
  across unpriced moves.

## Data and models

The study adds no data. It reads:

- `research/rogue-operator`'s (#549) 50 ms price grids of Coinbase and Kraken trades over 156 hours
  sampled by volatility stratum from a year of BTC-USD, ETH-USD and SOL-USD, the four worst hours of
  2025-10-10, its stratified year estimates and its attack measures;
- `research/cap-calibration`'s (#111, #134) hourly bars for every launch market, the client flow
  carried over by margin from a live house-counterparty venue (its holding-time profile, leverage
  and sizes), and its hourly house simulation;
- `config/production-genesis.toml` for the launch markets, their size bounds and margins.

Both sibling studies name their code `model`, so a process uses one of them (`calib/common.py`);
this study's own code is `calib/`. The cap study's `Scenario` gains two fields for it, a spread per
market and a fee schedule, which leave its own results unchanged.

## Spread tiers

A best tier covers the mean move of the index over #510's 3 s price-lag bound in the market's
90th-percentile open hour, plus ordinary flow's measured edge against the house (0.18 bps of
notional), rounded up to the whole basis points the engine stores. The crypto markets' moves are
measured on the trade grids; the others are scaled from their hourly ranges by the ratio the crypto
grids give between a 3 s move and the hour's Parkinson range (`calib/volatility.py`). Larger sizes
pay twice and four times the best tier, at the genesis's size bounds: a larger order's adverse
selection grows with its size, and this study has no measurement of how fast. Every proposed best
tier sits within or below the published retail norm for its class (`results/report.md`); the
tiers are in `results/spread_tiers.toml`, in the genesis's format.

**FX and index first tiers.** FX majors need about 0.4 bps (a 0.2 bp move plus the 0.18 bp edge),
indices 0.7 to 1.0 bps: the FX tier's 1 bp is the engine's whole-basis-point unit, not the lag
window. At 1 bp, latency arbitrage on EUR-USD and GBP-USD measures zero, and on the other FX
markets and the indices $3k to $29k a day per market on an operator-sized position, an upper
bound. A tighter tier needs a finer spread unit, which is a new protocol version; what each
tighter tier would leave, and the owner's reference tiers (0.1 bp to $2M, 1 bp to $10M, 3 bps
above), are tabulated in the report for the owner to choose.

## The holding-time fee

**The schedule.** The fee is a share of a closed position's gain that depends on its hold: 100%
up to 5 minutes, falling linearly in the logarithm of the hold to 10% at 30 minutes, and 10% past
that. Losses pay nothing. The frontier in the report spans schedules reaching 10% at 15 to 60
minutes from peaks held to 1 s, 10 s, 1 minute and 5 minutes:

- The proposal costs target flow (holds of 15 minutes or more) 2.9% of its gross profit, and takes
  89% of the gross profit on holds under 15 minutes.
- Holding the full fee only to 10 s (the 10 s variant) costs target flow 1.0% and leaves the
  operator's latency take about the same ($117k a day against $115k), but leaves strategies held
  from seconds to a few minutes about half their gains. The owner's aim is that those strategies
  not pay, so the proposal is the 5-minute peak; the 10 s variant is the gentler choice if target
  flow's cost should be smaller.
- A flat 10% leaves the operator's own accounts $3.17M a day at the calibrated spreads.

**Measuring a hold** (`calib/fee.py`, `HoldClock`). The hold is the position's, not a lot's:
opening sets the open time; an add moves it to the size-weighted average of the old open time and
now, as the entry price is averaged; a partial close pays at the hold since the open time and
leaves it; a flip closes the position at its hold and opens the rest now; a close-out is a close,
whose gain, if any, pays at its hold. An aged position adjusted in slices keeps an old open time
only in proportion: slices of a tenth every 10 s pay at a hold of 100 s.

**The yardstick.** The honest cost is measured on the #111 profile's holds of 15 minutes or more,
the owner's target client; the profile's median hold, about a minute, is flow a zero fee pays for.

## The toxicity score

Each fill's markout is the index's move in its favour 5 s later, in units of the market's markout
spread over the trailing hour of sequenced marks (floored at half the year's). The score is the
account's time-decayed sum of weighted markouts over the square root of its decayed sum of squared
weights, half-life 7 days: how many standard errors its recent edge sits above zero. A fill's
weight is its notional, capped at four times the account's decayed mean fill. A same-index round
trip within the horizon scores nothing. No tier applies before 20 decayed fills; scores of 4 and 8
add 2 and 5 bps to the account's spread. On the sampled hours:

- 0.05% to 0.35% of edgeless accounts reach the first tier at some point in 1,000 fills, measured
  from the fat-tailed markouts themselves (kurtosis 9 to 17), not a normal tail; none reach the
  second.
- An account trading the price lag carries 1.6 to 2.0 standard errors of edge a fill and reaches
  the first tier at the 20-fill minimum.
- Holding such an account's score under a tier with edgeless fills costs 8 to 11 times the edge it
  hides, at the calibrated spreads and the weight cap.

The sample holds separate hours, so each hour's own markout spread stands in for its trailing hour.
Against the year's single spread instead, edgeless accounts reach the first tier at 0.40% to 0.55% in
1,000 fills, and a latency fill carries 2.1 to 3.2 standard errors (`results/toxicity.json`,
`fixed`).

## Volume tiers

| Counted volume, 30 days | Requests a second | Best-price size | Leverage | Fee floor | Financing discount | Emissions |
|---|---|---|---|---|---|---|
| Under $1M | 2 | a quarter of the first spread bound | a fifth of the maximum | 10% | - | 1x |
| $1M, over 2 days or more | 5 | half | half | 10% | - | 1.1x |
| $25M, over 5 days or more | 10 | the first bound | the maximum | 9% | 10% | 1.25x |
| $250M, over 10 days or more | 20 | 2.5 times the first bound | the maximum | 8% | 20% | 1.5x |

**Counting.** Volume counts the notional of fills that change a position, but not a same-index
round trip within the score's horizon, nor any part closed within 60 s of its open time, so churn
and wash trading held for seconds earn nothing. A day counts at most half, a fifth and a tenth of
tiers 1, 2 and 3's thresholds, so a tier takes trading on that many days; a wallet the score catches
within a day stays at tier 0 whatever its size. Above tier 1, farming a threshold with edgeless
round trips (a spread on each fill and the fee on the winning side) costs more than a month of the
tier's best-price benefit.

**What recycling gains and forfeits.** For the latency strategy at its best under the proposals,
recycling wallets avoids the score's add-on and forfeits the kept tier's best-price size, leverage
and wallets. The benefits forfeited are 35% to 70% of the add-on avoided, so recycling still pays
the strategy by $1k to $31k a day per market; what it pays is the take the spreads and fee leave
(next section), not a gain over the defences. The benefits forfeited leave out tiers 2 and 3's fee
and financing discounts, which a kept wallet's holds of 15 to 30 minutes would earn, so the figure
leans towards recycling.

## The attack classes against the launch criterion

Each of #549's classes at its genesis value and at the proposals, as the operator's take a day at
launch flow, with the parameter at which it stops paying (`results/report.md`, headline):

- **Latency arbitrage, crypto, the operator's 3 s view:** $8.41M a day with the toxicity add-on
  paid at genesis ($64.95M with the score laundered), $115k under the proposals. Against the
  target-flow share ($483k) any schedule in the frontier with a full fee at 1 s passes; against
  #134's pre-spread $169k it needs the full fee held to 10 s or more and 10% reached no sooner
  than 15 minutes. A flat 10% leaves $3.17M.
- **Latency arbitrage, FX and indices at 1 bp:** $87k a day over the ten markets, an upper bound;
  zero on EUR-USD and GBP-USD.
- **Skimming client fills:** $159k a day at current flow, unchanged by spreads or the fee; it
  scales with client fills, so about a quarter of it at target flow.
- **Censoring closes and the held sync's kill option:** no gain, and about $2k a day, as in #549.
- **Paired positions across unpriced moves:** about zero a day at 1% margin (an interval from
  -$133k to $197k), but $19.1M on 2025-10-10's four stress hours; negative at 2% margin and below,
  stress day included. The class stops paying at 2% margin, which is tier 1's leverage.
- **Altering the Databento quotes:** $360k a day at current flow, unchanged; it scales with
  client fills. It stops only with authenticated quotes or a second independent source.

Together, without the Databento quotes, the classes take about $366k a day at current flow
against a $1.77M share, and about $250k at target flow (skimming falling with the flow) against
$483k; with the quotes, about $725k and $340k. The honest fastest client, at an honest node's
0.25 s lead, still takes about $60k a day over the crypto markets, mostly on SOL-USD and ETH-USD
positions held 15 to 30 minutes after sub-second moves of 12 bps or more; that is a cost to the
house rather than an operator's take, and the frontier gives it beside each schedule.

## Retained NAV and house income

#134's retained NAV (1,000 paths, 90 days, 37 markets, the genesis margins with 100x on BTC-USD
and ETH-USD), on the current flow and on target flow, at genesis pricing and with the calibrated
spreads and the proposed fee. Target flow keeps the flow held an hour or more, the within-hour
flow held 15 to 60 minutes, and a tenth of the rest (`calib/target.py`).

| Flow and pricing | Retained NAV | Distributed, 90 days | Spread income | Profit fees | Operator's share a day |
|---|---|---|---|---|---|
| Current, spreads and fee | $107M | $798M | $481M | $316M | $1.77M |
| Target, spreads and fee | $107M | $217M | $142M | $73M | $483k |
| Current, genesis pricing | $107M | $76M | - | $74M | $168k |
| Target, genesis pricing | $107M | $54M | - | $52M | $119k |

The retained NAV does not move: in every variant the exposure caps' refusals set it (under 1% of
directional orders refused at $107M, about 4% at the grid's next floor down, $67M), and house cash
stays above zero in at least 99% of paths at every floor. Spread income raises what is
distributed, not what must be retained. Spread income assumes the flow trades as it does at a zero
spread; the target-flow rows are the more conservative yardstick.

## #500: BTC-USD and ETH-USD at 100x

#500 asks how BTC-USD's and ETH-USD's move to 100x changes the cap study's answers. Each market
alone at its share of the flow (#111's per-market stage, 2,000 paths):

| Market | Margins and pricing | Refusals end at | NAV below zero, ever |
|---|---|---|---|
| BTC-USD | 10x (committed) | $10.0M | 63% |
| BTC-USD | 100x, genesis pricing | $31.6M | 40% |
| BTC-USD | 100x, spreads and fee | $31.6M | 0% |
| ETH-USD | 10x (committed) | $5.6M | 65% |
| ETH-USD | 100x, genesis pricing | $17.8M | 44% |
| ETH-USD | 100x, spreads and fee | $17.8M | 0% |

At 100x the caps bind on three times the notional, so refusals end at three times the floor; the
larger floor makes NAV below zero less likely, and with spreads and the fee it does not happen in
any path. The hourly model closes positions out at their trigger and so shows no gap bad debt in
markets that never close; on the trade grids, the index passes the half of a 1% margin left at the
trigger within the engine's next price (up to 3 s) in about 3 seconds a day over the two markets,
with a mean loss past the balance of 0.5% of the position's notional, and in 25 minutes of
2025-10-10's four stress hours. At 2% margin it is under a second a day; at 5%, none in the year
sample and 3 seconds on the stress hours. Paired
positions across unpriced moves are in the previous section: they stop paying at 2% margin. This
study does not re-run the cap study's joint floors or its utilisation stress levels at 100x,
which #500 also asks for.

## Proposals for the owner

1. Adopt the spread tiers in `results/spread_tiers.toml`.
2. Adopt the holding-time fee (100% to 5 minutes, 10% at 30 minutes) with the hold measured as
   above; the 10 s variant is the gentler alternative.
3. Replace the toxicity score with the time-decayed, weight-capped score above.
4. Adopt the volume tiers and the counting rules.
5. On BTC-USD and ETH-USD, keep the leverage tiers 2 and 3 earn at 50x (2% margin), or bound how
   long a market can stay unpriced: at 1% margin a stress day still pays paired positions.
6. Authenticate the Databento quotes, or require a second source for the markets they price.
7. Decide whether FX and index first tiers should go below 1 bp, which needs a finer spread unit
   and a protocol version.

## Limits

- The spreads, fee and score are measured on three crypto markets' trades; the other markets'
  moves are scaled from hourly ranges, and their latency figures from BTC-USD's.
- The flow does not respond to prices: spread income assumes the flow trades as it does at a zero
  spread, and target flow is a reshaping of the measured flow, not a measured target client.
- The latency strategy is one rule family, measured with its later moves mirrored, so takes are
  per rule, not an optimum; the stick and size effects on it are bounds.
- The holding-time profile is by notional, applied to margin classes.

## Re-running

From a virtualenv outside the repo with `requirements.txt`, in this directory, each stage reading
committed data and earlier stages' results:

```
python -I run.py spreads
python -I run.py latency --workers 3
python -I run.py scaled --workers 3
python -I run.py fee
python -I run.py frontier
python -I run.py fx_tiers
python -I run.py toxicity
python -I run.py carrot
python -I run.py attacks --workers 3
python -I run.py per_market --workers 3 --paths 2000
python -I run.py gaps
python -I run.py retained --workers 3 --paths 1000
python -I run.py report
python -I -m pytest tests
```

## Files

- `run.py`: the stages.
- `calib/`: `common.py` (the sibling studies and the genesis), `volatility.py` (lag moves),
  `spreads.py` (tiers), `latency.py` (the latency rule on a grid), `jump.py` (a jump stress
  trigger variant), `fee.py` (schedules, the hold clock, honest cost), `toxicity.py` (the score),
  `carrot.py` (volume tiers), `target.py` (target flow), `unprice.py` (#549's unpriced-spell
  measure at the calibrated spreads), `report.py`.
- `tests/`: the fee, hold clock, score, tiers, target flow, spreads and latency rule on hand cases.
- `results/`: each stage's output and `report.md`.
