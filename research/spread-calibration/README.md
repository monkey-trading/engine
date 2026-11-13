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

The retained NAV stays at $107M on both flows: the exposure caps' refusals set it, as before. Over
90 days current flow pays $456M of spread income and $55M of profit fees ($30M of it from churn),
target flow $135M and $36M.

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

The figures in the sections after the next are before a correction to the house income model,
which charged the churn profit fee on the price gain at each cohort's mean hold and added both
fills' spread income beside it. The engine charges the fee on realised PnL after the spread, so the
model overstated house income and the operator's honest share. The next section makes the
correction and restates the headline at the owner's values (#697).

## The corrected headline at the owner's values (#697)

**Bottom line.** On the corrected income model, bounded below on the fee as the next paragraph
describes, and on the genesis's 34 launch markets (#685 deferred DE40, UK100 and Brent), the
operator's honest share is $1.14M a day on current flow and $385k on target flow ($1.17M and $389k
on the central estimate), against $1.77M and $483k before the correction at #623's proposals on 37
markets. At the owner's values every attack class together takes about $640k a day on current flow
and $528k on target flow, so the launch criterion holds on current flow and fails on target flow.
Each class alone is below either share. For the criterion to hold on target flow, the loss-rate
trigger (#695) must hold the classes the house pays, which are the ones it can see (latency
arbitrage, unpriced pairs and the held sync's kill option), to **at most $423k a day of house
loss**, against $601k unbounded: a cut of at least 30%. On current flow their budget is $1.22M a
day, which they are within. The trigger must also bound two things the daily figures leave out:
unpriced pairs on 2025-10-10's four stress hours, $24.4M to the operator in one day, and the fastest
honest client's $102k a day, which #614's decision D9 sets at zero. Raising the profit fee's base,
which the bounds allow up to 15%, does not move the operator's latency take; holding the full fee to
10 s instead of 1 s halves it and brings target flow within the criterion before the trigger (the
proposal below). The tables are in `results/owner_values.md` and the budgets in
`results/owner_criterion.json`.

**The correction.** #623's house model charged churn (flow held under an hour) the profit fee on its
gross price gain at each class's mean hold, and added both fills' spreads beside it. The engine
charges the fee on realised PnL at the spread-adjusted fill prices (`execution/mod.rs`, `book`), so
a winning short hold pays the spread out of its gain. The cap study's model now charges a churn
class `fee(h) * sigma * sqrt(h) * psi(2 s / (sigma * sqrt(h)))`, `psi(u) = E[max(Z - u, 0)]`, over
its holds, and a position held an hour or more the fee on its realised PnL less the spread
(`research/cap-calibration/model/holdfee.py`). The fee is #614's decision D7 on the engine's table
of D3's schedule (eleven points, linear, rounded up to the bp, at most 0.81 points above the
log-linear curve). The derived flow records each churn class's mean hold and mean square-root hold,
not the distribution, so the integral is bounded: the share takes the least fee any distribution of
holds with those two moments allows, a linear programme whose optimum sits on at most three holds,
and every account pays tier 3's fee, the least any tier pays past the curve. The central estimate
tilts the measured within-hour holding profile to each class's moments instead. #614's decision D5
is not modelled: every account trades at the market's own leverage, as tiers 2 and 3 do. An account
below tier 2 pays a higher fee but holds less notional where the market allows more than 20x (tier
0) or 50x (tier 1), and the flow data does not say how volume spreads across tiers; tier 2 takes
$25M of counted volume over five days or more. At the owner's values, charging churn as #623 did
overstates the share by $103k a day on current flow and $13k on target flow against the lower bound,
and by $73k and $10k against the central estimate.

| A day | Current flow | Target flow |
|---|---|---|
| Honest share, lower bound | $1.14M | $385k |
| Honest share, central estimate | $1.17M | $389k |
| Honest share, no tier discount | $1.16M | $405k |
| #623 at its proposal, before the correction (37 markets) | $1.77M | $483k |
| Latency arbitrage, crypto, the operator's 3 s view | $265k | $265k |
| Latency arbitrage, FX and indices at D6's tenths | $197k | $197k |
| Unpriced pairs at 1% margin, tier 3's 8% fee | $17k | $17k |
| Skimming client fills | $159k | $47k |
| Held sync's kill option | $2k | $2k |
| **All together** | **$640k** | **$528k** |
| Fastest honest client, 0.25 s | $102k | $102k |

The retained NAV stays at $107M on both flows: the exposure caps' refusals set it, as before. Over
90 days current flow pays $474M of spread income and $57M of profit fees ($31M of it from churn),
target flow $141M and $38M.

**FX and index first tiers (D6).** Under the owner's fee, latency arbitrage on the launch genesis's
eight FX and index markets takes $246k a day of house loss at D6's tenths, $161k at 1 bp and $553k
at the owner's reference tiers, measured at each market's exact spread as the crypto takes are (size
tiers, the stick and recycling); #623's upper bound (the whole position at the first tier, no stick)
gives $676k, $456k and $1.17M, and on that bound all the classes together take $984k a day on
current flow, below its share, and $872k on target flow. These are several times #623's, which held
the full fee to 5 minutes: the owner's 1 s schedule leaves short holds two-thirds of their gains.
Per market in `results/owner_values.md`. #623's zero on EUR-USD and GBP-USD at 1 bp came from its
interpolation, which read past its largest measured spread as zero; measured at the spread, each
takes about $1.6k a day under the owner's fee.

**Across the fee bases and discounts (D7).** The bounds admit a base from launch's 10% to 15% and
tier discounts no larger than launch's, ordered by tier. On every admitted combination, checked
every hundredth of a second to an hour, each tier's fee falls with the hold, a higher tier never
pays more than a lower one, and every tier pays at least its launch fee at every hold: the ordering
guarantee and the protection of the calibrated fee from below hold. A higher base raises the share
($1.16M and $405k at 12.5%, $1.18M and $425k at 15%), lowers the fastest client's take to $82k and
turns the pairs' year negative, but leaves the operator's latency take where it is: its best holds
are 3 to 5 s, where the curve sets the fee, and its best strategy recycles wallets at tier 0, where
no discount applies. The 5% cut the owner ruled out (#614, 2026-11-12) would put every tier up to 5
points under its launch fee from 15 minutes.

**Proposal for the owner.** The lever on the operator's latency take is the curve's short end, which
D7 lets move only upwards: holding the full fee to 10 s, still reaching 10% at 15 minutes, takes the
crypto take from $265k to $160k and the FX and index take from $197k to $63k, so all the classes
together fall to about $289k a day on target flow, within its share, while target flow pays nothing
more and holds under 15 minutes pay 44% of their gains instead of 33%. Holding it to 60 s gives the
same takes for 61%. It does not reach the fastest client, whose best holds are 15 to 30 minutes, nor
the stress day's pairs: the trigger still has to bound those.

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
python -I run.py owner_fee
python -I run.py owner_latency
python -I run.py owner_fx --workers 3
python -I run.py owner_unprice --workers 3
python -I run.py owner_income --workers 3 --paths 1000 --only "launch, bound"
python -I run.py owner_income --workers 3 --paths 500
python -I run.py owner_report
python -I -m pytest tests
```

`owner_income` keeps the variants already in `results/owner_income.json`, so the second command runs
the sensitivities at 500 paths beside the two 1,000-path headline runs. With `retained` and
`per_market`, it runs the cap study's corrected model: re-running those two stages gives #623's
proposal on the corrected model, not the committed figures, which are #623's record.

## Files

- `run.py`: the stages.
- `calib/`: `common.py` (the sibling studies and the genesis), `volatility.py` (lag moves),
  `spreads.py` (tiers), `latency.py` (the latency rule on a grid), `jump.py` (a jump stress
  trigger variant), `fee.py` (schedules, the hold clock, honest cost), `toxicity.py` (the score),
  `carrot.py` (volume tiers), `target.py` (target flow), `unprice.py` (#549's unpriced-spell
  measure at the calibrated spreads), `report.py`, `owner_report.py` (#697's results page and
  the trigger's budgets).
- `tests/`: the fee, hold clock, score, tiers, target flow, spreads and latency rule on hand cases,
  and D7's fee table, the tenths, the admitted discount tables and each tier's fee in the latency
  best response.
- `results/`: each stage's output and `report.md`; #697's `owner_*.json`, `owner_values.md` and
  `owner_criterion.json`.
