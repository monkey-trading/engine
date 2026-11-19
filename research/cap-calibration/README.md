# Exposure-cap calibration

The house takes the other side of every trade and starts with no capital. Each of its exposure caps,
one for each market and one in total, is the larger of a share of the house's net asset value (NAV)
and a fixed dollar floor. At launch the house's NAV is close to zero, so the floors alone decide how
much risk the house carries before trading losses have built its capital, and how many orders it
refuses while it is small. This study estimates, for each market and in total, how the floors trade
the probability that the house becomes insolvent in its first 90 days against the share of opening
orders that the caps refuse.

At the launch margins, a floor of $1 million on every market meets a 1% insolvency target on both of
the study's measures. Over 2,000 simulated paths of 90 days, the house's NAV and its cash each fall
below zero in 0.85% of paths, and these floors refuse 64% of opening notional in the first week and
23% over the 90 days. No floor above zero meets a target of 0.1% or 0.01%. The launch floors are
larger. The [launch-caps study](../launch-caps/README.md) sets them by running this study's stages
again at the launch pricing, which adds the spreads and the holding-time fee that this study leaves
out.

The study also calibrates three other parameters from the same data and model:

- the stress levels' fast-move thresholds of 75 and 200 basis points over 60 seconds, and their
  divergence triggers at 60.01% and 95% of each market's divergence limit, which the launch genesis
  uses;
- the prior and memory of the notional-weighted toxicity score, $5,629,518 and $11,259,036;
- a retained NAV of $107 million, the NAV the house must keep back from staking distributions so
  that the vault always covers its clients' balances.

A basis point, abbreviated to bp, is one hundredth of one percent. Amounts are in US dollars.

- [Results](#results): [exposure-cap floors](#exposure-cap-floors),
  [sensitivity to the assumptions](#sensitivity-to-the-assumptions),
  [BTC-USD and ETH-USD at 100:1](#btc-usd-and-eth-usd-at-1001), [stress triggers](#stress-triggers),
  [toxicity parameters](#toxicity-parameters), [retained NAV](#retained-nav),
  [what the results leave open](#what-the-results-leave-open)
- [Method](#method)
- [Data and sources](#data-and-sources)
- [Limits](#limits)
- [Re-running](#re-running)

## Results

The study measures insolvency in two ways. Each is the share of simulated paths in which the event
happens at some hour within 90 days, starting from a house with no capital:

- **NAV below zero:** the house's NAV, which is its cash less its clients' unrealised profit and
  loss, is below zero.
- **House cash below zero:** the vault holds less USDC than its clients' balances, which are the
  most that clients may withdraw on demand. This is NAV below what the house owes in withdrawals,
  counting only realised results.

The cost of a floor is the share of opening notional that the caps refuse. Notional is a position's
value at the market price. The refused share counts directional positions, which are not legs of a
hedge, and churn, which is positions held for under an hour. It is given for the first week, when
the floors carry the house, and for the 90 days. The floor where a market's refusals end is the
smallest floor at which that market, run alone, refuses under 1% of its directional notional over
90 days.

Each probability comes with the count of paths in which the event happens and the exact one-sided
95% upper bound on the rate. On 2,000 paths, a target of 1% allows 20 failing paths, 0.1% allows 2,
and 0.01% allows none. When no path fails, the bound is still 0.15%, so the two smaller targets can
be met in the paths but cannot be established at 95% confidence. Every result on the floors is at
the launch margins, with BTC-USD and ETH-USD at 100:1, and without spreads or the holding-time fee.

### Exposure-cap floors

The recommended floors for each target and measure are:

| Target | NAV never below zero | House cash never below zero (the vault covers client balances) |
|---|---|---|
| 1% | $37M in total: $1M on every market (0.85%; 17 paths, bound <1.27%). Refuses 64% of launch-week opening notional, 23% over 90 days. | $37M in total: the same $1M on every market (0.85%; 17 paths, bound <1.27%). The floors where each market's refusals end ($274M) measure 3.50% (70 paths). |
| 0.1% | $0 in total: nothing opens. The best candidate with any floor, $100k on every market, measures 0.35% (bound <0.66%). | $0 in total: nothing opens, at the same 0.35%. |
| 0.01% | $0 in total: nothing opens. | $0 in total: nothing opens. |

The study tests two families of candidate floors. The first sets each market's floor from that
market run alone, at a risk level described under [Method](#calibration-of-the-floors). Run alone,
no market stays at or below 1% at any floor, because a small market's own fees cannot absorb its
losses when its price gaps. At a level of 1% or below, that family therefore puts $0 on every
market, and nothing opens. The second family puts the same floor on every market. With every market
at $1M, the fees pooled from the crypto markets carry the small markets, so the $1M uniform floor is
the choice at 1% on both measures. At 0.1% and 0.01%, every candidate with any floor measures above
the target, so the recommendation is no floor.

What the floors trade against each other:

- **Small floors make early dips rarer and shallower, and large floors make them larger.** With $10M
  on every market, NAV falls below zero at some hour in 5.85% of paths (117 of 2,000). The median
  first dip comes 0.1 days after launch, before fees have built any capital. NAV falls below -$100k
  in 1.30% of paths and never below -$1M. House cash goes below zero in 1.55% of paths (31), no
  withdrawal ever waits in the vault's first-in, first-out queue, and NAV ends the 90 days at a
  median of $75.7M. At the floors where each market's refusals end, $274M in total, NAV falls below
  zero in 24.6% of paths, below -$100k in 14.7% and below -$1M in 1.9%.
- **Small floors refuse orders for their size, not for the exposure.** The caps refuse an order
  larger than the cap, whatever flows around it. $1M of margin buys $26.4M of BTC-USD or ETH-USD
  under their margin bands. A floor under a few million dollars therefore refuses most of the flow
  on the markets that carry most of it: 91% of launch-week notional at $100k a market, 64% at $1M
  and 39% at $3M, against 13% at $10M, which is papertrade.xyz's position cap.
- **Along the risk-level family, the dip rises quickly with the floors.** NAV falls below zero in
  1.45% of paths at $1.18M in total, the 3% level. The share passes 3.85% at $15.2M, 13.45% at
  $38.5M and 24.75% at $143M, and reaches 24.6% at $274M, where every market sits at the floor where
  its refusals end.
- **Pooling carries the small markets.** Each market alone, at the floor where its refusals end,
  dips below zero in 39.5% (XAU-USD) to 73.0% (DE40) of paths, because its own fees are small next
  to its losses across gaps. All markets together at $10M each earn the house a mean of $70.7M of
  fees over the 90 days, and the crypto markets fund the other markets' risk.
- **The total floor does not bind.** At $1M on every market, $37M in total, a total floor of 25% to
  100% of that sum leaves the measured risk unchanged. It moves the refused share by 0.2 points in
  the first week and by none over 90 days.

The three tables below run all markets together. The first two set each market's floor at a risk
level: the largest of its tested floors at which the market, run alone, stays at or below that level
on the table's measure. The third puts the same floor on every market. In the last row of the first
two tables, every market is at the floor where its refusals end. Bad debt is a loss beyond a
client's balance, which the house bears.

Floors set for NAV below zero at some hour:

| Level | Total floor | Markets with a floor | NAV below zero | Cash below zero | NAV below -$100k | Refused, week 1 | Refused, 90 days | Median NAV, day 90 | Mean fees | Mean bad debt |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 | $0 | $0 |
| 0.01% to 1% | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 | $0 | $0 |
| 3% | $1.18M | 2 | 1.45% | 1.55% | 0 | 81.6% | 28.3% | $61M | $54.1M | $509k |
| 10% | $15.2M | 12 | 3.85% | 2.00% | 0.40% | 35.3% | 13.4% | $72.6M | $66.5M | $574k |
| 20% | $21.1M | 15 | 4.30% | 1.75% | 0.55% | 28.6% | 11.6% | $74M | $68.3M | $586k |
| 30% | $38.5M | 17 | 13.45% | 2.15% | 5.10% | 18.6% | 7.0% | $75.4M | $71.4M | $596k |
| 50% | $143M | 29 | 24.75% | 3.50% | 14.60% | 3.6% | 1.0% | $75.7M | $74M | $630k |
| Where refusals end | $274M | 37 | 24.60% | 3.50% | 14.70% | 2.4% | 0.7% | $75.7M | $74.1M | $632k |

Floors set for house cash below zero at some hour:

| Level | Total floor | Markets with a floor | NAV below zero | Cash below zero | NAV below -$100k | Refused, week 1 | Refused, 90 days | Median NAV, day 90 |
|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 0.01% to 1% | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 3% | $278k | 2 | 1.00% | 1.05% | 0 | 94.3% | 35.6% | $55.3M |
| 10% | $29.9M | 12 | 16.35% | 2.60% | 6.20% | 23.5% | 7.7% | $75.2M |
| 20% | $104M | 20 | 24.85% | 3.50% | 14.60% | 3.9% | 1.0% | $75.6M |
| 30% | $210M | 26 | 24.65% | 3.50% | 14.70% | 2.5% | 0.7% | $75.7M |
| 50%, where refusals end | $274M | 37 | 24.60% | 3.50% | 14.70% | 2.4% | 0.7% | $75.7M |

The same floor on every market:

| Floor | Total floor | Markets with a floor | NAV below zero | Cash below zero | NAV below -$100k | Refused, week 1 | Refused, 90 days | Median NAV, day 90 | Mean fees | Mean bad debt |
|---|---|---|---|---|---|---|---|---|---|---|
| $100k | $3.7M | 37 | 0.35% | 0.35% | 0 | 90.9% | 33.1% | $57.3M | $50.5M | $491k |
| $300k | $11.1M | 37 | 0.75% | 0.75% | 0 | 82.6% | 28.8% | $60.7M | $53.9M | $522k |
| $1M | $37M | 37 | 0.85% | 0.85% | 0 | 64.1% | 22.9% | $65.4M | $58.4M | $558k |
| $3M | $111M | 37 | 1.30% | 1.25% | 0.05% | 38.8% | 16.4% | $70.5M | $63.7M | $605k |
| $10M | $370M | 37 | 5.85% | 1.55% | 1.30% | 13.1% | 8.1% | $75.7M | $70.7M | $632k |

The recommendation for a target is the candidate that measures at or below the target and refuses
the least. The upper bound is the uncertainty in the measurement, not the criterion for the choice,
so a choice that measures exactly at its target sits within sampling noise of it.
`results/report.md` holds every table, including each market run alone and the sweep of the total
floor.

### Sensitivity to the assumptions

Each variant below runs at the floors that both measures choose at 1%, $1M on every market and $37M
in total, on 1,000 paths, where no failing path bounds the rate below 0.30%. On those paths the
baseline puts NAV and house cash below zero in 0.80% each.

| Assumption | NAV below zero | Cash below zero | NAV below -$100k | Mean bad debt |
|---|---|---|---|---|
| Baseline | 0.80% | 0.80% | 0 | $557k |
| No farming (no hedged flow) | 12.60% | 13.20% | 0 | $131k |
| One client direction for all markets | 2.30% | 2.20% | 0 | $557k |
| Farmers hold 10% of their pairs through every closure | 0.80% | 0.80% | 0 | $6.56M |
| Volatility x2 | 0.90% | 0.80% | 0 | $1.89M |
| Markets bootstrapped independently | 0.40% | 0.40% | 0 | $561k |
| Farming share x2 | 0 | 0 | 0 | $1.01M |
| Flow x0.25 / x4 | 2.00% / 0.30% | 1.80% / 0.30% | 0 / 0 | $101k / $2.42M |
| Leverage mapped relative to each limit | 6.30% | 3.60% | 0 | $26.7k |

Farming means opening pairs of opposite positions, the hedged flow, to collect the token that a
venue mints on losses.

- **Farming is what keeps the house solvent early.** Hedged flow brings in most of the fees. On
  these paths, the mean fee income over 90 days is $58.4M with hedged flow and $16.1M without it.
  Without it, NAV falls below zero in 12.6% of paths and house cash in 13.2%, at floors that
  otherwise measure 0.8%. Every scaling of the floors from 0.1 to 1 times leaves both above 1%
  (`results/sensitivity.csv`).
- **Clients leaning the same way in every market is among the largest market risks.** The house's
  net exposure then adds up across markets instead of netting, and the measures rise to 2.3% and
  2.2%.
- **Pairs held through closures are a risk the floors barely touch.** A pair adds no net exposure,
  so a cap limits it only through the size of its orders. A gap that takes a leg past its balance is
  still bad debt: $6.56M over 90 days in this variant, all of it on markets that close, since
  BTC-USD and ETH-USD never do. Margin over closures controls this risk, and floors do not.
- **The share of flow that trades at the maximum leverage decides the fees.** The baseline caps each
  position's leverage at the market's maximum, so most flow trades at it. When each position's
  leverage is instead mapped to the same fraction of its market's maximum as it was of
  papertrade.xyz's 1000:1, fees over the 90 days fall from $58.4M to $3.17M. NAV then falls below
  zero in 6.3% of paths and house cash in 3.6%.

### BTC-USD and ETH-USD at 100:1

At launch, BTC-USD and ETH-USD charge a margin of 1% on the first $2M of notional, 2% up to $10M,
and 5% above that, which allows leverage of up to 100:1. The margin on these two markets changes the
answers more than any other input. The model caps each position's leverage at the market's maximum,
so most of the flow on the two markets moves with the margin, and those markets carry 71% of all
flow. $1M of margin buys $26.4M of notional at 100:1, against $5.5M at 10:1 (10% margin up to $1M of
notional and 20% above). The two markets therefore fill their caps sooner and lose more when their
prices gap.

The table compares the committed results at 100:1 with the same stages at 10:1, at the same seed and
path counts. Every stage ran on the committed `data/derived/` with only the bands changed. The 10:1
figures are not kept in `results/`. They are the study's results at commit `87f5d08ecd`, which
`git show 87f5d08ecd:research/cap-calibration/results/report.md` prints.

| Value | 10:1 | 100:1 |
|---|---|---|
| BTC-USD alone: floor where refusals end | $10M | $31.6M |
| ETH-USD alone: floor where refusals end | $5.62M | $17.8M |
| BTC-USD alone at that floor: NAV below zero / cash below zero | 63.00% / 21.90% | 40.35% / 12.85% |
| ETH-USD alone at that floor: NAV below zero / cash below zero | 64.80% / 23.35% | 43.70% / 14.80% |
| BTC-USD, ETH-USD alone: mean fees over 90 days | $6.33M, $4.28M | $36.2M, $23.2M |
| Floors at 1%, NAV measure | $1M on every market ($37M; 1.00%) | $1M on every market ($37M; 0.85%) |
| Refused at those floors, week 1 / 90 days | 47.0% / 20.1% | 64.1% / 22.9% |
| Floors at 1%, house-cash measure | $240M (0.80%) | $37M (0.85%) |
| Refused at those floors, week 1 / 90 days | 0.3% / 0.1% | 64.1% / 22.9% |
| Floors where refusals end, every market: total | $240M | $274M |
| At them: NAV below zero / cash below zero / NAV below -$100k | 19.7% / 0.80% / 3.60% | 24.6% / 3.50% / 14.7% |
| At them: median NAV on day 90 / mean fees | $26.2M / $25.3M | $75.7M / $74.1M |
| $10M on every market: NAV below zero / cash below zero / NAV below -$100k | 18.75% / 0.85% / 3.40% | 5.85% / 1.55% / 1.30% |
| Mean bad debt over 90 days, at the floors where refusals end | $632k | $632k |
| Utilisation level 1 (`utilisation_bps`) | 7000 | none below 9500 passes |

**Floors.** At 100:1 both measures choose $1M on every market at 1%. At the floors where refusals
end, BTC-USD and ETH-USD carry net exposures three times those at 10:1, on a margin a tenth the
size, and the house-cash measure at those floors rises from 0.80% to 3.50% (70 paths). Smaller
floors therefore trade a larger refused share for a smaller dip. At $1M a market the caps refuse 64%
of launch-week notional, and at $10M they refuse 13%, against 1.9% at $10M at 10:1. NAV and fees are
larger throughout, because the flow carries more notional for each dollar of margin. That result
rests on the approximated 100:1 flow profile that [Limits](#limits) describes.

**Bad debt.** The mean is unchanged, because the hourly model fills a crypto close-out at its
trigger price, so BTC-USD and ETH-USD show no bad debt at either margin. The $632k comes from the
markets that close. The gap risk at 100:1 is therefore not in this study's results. Paired positions
held in two accounts at the maximum leverage are the case that net exposure caps cannot limit. A leg
whose price jumps past its margin, with no close-out in between, leaves bad debt with the house. At
1% margin that takes a jump of about 1.1%, against about 11% at 10:1. The
[spread-calibration study](../spread-calibration/README.md) measures this risk at 1% margin, on
50-millisecond grids of Coinbase and Kraken trades
(`research/spread-calibration/results/report.md`):

- The index can pass the half of the margin that is left at the close-out trigger before the
  engine's next price, which can take up to 3 seconds. That happens for about 3 seconds a day over
  the two markets, with a mean loss past the balance of 0.5% of the position's notional. Over the
  four stress hours of 10 October 2025 it happens for 25 minutes. At 2% margin it happens for under
  a second a day. At 5% margin it never happens in the year's sample, and for 3 seconds over the
  stress hours.
- Paired positions held across moves during which the market has no price take about nothing from
  the house on a typical day, but $19.1M over those four stress hours. At 2% margin they stop
  paying.

**Utilisation levels.** The utilisation trigger fires more often at every level, because BTC-USD
fills its cap far more often. At a level of 7000 bps, the trigger fires in 19.0% of BTC-USD's open
hours at the floors where refusals end, against 2.9% at 10:1, and in 51.7% at a uniform $10M. No
candidate below 9500 bps keeps the flow-weighted rate under 2% on both of the relevant floor
families. [Stress triggers](#stress-triggers) gives the full table. The move and divergence triggers
do not depend on margin, and the stage at 100:1 reproduces 75 and 200 bps and shares of 6001 and
9500 bps.

The stages `run.py per-market`, `joint`, `sensitivity` and `stress` ran on the committed
`data/derived/`, with seed 1, 2,000 paths (1,000 for the sensitivities) and three workers. They took
21, 44, 89 and 16 minutes (`results/manifest_*.json`).

### Stress triggers

The engine sets each market's stress level, which widens its spreads. The level rises as soon as any
reading meets the trigger of a higher level, and it falls only after a five-minute cooldown. Every
reading is a share of a bound that depends on the market:

- a fast move, in bps of the index mid;
- the divergence between the market's venues, as a share of its divergence limit,
  `max_divergence_bps`;
- the house's exposure in the market, as a share of the market's cap at the house's NAV.

The `stress` stage calibrates the triggers so that each level fires when the house's risk rises, and
not on noise. Its values and the launch values are:

| Parameter | This study at 100:1 | Launch value |
|---|---|---|
| Fast-move window, `stress_move_window_ms` | 60000 | 60000 |
| Fast move, level 1 / level 2, bps | 75 / 200 | 75 / 200 |
| Divergence share, level 1 / level 2, bps of the limit | 6001 / 9500 | 6001 / 9500 |
| Utilisation, level 1 / level 2, bps of the cap | none / 9500 | 8000 / 9500 |

The launch value of 8000 bps for utilisation level 1 comes from the
[launch-caps study](../launch-caps/README.md), at the launch pricing and floors.
`results/stress_choice.json` holds this study's values with their method, and `stress_moves.csv`,
`stress_divergence.csv` and `stress_utilisation.csv` hold the tables behind them.

**Fast move.** The trigger reads the largest distance of the index mid from the lowest or highest
mid in its trailing window. The study chooses the most protective pair of thresholds whose
flow-weighted time at level 1 stays under 1%, with the time at level 2 under 0.1% and every market's
time at level 1 under 5%. At a 60-second window the candidates measure:

| Level 1 / level 2 threshold, bps | Time at level 1, flow-weighted | Worst market's time at level 1 | Mean volatility percentile of level-1 firings |
|---|---|---|---|
| 50 / 150 | 1.51% | 10.1% | 0.975 |
| 60 / 150 | 0.80% | 6.6% | 0.985 |
| **75 / 200** | **0.37%** | **3.8%** | 0.991 |
| 100 / 250 | 0.14% | 1.4% | 0.994 |
| 150 / 300 | 0.014% | 0.39% | 0.995 |

The last column is the mean, weighted by the market mix, of each market's mean volatility percentile
in the hours when level 1 fires. `choose_move` reports the same figure.

The window stays at 60 seconds. Windows of 30 and 120 seconds give parallel curves of fire rate,
because a longer window raises every reading alike. A 60-second window keeps its 12 buckets at one
every 6 seconds, against feeds that publish at least one mid a second. The chosen thresholds of
75 and 200 bps fire on the volatile crypto, oil and share markets. They fire 0.1% to 1.2% of the
time on the markets that carry at least 1% of the flow, and up to 3.8% of the time on the smallest
share listings, which keeps the worst market within the 5% bound. They are almost silent on FX and
indices, firing 0% to 0.38% of the time on the index markets and never on FX. Those markets rely
mainly on the divergence trigger and on the rule that leaves a market without a price when its
venues disagree. Thresholds of 150 and 300 bps, the development configuration's values, fire on none
of these markets outside crashes.

**Divergence.** A divergence reading of d bps means that the venues' mids sit d bps apart. With the
two fresh sources the protocol requires, the index mid is their midpoint. A fill at the index mid
then loses up to d/2 bps if the price snaps to the other venue, and d/4 bps on average if the snap
is uniform. That bound is exact for an index that is the midpoint of two sources. With more fresh
sources, the index is their median and the worst loss can exceed d/2, up to nearly d. The shares
would then need deriving again against the true loss for any market whose source count changes. A
share s of a market's limit B fires at d = ceil(s × B / 10000) bps, because the engine truncates the
share.

Level 1 takes the lowest share at which the worst stale-fill loss at the firing reading covers the
level's minimum spread on the market with the tightest limit, so that d/2 is at least the minimum
spread. That needs a reading of 4 bps. On the tightest limits, which are 5 bps, a share of
6000 still fires at 3 bps, and 6001 is the lowest share that fires at 4 bps. The study therefore
chooses 6001, which fires at 16 bps on crypto's 25-bp limit.

Level 2 would need a reading of 20 bps, which cannot occur where the limit is 5 bps, so the study
falls back to 9500. On limits below about 21 bps, a share of 9500 first fires at the limit itself.
The market still has a price at that reading, because a market loses its price only when the reading
is above the limit, so level 2 is the last widening before trading stops. On crypto's limit,
9500 fires at 24 bps. The development configuration's shares of 6000 and 8000 fire on the tightest
markets below the stale-fill cost of the level-1 spread. `stress_divergence.csv` also reports the
expected-cost rule, under which d/4 must be at least the minimum spread. No share under 10000 meets
that rule on the 5-bp markets.

**Utilisation.** The model records each market's hourly utilisation and exposure on the same
2,000 paths as the floor study, at three families of floors:

- the floors where each market's refusals end, $274M in total, which measure 3.50% on the house-cash
  measure at 100:1 (named `house_cash_1pct` in the files);
- a uniform $10M;
- a uniform $1M, which both measures choose at 1%.

Level 2 stays at 9500 bps, the last widening before the caps start to refuse orders. Level 1 is the
lowest candidate whose flow-weighted fire rate stays under 2% on the first two families, and under
5% on every market with at least 1% of the flow. At 10:1, a level of 7000 bps passes, firing in 1.4%
to 1.5% of open hours flow-weighted and in 2.9% on the worst market. At 100:1 no candidate passes,
because BTC-USD and ETH-USD fill their caps far more often. The flow-weighted fire rates, with
BTC-USD's in brackets, are:

| Level, bps | Floors where refusals end | Uniform $10M | Uniform $1M |
|---|---|---|---|
| 5000 | 27.6% (46.6%) | 45.0% (71.6%) | 47.2% (68.7%) |
| 6000 | 17.8% (31.9%) | 37.4% (62.5%) | 39.7% (59.9%) |
| 7000 | 10.1% (19.0%) | 29.4% (51.7%) | 32.1% (50.1%) |
| 7500 | 7.2% (13.8%) | 25.3% (45.7%) | 28.1% (44.8%) |
| 8000 | 4.8% (9.4%) | 21.2% (39.1%) | 24.2% (39.2%) |
| 8500 | 2.9% (5.8%) | 17.0% (32.1%) | 20.1% (33.3%) |
| 9000 | 1.3% (2.6%) | 12.7% (24.4%) | 16.0% (27.0%) |
| 9500, level 2 | 0.26% (0.5%) | 8.1% (16.0%) | 11.7% (20.2%) |

At 10:1 and 7000 bps, the three families fire in 1.5%, 1.4% and 35.4% of open hours flow-weighted,
and BTC-USD fires in 2.9%, 2.9% and 55.9%.

On the floors where refusals end, 9000 bps would pass, at 1.3% and 2.6% for BTC-USD, which is
500 bps under level 2. Those floors fail the 1% target, however, and a uniform $10M fires at 12.7%
at that level. Under $1M floors, a market's cap is so small that every level fires constantly. That
is a property of those floors, which the study reports rather than calibrates to. Because those are
the floors the study recommends, the utilisation trigger is not a rare event at them.

At 7000 bps on BTC-USD, at the floors where refusals end, the mean exposure when the trigger fires
is $25.6M, 81% of the $31.6M cap. Against a 99th-percentile gap of 180 bps to the next price, that
is a $461k hole. The model nets each hour's orders against the cap on net exposure and fills
offsetting flow in full, as [Method](#admission-of-orders) describes. After settlements on one side,
a book can therefore briefly exceed the cap, where the engine, which checks each order, would refuse
more. The exposure-at-fire rows of `stress_utilisation.csv` inherit that approximation. The choice
of level rests on the fire rates, which it affects in only a handful of hours per market.
`stress_choice.json` records level 1 as null when no candidate passes, with the fire rates at the
highest candidate below level 2.

### Toxicity parameters

A toxicity score measures how consistently the price moves in an account's favour shortly after its
trades. That move is the trade's markout, measured here over 2 seconds. Accounts whose score reaches
a tier pay that tier's extra spread. The `toxicity` stage calibrates the notional-weighted score,
which averages an account's markouts weighted by the notional of each fill. It sets the score's
`prior`, the neutral history a new account starts with, its `memory`, the amount of recent fill
notional the score averages over, and its tier thresholds and spreads. The aim is a score that
identifies toxic flow quickly without drifting on large fills.

| Parameter | Calibrated value |
|---|---|
| `prior` | $5,629,518 |
| `memory` | $11,259,036 |
| Tier 1, minimum score / spread | 1 bp / 2 bps |
| Tier 2, minimum score / spread | 2 bps / 3 bps |

The design toxic level is a sustained markout of 2 bps on every fill. The calibrated tiers detect an
account at that level within 75 fills of the median size. A tier at 5 bps or above would never
detect it, because at a sustained 2-bp markout the score levels off below 5 bps at any fill size.

The launch engine scores toxicity in a different way, as the number of standard errors by which an
account's markout edge exceeds zero. The [spread-calibration study](../spread-calibration/README.md)
calibrates that score. The launch genesis keeps this study's prior and memory for the
notional-weighted score, which applies when a log written under an earlier protocol version is
replayed. The tiers above are in no launch configuration.

The committed values are from the 37-market list at a 10% margin on BTC-USD and ETH-USD. Running the
same stage, `python -I run.py toxicity`, on the launch genesis gives a larger memory, because 1%
margin raises the two markets' median fill from $100k to $1M and their 99th-percentile fill from
$3.2M to $14.8M:

- ETH-USD instead of Brent sets the memory, which rises from $11,259,036 to $17,626,099, and the
  prior from $5,629,518 to $8,813,050;
- a toxic account reaches tier 1 within 100 fills of the median size, rather than 75;
- a burst of 25 fills half a second apart crosses tier 1 1.0% of the time, rather than 3.9%;
- the tier thresholds of 1 and 2 bps, and their spreads of 2 and 3 bps, do not change.

### Retained NAV

The `retained` stage sizes the staking liquidity floor, which is the NAV the house must hold back
from distributions so that the vault always covers its clients' balances. The house opens at a
candidate floor as its capital and sweeps everything above it each hour. That is the worst case,
because the engine's own deductions can only retain more. The house trades under the genesis caps'
shares of that NAV, with no dollar floors, and with BTC-USD and ETH-USD at the launch bands. Cloned
markets extend the launch set to 50 to 500 markets, since the launch set alone cannot show how the
answer scales.

The retained NAV is the lowest floor on the grid that refuses at most 1% of directional notional and
keeps house cash below zero in at most 1% of paths, at that floor and at every higher one:

| Markets | Paths | Retained NAV | Cash below zero | Refused, 90 days | Mean fees |
|---|---|---|---|---|---|
| 37 (launch) | 4000 | $107M | 0 | 0.49% | $74.3M |
| 100 | 1000 | $107M | 0 | 0.49% | $78M |
| 500 | 200 | $107M | 0 | 0.43% | $101M |

The answer is stable in the number of paths: on the first 500, 1,000, 2,000 and 4,000 paths, the
37-market answer is $107M. It is also stable in nearly every sensitivity, including correlation from
0.3 to 1, volatility doubled, flow from 0.25 to 4 times, one client direction for all markets, the
sweep interval, and no farming. What moves it:

- **The margin on BTC-USD and ETH-USD dominates.** With a 10% margin on the two markets instead of
  the launch bands, retained NAV falls to $26.2M at every count up to 300 markets, and to $41.9M at
  500 markets. A high margin lets a small NAV carry little risk. Bands of 1% up to $1M and 20% above
  give the same $107M as the launch bands.
- **The flow on each cloned market is the scaling assumption.** The answer does not change with the
  number of markets, because crypto carries about 76% of the flow and the cloned markets are light
  equity markets. When every clone is instead as busy as BTC-USD or ETH-USD, the retained NAV
  reaches $440M at 100 markets and $2.88B at 500. Mixed clones sit between, at $172M for
  200 markets.
- **Fixed dollar caps on each market are far weaker than caps that are a share of NAV.** With dollar
  caps of $10M a market in place of the shares of NAV, retained NAV falls to $2M at 37 and
  100 markets. At that floor on 37 markets, cash falls below zero in 0.60% of paths (12 of 2,000,
  upper bound 0.97%). A fixed cap does not grow with the house, so a small floor meets the same
  flow. Caps of $1M a market retain $1M, and already measure 0.67% at 500 markets.

Under the launch bands the house must therefore hold $107M as NAV and sweep only the excess. With
BTC-USD and ETH-USD at a 10% margin, $26.2M would serve, so the margin bands are what moves the
figure. The launch deployment sets the staking liquidity floor at $30.5M
(`ops/runbooks/production-deployment.toml`), below this study's figure.

The stage writes `results/retained_nav.csv`, `results/retained_stability.csv` and
`results/manifest_retained.json`.

### What the results leave open

- **The measure and the target.** At 100:1, NAV below zero at any hour and house cash below zero
  choose the same floors at 1%, $1M on every market. A tolerance on depth admits larger floors. With
  NAV allowed to fall to -$100k but no lower, $3M on every market measures 0.05% (1 path) and
  refuses 39% of launch-week notional and 16% over 90 days, and $10M measures 1.30%.
- **Seeding the house.** The early dip happens before fees accrue. Seed capital removes it, and
  floors do not.
- **Leverage on markets that gap.** Floors leave untouched the bad debt from gaps past the 100:1 and
  200:1 margins of oil, silver and indices, past the 10:1 margin of shares across earnings, and past
  the 1% margin of BTC-USD and ETH-USD. The hourly model shows no crypto bad debt, because it fills
  a crypto close-out at its trigger. The measure on trade data is in
  [BTC-USD and ETH-USD at 100:1](#btc-usd-and-eth-usd-at-1001). Launch keeps every margin constant
  across closures, with no weekend margin, so the house carries this gap risk.
- **The utilisation level.** At 100:1 no level below 9500 bps keeps the fire rate within its bounds
  on the floors that matter at launch, as [Stress triggers](#stress-triggers) shows.

## Method

`run.py` runs a Monte Carlo simulation (`model/`) hour by hour over 90 days, starting from a house
with no capital.

### Measures

The calibration uses the two measures that [Results](#results) defines, each read at the end of
every hour. The simulation also records three more, each as a probability over the 90 days:

| Measure | Event |
|---|---|
| Queue | Under the modelled withdrawals, some withdrawal waits in the vault's first-in, first-out queue because the vault cannot pay it. |
| NAV below zero at day 90 | House NAV is below zero at the end. |
| NAV below -$100k, -$1M | The lowest NAV falls below that depth. |

Nothing is extrapolated beyond what the paths count.

### Prices

A path is 13 weeks of history, drawn in blocks of four consecutive weeks from the 102 weeks of
hourly bars, with the same weeks for every market. Drawing whole weeks keeps the correlation between
markets, the clusters of volatility, the weekend and holiday gaps, and each venue's trading
sessions. Each market's mean hourly return over the window is removed, so trends in the history do
not decide who wins.

### Client flow

The model carries papertrade.xyz's flow over by margin, the collateral that traders commit, rather
than by notional, because these markets allow 10:1 to 200:1 rather than 1000:1. Each simulated day
brings papertrade.xyz's margin of the same day after its launch. The first ten days are its ten full
days in order, and every later day takes the mean of its last seven, $182M a day. Each market takes
its share of that margin by its volume on Hyperliquid, spread over the market's open hours. The
shares by asset class are 76.5% for crypto, 8.8% for indices, 6.0% for oil, 3.1% for metals, 5.5%
for shares and 0.1% for FX. A market with no Hyperliquid listing takes the smallest listed share in
its asset class. Each class of papertrade.xyz flow keeps its share of margin, its hold and its long
share.

Each position's leverage is capped at the market's maximum, which is set by its first margin band,
so most flow trades at the maximum. A position's notional is no more than its margin buys under the
market's margin bands, which charge each slice of notional at its band's rate. $1M of margin buys
$10M of a 10:1 market with one band, $3.3M of a share (10% up to $500k, 20% up to $2M, then 50%),
and $26.4M of BTC-USD or ETH-USD (1% up to $2M, 2% up to $10M, then 5%). BTC-USD's and ETH-USD's
bands have no size or leverage profile in the papertrade.xyz data, so `flow.with_genesis_profiles`
gives them the profile of the oil schedule, which has the same first-band rate.

The flow comes in three kinds:

- **Directional** positions open at the hour's close and settle after their class's hold of 1, 4,
  16 or 48 hours. A planned exit that falls inside a closure waits for the market to reopen. The
  long share follows an hourly autoregressive process around each class's measured share, fitted to
  papertrade.xyz's BTC flow on the full days, with a coefficient of 0.69, a residual of 0.12, and
  +0.05 for each unit of the previous hour's log return.
- **Hedged** positions open as offsetting pairs in two accounts, since an engine account nets to one
  position in each market. A pair ends at its hold, or when either leg is closed out, in which case
  the other leg closes with it.
- **Churn**, positions held for under an hour, pays the profit fee on its expected gain over its
  hold, at the hour's volatility by the Parkinson estimator, which reads volatility from each bar's
  high-low range. Churn's mean open net notional over the hour competes for room under the caps and
  takes the hour's move from its open, but not a gap at the reopening. Churning traders fund what
  they owe.

### Admission of orders

Every opening order, whether directional, paired or churn, must be one that the engine would fill.
The engine refuses an order whose fill would take the house's net exposure further past a cap,
whatever flows around it. It fills an order that shrinks the exposure, whatever its size
(`check_risk`). Each side of each class therefore keeps only the share of its margin in orders small
enough for its side's room, taken from papertrade.xyz's size profile on the market's margin
schedule:

- the side that grows the exposure may use up to the room under the market and total caps;
- the side that shrinks it may use up to that room plus twice the exposure.

A pair's two accounts open first the leg that fits. With no room, as at zero NAV, a zero floor and
no exposure, nothing opens. Of the orders that fit, the side that reduces the house's net exposure
fills in full, and the crowded side fills up to the cap. A pair leaves the house's net exposure
unchanged, but the engine checks each leg in turn, and the second leg returns the exposure to where
the pair found it. Once the exposure is past a cap, as it is when NAV has fallen, no pair opens,
whichever leg opens first.

### Engine rules

`model/accounting.py` reproduces each rule below against the engine function it mirrors, and the
tests check them on cases taken from the engine's own tests:

- a 10% profit fee on realised gains, and nothing on losses (`profit_fee`);
- financing at each 22:00 rollover on the notional of every open position at the market price. A
  long position pays the benchmark rate plus the markup, and a short position pays the markup less
  the benchmark rate, at the genesis markups and day-count bases (`daily_financing`);
- a close-out when equity falls below 50% of margin, only while the market is open. In continuous
  trading the close-out fills at the trigger, and when the market gaps through the trigger, at the
  bar's open (`closeout.rs`);
- negative balance protection, under which a loss beyond the account's balance, after the financing
  it has paid or received, is bad debt borne by the house, with each account of a pair settled on
  its own (`settle_bad_debt`);
- settlement in US dollars. USD-JPY and Japan 225 accrue profit and loss in yen, Germany 40 in euros
  and UK 100 in pounds. Each converts it at the engine's own USD-JPY, EUR-USD or GBP-USD price, from
  the same weeks of history, so its margin, close-outs, caps and NAV are in US dollars;
- each cap is the larger of its genesis share of NAV (`house_cap_bps`, `house_total_cap_bps`) and
  its floor, and an order that would take the house's net exposure past a cap is refused
  (`check_risk`).

### Vault and withdrawals

Proceeds from closing a position go to the client's free balance. New margin is drawn from free
balances first, and the rest is deposited. Half of each hour's remaining proceeds is withdrawn. Free
balances leave at 5% a day, or at 55% a day while NAV is below zero, which models a run on a public
log. Both rates compound hourly. A withdrawal the vault cannot pay joins the vault's first-in,
first-out queue and is paid as deposits arrive.

### MONKEY

MONKEY emissions move no house cash, so the model leaves them out and reports the realised losses
they are shared on. The incentive they create enters through the hedged flow, which the farming
share scales. A half-life on hedged flow is one of the sensitivities.

### Calibration of the floors

Each market run alone, at its share of the flow, gives each measure as a function of its floor over
18 floors from $0 to $100M. A risk level q sets every market's floor to the largest floor at which
that market alone stays at or below q, with every smaller floor also at or below q. That floor is
capped at the floor where the market's refusals end, which is where it refuses under 1% of its
directional notional over 90 days. It is zero when even a zero floor exceeds q. The total floor is
the sum of the market floors.

A market run alone cannot pool its fees with the other markets' fees, so a second family of
candidates puts the same floor on every market: $100k, $300k, $1M, $3M and $10M. All markets
together then run every candidate on the same 2,000 paths: twelve risk levels for each measure and
the five uniform floors. For each measure and target, the recommendation is the candidate that meets
the target and refuses the least directional notional over the 90 days. At the choice for 1%, a
second run sets the total floor to 25% to 100% of the sum of the market floors, where the total cap
starts to bind.

### Sensitivities

Thirteen variants start from the floors the house-cash measure chooses at 1%:

- no hedged flow, and twice the hedged flow, with directional flow unchanged in both;
- hedged flow that halves every 30 days, as farming would if the MONKEY reward halved;
- pairs held through closures;
- volatility 1.5 and 2 times;
- leverage mapped relative to each market's limit;
- markets bootstrapped independently;
- one client long share for every market;
- flow 0.25 and 4 times;
- exits before closures;
- history with its drift kept.

In the closure variant, farmers open pairs at the last open hour before each closure of 3 hours or
more. The pairs are at the maximum leverage, in orders that fit, and worth 10% of the margin the
market accepted over about the past week in hedged pairs held for an hour or more. Hedged churn is
left out, because its margin turns over within the hour. The pairs close at the reopening, where a
gap past a leg's balance is bad debt. Each variant runs with every floor scaled together by 0.1,
0.3 and 1.

### Spreads and the holding-time fee

By default the model charges no spread and the standard 10% profit fee. Three options on `Scenario`
change that, and the [spread-calibration study](../spread-calibration/README.md) uses them. No stage
of this study sets them.

- `Scenario.spread_bps` charges each fill a spread for each market, which the house keeps as
  `spread_income`, and charges both fills when the position opens.
- `Scenario.spread_bands` instead prices each fill through its market's size bands on its own
  notional, as the engine does (`accounting.staircase_rate`). In each hour, the orders that fit
  under a side's room pay their own margin-weighted mean spread (`flow.spread_table`, `order_fits`),
  so a small cap fills only the small orders, at the first band. A position held for an hour or more
  pays its closing spread when it exits, out of what it leaves its client and never more, as the
  engine takes the spread from the price of the closing fill. The fee is charged on profit less the
  mean over every order, which can only lower it.
- `Scenario.fee_schedule` replaces the flat 10% with a fee that depends on the hold
  (`accounting.HoldFee`). It takes either the engine's table, `max(base x (1 - discount), C(hold))`,
  or the spread-calibration study's log-linear family.

The engine charges the fee on realised profit at the spread-adjusted fill prices, so with either of
the last two options set, the fee is on the gain after both fills' spreads. A position held for an
hour or more pays the fee on its realised profit less the spread. Churn pays it on its expected gain
over its class's holds, `fee(h) * sigma * sqrt(h) * psi(2 s / (sigma * sqrt(h)))` (`holdfee.py`).
The derived flow records two moments of each churn class's holds, not their distribution.
`Scenario.churn_holds` therefore takes either the least fee that any distribution with those moments
allows (`bound`, the default), or the measured holding profile within the hour, tilted to those
moments (`central`). Runs record both (`churn_fee`, `churn_fee_central`), and the fee at the mean
hold on the gain before spreads (`churn_fee_gross`). A re-run adds `spread_income_mean` (zero
without spreads) and `filled_mean` columns to the results, and these fields to the manifests, which
the committed files do not have.

### The stress-trigger simulation

Each market's volatility per second comes from its hourly bars' Parkinson range. The stage simulates
one path at minute scale for each sampled hour of history, 400 hours per market, at the engine's
one-second cadence, with its bucketed window and its cooldown. That gives each candidate's share of
time at each level, and the volatility percentile of the hours in which it fires. Each simulated
hour opens with two window lengths of warm-up, which the tables discard, so every recorded second
reads against a full window. The cooldown is evaluated within each hour, and no state crosses from
one independent path to another. The stage reads the cooldown and the window from the genesis it
calibrates, so a change to either cannot leave the tables describing a different engine.

For utilisation, the model records the book that each hour leaves behind, after its settlements and
its openings. Those positions stay outstanding until the next hour, which makes the book the closest
hourly analogue of the engine's re-evaluation after each command.

### The toxicity calibration

The stage runs no Monte Carlo over price paths, because the engine's notional-weighted recurrence is
exact. It checks the one probability bound with 40,000 simulated trials of that recurrence, which
take about 5 seconds.

- **Fill sizes** (`size_quantiles`). The stage inverts papertrade.xyz's margin-weighted distribution
  of position notional on each market's margin schedule at the median and the 75th, 90th and 99th
  percentiles. A position opens as one order, so a fill is a position's notional. The sizes vary by
  schedule: a $100k median and a $3.2M 99th percentile on the crypto and share schedules, a $1.3M
  median and a $10M 99th percentile on FX, and a $1M median and a $10M 99th percentile on oil.
- **Markout scales** (`sigma_markout_bps`). The scale is the median open hour's Parkinson
  volatility, which is the model's hourly scale, scaled to the 2-second horizon, in bps. A typical
  hour prices a typical fill: 0.13 to 0.47 bps on FX and indices, 0.70 to 1.33 bps on crypto, and up
  to 1.58 bps on the noisiest shares. `results/toxicity_markets.csv` has every market.
- **Memory.** The score is a weighted average of the last `memory` dollars of fills. For fills of
  size n, its sampling drift is therefore about `sigma_markout * sqrt(n / memory)` bps. At the
  observed sizes, that drift stays under 1 bp unless single fills exceed about $5M at a memory of
  $10M. The chosen memory is the smallest that keeps the drift under 1 bp at every market's
  99th-percentile fill: $11.26M, set by Brent, which has a $10M 99th-percentile fill at a markout
  scale of 1.06 bps.
- **Prior.** The prior is half the memory. That is enough neutral history for a new account's first
  fills to move its score only modestly, while a toxic account still reaches its tier well inside
  the candidate fill counts. A prior of $10k, the development configuration's value, lets a single
  $1.3M fill set the score. The calibrated prior caps a first fill's weight at 1.33/(5.63 + 1.33),
  about 19%, on the largest median fill.
- **Tiers and spreads** (`choose_tiers`). The design toxic level is a sustained markout of 2 bps on
  every fill, which is above the widest size spread, so an account at that level profits from the
  house at any size. Tier 1 is the highest whole-bp threshold that meets two conditions. First, a
  toxic account filling at the $100k median reaches it within the stated number of fills. Second, a
  benign account filling at a market's median size stays below it on each market for 200 fills with
  a probability of at least 99.9%, in 40,000 trials of the exact recurrence. The benign account's
  fills are at least one horizon, 2 seconds, apart, so that their markouts do not overlap. The first
  count that passes is 75 fills, so a toxic account reaches tier 1 within 75 fills of the median
  size. Tier 2 doubles the threshold. A tier's spread is the sustained markout that reaches the tier
  in the stated number of fills, so that the house recovers the marginal toxic account's edge. It is
  rounded up to the whole bps the engine stores: 1.8 bps becomes a spread of 2 bps, and 2.6 bps
  becomes 3.

Fills closer together than the horizon share price moves, and the bound does not cover them. The
burst sensitivity in `results/toxicity_choice.json` gives the probability that a burst of 25 fills
crosses tier 1, at one-second and half-second intervals. On the noisiest median fill, which is
Brent's, it is about 0.5% and 3.9% at the chosen tier. Shorter intervals make crossings more likely:
an account placing $1M fills every half-second, as a market maker might, reaches the tier in about
one burst in twenty-five.

The benign bound rests on the median fill, not the tail, by design. An account that fills at the
99th-percentile size every time is already an outlier. The memory bound puts that account's noise at
exactly the drift bound, so no threshold within the toxic account's reach could hold against it.
Such accounts are what the score is for, not its false positives.

### The retained-NAV scenario

`model/retained.py` reuses the floor study's data and simulation with a different scenario. The
house opens each 90-day run with the candidate floor as its capital and sweeps everything above it
every hour, so fees never build a cushion. The exposure caps are the genesis shares of NAV, with no
dollar floors, so the floor is also what the caps are a share of. Markets beyond the launch set are
clones of equity markets, each correlated 0.7 with the market it copies and as busy as that market
(`model/expansion.py`). For each market count and variant, the floor rises along a geometric grid,
with a ratio of 1.6 from $4M to $4.6B, on the same paths. The dollar-cap variants use a grid from
$0 to $64M.

## Data and sources

`results/report.md` tabulates the derived data in full: the papertrade.xyz summary and daily flow,
and each launch market's statistics.

### papertrade.xyz trader behaviour

papertrade.xyz runs a perpetual futures venue on Hyperliquid's EVM (HyperEVM, chain 999), where a
liquidity pool that starts at zero takes the other side of every trade. Losing traders pay nothing
beyond their loss. Winning traders pay a 2% fee after an impact haircut that keeps at most 90% of a
gain. Losses mint the PAPER token, at 100 tokens per $1 while the pool holds less than $2M. The
venue lists BTC and ETH at up to 1000:1, with isolated margin and a $10M position cap
([docs](https://docs.papertrade.xyz),
[contract addresses](https://docs.papertrade.xyz/dev/contract-addresses/),
[impact haircut](https://docs.papertrade.xyz/how/asymmetric-impact/),
[mint curve](https://docs.papertrade.xyz/how/mint-curve/)).

`fetch/papertrade.py` pulls every event log of the Exchange proxy
`0x6cd5661646289fb6e65ea5c032310fded797d0a2`, from its creation block 47,854,719 to block
49,109,718 (2026-10-21 05:42 UTC). It reads them from six public HyperEVM RPC endpoints in
1,255 chunks of 1,000 blocks, with block timestamps every 10,000 blocks. 25 chunks chosen at random,
fetched again from an endpoint chosen at random, matched their stored log counts. The raw pull is
212 MB and is not committed.

The Exchange's source is not verified on the block explorer, so `derive/papertrade.py` infers the
event layouts from the logs and checks them:

- every one of the 1,713,192 closing events, of which 7.76% are liquidations, matches one of the
  1,715,360 open events;
- over the 1,374,753 close events with a non-zero profit after the haircut, the median ratio of the
  fee to that profit is 0.0200, the documented 2% fee on wins and on the pool's side of losses;
- BTC entry prices sit within 0.1% (median) of Binance's BTCUSDT close for the same hour;
- deposits of $351.2M less withdrawals of $305.9M give $45.3M, against the $45.36M of USDC that the
  explorer showed the Exchange holding on 2026-10-21.

The contract's open-interest event is not used. Its changes do not follow the flow of positions, and
until block 48,217,087 it carried a fixed offset of about $5B on each side.

Trading opened at 2026-10-10 14:00 UTC, so the data covers 10.6 days, 18,504 accounts and $713B of
notional. The study counts both legs of a hedge as hedged, for the notional they offset, in two
cases:

- Within an account, a position is hedged for the part that offsets the account's open opposite
  notional in the same asset, not already offset, matched against the earliest opposite positions
  first. When a position closes, what it offset can be offset again.
- Across accounts, two opposite positions of the same notional are hedged when they open and close
  within two blocks of each other and neither is hedged within its own account.

A position that is partly hedged counts in both kinds of flow by its shares.

The derived dataset in `data/derived/` holds the flow profile the model samples
(`papertrade_flow.csv`, margin by kind, hold class and leverage bin), the size profile
(`papertrade_sizes.csv`), the leverage profile (`papertrade_leverage.csv`), the daily series
(`papertrade_daily.csv`), the holding-time table (`papertrade_holding.csv`) and the summary
(`papertrade_summary.json`). The size profile gives, for each kind and launch margin schedule, the
margin-weighted distribution of each position's notional on that schedule under either leverage
mapping. The leverage profile gives the notional that each leverage bin's margin buys on each
schedule. The flow, size and leverage profiles and the fit of the long share count the closed
positions opened on the ten full days that also set the daily path of margin, 1,608,754 positions.
They leave out the partial first and last days, and the 2,168 positions still open at the end, which
include the longest holds.

What the data shows about hedged flow and holds:

- Hedged flow is 41% of all notional and 67% of exposure-time. The launch afternoon was the most
  hedged, and on the ten full days hedged flow is 24% of notional and 41% of margin, which is what
  the model uses. Within accounts, 60% of gross exposure-time is offset by the same account's
  opposite positions, and across the market 87%.
- Holds are short. The median position lasts 64 seconds, but 71% of exposure-time comes from
  positions held for over an hour, and 9% from positions held for over a day.
- Hedged flow pays $0.235 of haircut and fee on its winning legs for every $1 lost on its losing
  legs. On this engine a winning leg pays a 10% profit fee and no haircut, so the same farming costs
  about $0.10 per $1 of loss, and the house takes less than half of what papertrade.xyz takes from
  it.
- Flow fell from $373M of margin on the first full day to $79M on the last.

### Price history of the markets

The committed results cover 37 markets: the 34 that the launch genesis lists, and DE40, UK100 and
Brent, which are not listed at launch. `data/derived/` keeps the bars and statistics of all 37.
`fetch/markets.py` pulls the following for each:

| Data | Source |
|---|---|
| Hourly OHLC, 2024-10-28 to 2026-10-12 (102 weeks) | Crypto: Binance spot klines (`data-api.binance.vision`). Everything else: Yahoo Finance chart API (`query1.finance.yahoo.com/v8/finance/chart`), which serves hourly bars for the last 730 days. |
| Daily closes from 2016 | FRED (St. Louis Fed) where an official series exists: `DEXUSEU`, `DEXUSUK`, `DEXJPUS`, `DEXUSAL`, `SP500`, `NASDAQ100`, `DJIA`, `NIKKEI225`, `DCOILWTICO`, `DCOILBRENTEU`. Otherwise Binance daily klines (from August 2017; SOL from August 2020) or Yahoo daily bars, dividend-adjusted for shares. |
| Benchmark rates | FRED `SOFR` (3.87%), `IUDSOIA` (SONIA, 3.73%), `ECBESTRVOLWGTTRMDMNRT` (ESTR, 2.44%), each on 2026-10-19 or the latest day before, read into `benchmarks.csv`; TONA 0.75% and AONIA 3.60% are assumptions. The model charges financing at these rates. |
| Market mix | Hyperliquid's public info API (`api.hyperliquid.xyz/info`, daily candles 2026-09-20 to 2026-10-19): each market's mean daily notional on its Hyperliquid listing, on the main exchange or the builder-deployed `xyz` exchange. |

`catalogue.py` names each market's stand-in series:

- Binance BTCUSDT, ETHUSDT and SOLUSDT for the crypto markets, and spot rates for FX;
- the CME E-mini S&P 500, Nasdaq-100 and Dow front months for US500, US100 and US30, as the venue
  calendars use, and CME's Nikkei yen future for JP225;
- the COMEX gold and silver and NYMEX WTI and Brent front months;
- the shares' regular sessions;
- the DAX and FTSE 100 cash indices for DE40 and UK100, because their venues' futures are not
  public.

`derive/markets.py` cleans the hourly bars:

- A gap across a one-hour step is set to zero. Around a roll, the public futures series alternates
  between the front-month and next-month contracts, so consecutive bars jump by 80 to 170 bps and
  back. A continuously traded market's next bar opens at the previous close.
- For crypto, FX and futures, a gap after a hole of 3 to 24 hours is set to zero unless the hole
  touches a US market holiday or early close, or the day on either side of one. At the other holes
  the futures series switches contract, and ES jumped 2.6% across one on 2024-12-20. Gaps over the
  daily break, weekends, holiday halts and holidays are kept.
- DE40 and UK100 fold each weekday's overnight gap into continuous movement, since their venues'
  futures trade 20 or more hours a day. They keep weekend gaps, taken to the first bar's close,
  because the FTSE series opens at the previous close.

`market_stats.csv` gives each market's volatility, the number of reopening gaps a year that pass
half its first margin band, where a position at the maximum leverage is closed out, and that pass
the whole band, where the position loses more than its balance. It also gives the largest hourly and
daily moves, over the hourly window and over the daily history from 2016.

For the ten markets with an official FRED series, `market_stats.csv` also compares the hourly data's
week-on-week returns, at Friday's last bar, with the FRED series. The correlation is 0.978 for
US500, 0.981 for US100, 0.943 for US30, 0.937 for WTI, and 0.935 to 0.953 for the FX pairs, whose
FRED rates are the noon fixings in New York. JP225 (0.803) and Brent (0.816) are compared with the
Tokyo close and the London Brent spot assessment. The ratios of volatility are 0.94 to 1.13. The
hourly window is volatile for metals and several shares, with gold at 26% a year against 17% since
2016 and silver at 57% against 35%. It is calm for crypto, with BTC at 44% against 67%.

## Limits

- **The 100:1 flow is approximated.** BTC-USD's and ETH-USD's bands have no profile in the derived
  papertrade.xyz data. Their size and leverage profiles are scaled from the oil schedule, which has
  the same first-band rate (`flow.with_genesis_profiles`). Every stage's notional, fees and exposure
  on the two markets, and the floors they set, rest on that scaling. papertrade.xyz's traders chose
  their leverage at 1000:1, and the sensitivity row for leverage mapped relative to each limit
  bounds how much of that choice carries over.
- **Ten days of behaviour.** The papertrade.xyz data covers 10.6 days of a new venue, with two
  markets at 1000:1, traders native to crypto, and a spike at launch. Holds, leverage and long share
  on the other markets, at lower leverage and with closures, are assumptions carried over from it.
- **Fixed holds.** Each hold class settles at one hold, of 1, 4, 16 or 48 hours. papertrade.xyz's
  notional-weighted mean holds within the classes are 1.4, 3.9, 13.8 and 54.3 hours.
- **Volume.** The level of flow assumes that this engine draws papertrade.xyz's margin. Fees scale
  with it, and the rows for flow at 0.25 and 4 times bound the effect.
- **Two years of hourly history and 2,000 paths.** The tails come from 102 weeks, from late 2024 to
  late 2026, which include April 2025 and March 2026 but not March 2020, and the volatility rows
  stress them. 2,000 paths measure a 1% rate to within about half a point, but cannot tell 0.1% from
  0.01%, because a run with no failing path bounds the rate only below 0.15%.
- **Price stand-ins.** DE40 and UK100 use cash indices. Futures series carry roll artefacts that the
  cleaning removes only at holes and one-hour steps. A roll that coincides with a weekend stays in
  that weekend's gap, which overstates the gaps of WTI and Brent. Yahoo's hourly bars are a public
  but unofficial source. The crypto series are Binance spot, and the study does not repeat its
  results with prices from Coinbase and Kraken alone.
- **Engine detail left out.** Size spreads, stress spreads and toxicity spreads would add income for
  the house, so leaving them out is conservative. The model also leaves out:
  - margin shared across an account's markets, because each position is its own account;
  - the higher close-out level of positions that extend past the first margin band. The bands set
    how much notional margin buys, but the close-out level stays at half the first band's rate,
    which is below the engine's for orders past the first band. Those positions therefore close out
    later and leave less balance against a gap;
  - close-outs triggered by financing. Financing changes what an account can lose, but not its
    trigger;
  - the rule that leaves a market without a price when its venues disagree, and the rule that leaves
    a market without a price while the currency it settles through has none.
- **Order sequencing and size.** Within an hour the model nets the orders that fit against the
  exposure at the hour's start. It therefore assumes that the orders that reduce exposure arrive
  spread among the crowded ones. When the room is small next to the orders, the engine checks them
  one by one and refuses more. Orders keep papertrade.xyz's sizes, so a trader who splits an order
  under a small cap fills more of it than the model gives.
- **Close-out fills.** In continuous trading a close-out fills at its trigger. When a crash inside
  an hour makes the venue index jump across the trigger, the engine fills worse, so crypto bad debt
  is understated. For the same reason the retained NAV says nothing about the safety of 100:1
  leverage.
- **Withdrawals.** The rates of withdrawal and of a run are assumptions. The queue measure depends
  on them, and the house-cash measure does not.
- **Markets in the committed results.** The committed results cover 37 markets, including DE40,
  UK100 and Brent. `run.py` drops the markets that the genesis does not list, so a re-run on the
  launch genesis moves several figures that this page cites:
  - The per-market stage prices each market alone, so it does not change.
  - The joint, sensitivity and retained stages have not been run on 34 markets. Their totals scale
    with the market count: at $1M a market, the total is $34M rather than $37M. On a re-run, the
    retained stage makes up its 37-market count from the 34 launch markets and three clones.
  - Run on the launch genesis, `python -I run.py stress` keeps every chosen value: the 60-second
    window, moves of 75 and 200 bps, a divergence share of 6001, and utilisation level 2 at 9500.
    The flow-weighted utilisation fire rates rise by up to 1.5 points. On a uniform $10M they go
    from 29.4% to 30.5% at 7000 bps and from 12.7% to 13.2% at 9000 bps, and still no level below
    9500 passes.
  - Run on the launch genesis, `python -I run.py toxicity` gives the values listed under
    [toxicity parameters](#toxicity-parameters).
- **The toxicity trials.** The trials price markouts at the volatility of the median hour, while
  live fills cluster in livelier hours, which raises crossings. The bound for each market combines
  across markets only through a union bound, so an account trading many markets at once is bounded
  market by market, not jointly. Both assumptions need checking against live fill data.
- **Zero capital at launch.** The genesis starts the house with no capital. A liquidity floor below
  the opening NAV is swept by the first staker, so the floor must be at or above the opening NAV.
  From zero capital nothing is swept until fees have built NAV up to the floor. Until then the house
  carries the early-dip risk that the floor study shows at zero capital.

## Re-running

From a virtualenv outside the repository (Python 3.12 or newer):

```sh
python3 -m venv /tmp/cap-venv && /tmp/cap-venv/bin/pip install -r research/cap-calibration/requirements.txt
cd research/cap-calibration
/tmp/cap-venv/bin/python -I -m pytest tests     # the accounting tests
/tmp/cap-venv/bin/python -I run.py all          # per-market, joint, sensitivity, then results/report.md
```

`all` runs the floor study again from the committed `data/derived/`, with the committed results'
settings. `DEFAULTS` in `run.py` sets each stage's paths and paths per batch, and `--workers`
defaults to 4. Each stage also runs alone, as `per-market`, `joint`, `sensitivity` or `report`. The
stress triggers run as `stress`, the toxicity parameters as `toxicity`, and the retained NAV as
`retained`. None of those three is part of `all`, and they follow the same seeding and manifest
conventions.

Each batch of paths is seeded from `--seed`, which defaults to 1, and from its index. The batch size
shapes the random draws, so the same seed and batch size reproduce a run exactly, and another batch
size gives different, equally valid paths. `results/manifest_<stage>.json` records the scenario,
including both values, the hashes of the derived files the stage read, and the run time. The
`stress` stage's manifest also records the stage's own settings: the effective scenario, the hours
of the move tables, and the floor families. The move tables sample 400 hours of history per market
at the same seed.

To rebuild the data, run `fetch-papertrade` (which resumes, and whose `--to-block` pins the end),
`fetch-markets`, `derive-papertrade` and `derive-markets`. Yahoo serves hourly bars for the last
730 days only, so a later rebuild needs a later window in `fetch/markets.py`.

To recalibrate with live data:

- Replace papertrade.xyz's flow with this engine's own. The public log records every order and
  execution, from which the same flow profile follows: margin by hold class and leverage, long
  share, and hedged share. The market mix becomes the engine's own volume.
- Move the price window forward, and widen it as hourly history allows.
- Take the benchmark rates from the signed rates the engine applies.
- Run `all` again before changing the floors.
- Run `stress` again after any change to a floor, a market or a bound. The utilisation levels' fire
  rates move with the caps, and the move thresholds move with each market's volatility.
