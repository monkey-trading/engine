# The house loss-rate trigger

## Summary

**The trigger proposed for #695.** Every market's stress level rises to 2 when the house's *marked*
loss to its clients over the trailing 24 hours, summed over every market, is above zero, and cools
down as the other triggers do. The marked loss is the realised loss to clients net of profit fees
and spreads, plus the change in clients' unrealised PnL: the house's NAV loss to its clients. A
threshold of zero needs no scaling to NAV or to the caps: the trigger fires when the house has lost
money to its clients over a day. At the owner's values (the full fee to 10 s, 1% margin on BTC-USD
and ETH-USD) honest target flow fires it in 0.87% of hours and holds level 2 for 0.65% of the time,
in about 74 periods a year averaging 46 minutes; honest current flow fires it in 0.01% of hours.

**What it bounds.** While the level is down, the house cannot have lost money to its clients over
the day, so an attacker takes at most the day's honest income plus the one command that crosses
zero: on target flow $2.18M a day on average, $3.18M at the 95th percentile and $4.41M on the worst
day of 102 weeks. The trigger caps how fast the house can lose; it does not cap what an attacker can
take from the house's income.

**#614's decision D9 is not met, and no loss-rate threshold meets it without firing almost
always.** At the 10 s curve:

| A day | Without the trigger | Under it |
|---|---|---|
| Latency arbitrage, crypto, the operator's 3 s view | $160k | $158k |
| Latency arbitrage, FX and indices at D6's tenths | $63k | $63k |
| Unpriced pairs at 1% margin, ordinary days | $17k | at most $17k |
| Held sync's kill option | $2k | at most $2k |
| Skimming client fills, target flow (the trigger cannot see it) | $47k | $47k |
| **All together, target flow** | **$289k** | **$287k** |
| Honest share, target flow, at the 10 s curve | $386k | $386k |
| Fastest honest client (D9: zero) | $102k | $101k |
| Unpriced pairs over 2025-10-10's four stress hours, house loss | $18.0M | $10.4M |

- **The launch criterion holds on ordinary days at the 10 s curve, before the trigger:** every
  class together takes $289k a day against a $386k share on target flow, and $402k against $1.15M on
  current flow. The trigger must hold the classes it sees to $424k a day of house loss on target
  flow; unbounded they take $303k.
- **The fastest honest client keeps $101k a day.** Its 22 trades a day on the crypto markets each
  add about $3.6k to a reading that honest flow moves by hundreds of thousands an hour, and it sits out
  the 0.65% of the time the level is raised. Nothing short of level 2 almost all the time reaches it.
- **Paired positions opened ahead keep $10.4M of the stress day's $18.0M.** Once a pair is open,
  level 2 raises only its closes, from the 6 to 8 bps a 1.4% move already pays at level 1 to 18 to
  22 bps, and a pair opened at level 0 still pays from a 1.31% move on BTC-USD (1.37% on ETH-USD),
  which 2025-10-10 offered again and again. Each pair opened ahead is $1M of margin over two accounts
  that have earned tier 2; the day takes 100 of them for $10.4M. Every candidate tried, thresholds
  firing in up to half of all hours included, leaves them $6.7M or more.

**Decisions for the owner:**

1. **The trigger:** a day's marked loss above zero (proposed), or the faster 60-minute alternative at
   22.5 bps of the total exposure cap ($483k at the retained NAV), which answers a sudden drain within
   the hour but leaves an attacker $13.8M a day.
2. **The fastest honest client:** D9's zero is not reachable by the trigger. On its upper bound it
   takes about 20 bps more on every fill, or a fee near 95% at every hold (85% of the target client's
   gross profit), to bring it to zero. Accepting its $101k a day is the alternative.
3. **The stress day's pairs:** the trigger halves them. At 2% margin on BTC-USD and ETH-USD (#614,
   D4's other option) they take $0.32M of house loss on the stress day without the trigger; at 1%,
   each pair an operator opens ahead ($1M of margin) takes $104k on average under the trigger and
   $132k without it.
4. **#697's stress-day figure:** its $30.6M of house loss ($24.4M to the operator) charged each
   raised level's minimum spread but not its size multiplier, and let a spell count a move from before
   the market's last repricing. With the engine's level spreads and quote order, #549's operator,
   who reopens one $50M pair before each spell, loses $0.58M on the stress day; only pairs opened
   ahead pay. #500's re-run of the pairs should charge the engine's level spreads.

## The question

#614 names a cap on how fast the house can lose; #549's proposal 3 sketched it as a stress trigger
on the house's realised loss over a window and left its threshold uncalibrated. The owner's answer
to #614's decision D9 is that the trigger brings the fastest honest client's take to zero or less
and holds the operator's take below its honest share; decision D4 accepts paired positions at 1%
margin on BTC-USD and ETH-USD with this trigger as their bound, 2025-10-10's stress hours included.
#698 asks, at #697's corrected pricing and the owner's values:

- honest flow's loss over windows of one to sixty minutes, per market and house-wide;
- the measure (house-wide or per market, against NAV or the cap), and a threshold that fires on
  honest flow in at most a stated share of hours;
- under it, the most the house can lose in a window, the operator's latency take on crypto and on FX
  and indices, paired positions at 1% margin with the stress day, and the fastest honest client;
- if no threshold meets D9 without firing often on honest flow, the trade-off for the owner.

The owner's values are those of #614's comment of 2026-11-13: the full fee held to 10 s, falling to
10% at 15 minutes (#697's 10 s variant); markout-spread floors at the year's markout spread (#718);
BTC-USD and ETH-USD at 1% margin. This study changes no genesis value.

## Results

All figures are at the owner's values: D7's fee on the engine's table with the full fee to 10 s and
10% at 15 minutes, tier 3's discount past the curve for honest flow and pairs, the genesis's spread
bands in tenths of a bp (`V52`), and BTC-USD and ETH-USD at 1% margin. `results/report.md` has every
table; the tables below come from it.

### Honest flow's loss over one to sixty minutes

House-wide, the house earns $6.81M a day from honest current flow and $2.18M from target flow, net of
what it pays winners. Over short windows it still loses often: the highest marked reading in an
hour is above zero in more than half of all hours over windows of one to five minutes. The marked
loss's highest reading in each hour, 99th percentile:

| Window | Current flow | Target flow |
|---|---|---|
| 1 minute | $124k | $131k |
| 5 minutes | $214k | $248k |
| 15 minutes | $266k | $344k |
| 30 minutes | $273k | $418k |
| 60 minutes | $224k | $485k |
| 6 hours | -$734k | $628k |
| 24 hours | -$4.74M | -$74k |

Per market the loss follows the flow: BTC-USD's 60-minute threshold at 1% of hours is $263k and
ETH-USD's $175k on current flow, SOL-USD's $30k, US500's $38k, and the FX majors' a few thousand
dollars or less (`results/report.md`, "Per market"). The realised loss (no unrealised change) has
smaller tails over the long windows but the same order over the short ones.

Honest bad debt is rare. On the rogue study's recorded hours, honest flow leaves any bad debt in
0.08% of BTC-USD's hours and 1.4% of ETH-USD's, never more than $1k in an ordinary hour, and at most
$33k in one of 2025-10-10's four stress hours. On bridged bars the gap markets carry it (WTI, US100,
silver, US500 at reopenings): house-wide, more than $25k of bad debt in 60 minutes in 0.16% of hours.

### The measure

- **Marked, not realised.** A realised reading sees what the house pays when a client closes. An
  operator holding pairs across an unpriced move lets the engine close out the losing leg, which the
  house realises as a gain of the leg's balance, and keeps the winning leg open: on the stress day no
  realised threshold holds them, and they take $17.9M to $18.0M. The marked reading counts the
  winning leg's gain when the market prices again. The engine already computes house NAV from each
  market's client quantity and cost (`engine/house.rs`), so the reading is the fall in house NAV over
  the window, leaving out house capital credits and distributions, and needs no state beyond the
  window's buckets.
- **House-wide, not per market.** The markets' losses partly offset, so one house-wide threshold at a
  given fire rate leaves an attacker less room than the per-market thresholds together (on the stress
  day, $10.4M against $12.0M on target flow), while each market's honest level-2 time stays about
  the same. A per-market reading would catch an attack on one small market sooner, but no measured
  class needs it.
- **Against zero.** Honest flow's income and its loss's spread both grow with its volume, so
  whether the house lost money over a day does not change when the same flow grows or shrinks, and
  needs neither NAV nor the caps to scale it. It does depend on the flow's mix: target flow, with less
  short-hold income against the same long-hold exposure, fires it in 0.87% of hours, current flow in
  0.01%. A threshold against NAV or the total cap would have to be negative to fire in 1% of hours
  over a day on current flow (-$4.74M), and would then fire most of the time if the flow halved. The
  60-minute alternative needs a positive threshold, and 22.5 bps of the total exposure cap
  (`max(NAV x 200%, total floor)`) keeps it scaled to the house.
- **Bad debt as a reading of its own** was tried: at $25k over 60 minutes it fires in 0.16% of hours
  and leaves the stress day's pairs $8.2M, about what the marked reading leaves, since a pair opened
  ahead still pays at level 2.

### The threshold and the window

What a threshold at 1% of hours leaves an attacker a day falls as the window lengthens, since honest
flow's loss over a window grows more slowly than the windows a day shrink: on target flow $191M a day
at one minute, $35M at 15, $13.8M at 60, $4.7M at six hours and $2.1M at a day. The day's window at
zero is the tightest bound that fires in at most 1% of hours on both flows:

| Trigger | Flow | Threshold | Hours fired | Level 2 | Periods a year, mean length | Room a day, mean / p95 | 2025-10-10, pairs ahead |
|---|---|---|---|---|---|---|---|
| A day, zero | target | $0 | 0.87% | 0.65% | 74, 46 min | $2.18M / $3.18M | $10.4M |
| A day, zero | current | $0 | 0.01% | 0.00% | 1, 5 min | $6.81M / $8.57M | $13.1M |
| 60 minutes, 22.5 bps of the total cap | target | $483k | 1.00% | 0.36% | 177, 11 min | $13.8M / $14.8M | $9.2M |
| 60 minutes, 22.5 bps of the total cap | current | $483k | 0.32% | 0.09% | 53, 9 min | $18.4M / $20.2M | $11.4M |

The latency classes running beside honest flow add about $300k a day of house loss to the day's
reading on target flow, which would raise its fire rate to between 1% and 2% of hours.

### What the trigger does when it fires

It raises every market to level 2 (a 10 bps minimum and four times the size spread), where it stays
for the five-minute cooldown and until the day's marked loss is back below zero, as `stress.rs` steps
any level down. Level 1 (2 bps and twice the size spread) would do less: the latency classes do not
fire the trigger, and a pair opened ahead pays at level 1 from a 1.19% move on BTC-USD against 1.31%
at level 2. A close's spread stays clamped at its market's ceiling: a first-band close pays 18 bps at
level 2 on BTC-USD and SOL-USD and 22 bps on ETH-USD, under ceilings of 30, 35 and 35 bps, while a
large BTC-USD close is clamped: a $50M position averages 7.1 bps through its bands, 38 bps at level 2,
and closes at 30 bps.

### The bound on each attack class

- **The most the house can lose in a day:** its honest income for the day plus the one command that
  crosses zero, and what is lost at level 2 while the level holds. The largest single command on the
  stress day, a pair's repricing, loses $0.82M; a latency trade loses its $6.9M position times the
  move it catches. In the worst single day of 102 weeks the room is $4.41M on target flow and $10.9M
  on current flow.
- **Latency arbitrage, crypto, the operator's 3 s view:** $160k a day at the 10 s curve (its best
  holds are now 15 minutes, recycling wallets at tier 0); $158k under the trigger, which it sits out
  (`run.py recommend`, at the exact threshold on target flow).
- **Latency arbitrage, FX and indices at D6's tenths:** $63k a day, unchanged.
- **Unpriced pairs at 1% margin on ordinary days:** #697's $17k a day; the trigger can only lower it.
- **Unpriced pairs over 2025-10-10's four stress hours:** with the engine's level spreads, one $50M
  pair reopened before each spell loses $0.58M with or without the trigger. Pairs opened ahead take
  $18.0M without the trigger (137 spells, so 137 pairs) and $10.4M under it on target flow (100). The
  first pairs used take little: 10 pairs $181k, 25 pairs $1.75M, 50 pairs $6.71M.
- **The held sync's kill option:** $2k a day, unchanged.
- **The fastest honest client:** $102k a day without the trigger, $101k under it.

### D9: the trade-off for the owner

D9 asks that the trigger bring the fastest honest client to zero and hold the operator below its
honest share, the stress day's pairs included (D4). The first cannot be met by any loss-rate
threshold: the client's trades are too few and too small to move the reading, and only a level held
nearly always would price its entries. On its upper bound (the whole position at the first tier)
it reaches zero only with about 20 bps more on every fill, or a fee floor of 95% to 100% at every hold,
which costs the target client 85% to 90% of its gross profit. The second is met on ordinary days
before the trigger, and on a stress day only for an operator who holds at most a few pairs opened
ahead: the day's remaining budget, $424k less the $281k the other classes take, is about $140k, which
ten pairs ahead already pass. 2% margin on BTC-USD and ETH-USD closes the stress day's pairs ($0.32M);
cutting unpriced spells to 60 s or 30 s leaves $12.3M or $8.1M.

### The launch criterion at the 10 s curve

The honest share on #697's corrected income model at the 10 s curve is $386k a day on target flow and
$1.15M on current flow (1,000 paths, tier 3's fee with the least fee churn's hold moments allow),
against $385k and $1.14M at the 1 s curve: the steeper short end adds little to churn's lower bound.
Every class together takes $289k on target flow and $402k on current flow, so the criterion holds
before the trigger on both. The classes the trigger can see take $303k a day of house loss; the
trigger must hold them to $424k on target flow and $1.24M on current flow (`results/criterion.json`).

## Data and models

The study adds no data. It reads:

- `research/cap-calibration`'s (#111, #134) 102 weeks of hourly bars for every genesis market and
  its client flow, carried over by margin from a live house-counterparty venue (holding-time
  classes, sizes, leverage, the long share's hourly AR(1)); `research/spread-calibration`'s target
  flow (#623), which keeps the flow held 15 minutes or more and a tenth of the rest;
- `research/rogue-operator`'s (#549) 50 ms grids of Coinbase trades over 52 hours per market sampled
  by volatility stratum from a year of BTC-USD, ETH-USD and SOL-USD, and 2025-10-10's four most
  volatile hours;
- `research/spread-calibration`'s fee function on the engine's table (`calib/fee.py`, #614's
  decision D7) and its measured takes at the owner's values (`results/owner_latency.json`,
  `owner_fx.json`, `owner_criterion.json`, `latency_hours.csv.gz`);
- `config/production-genesis.toml` for the markets, their margin and spread bands (in tenths of a bp
  from `V52`), caps, stress levels and close-spread ceilings.

It sits beside `research/spread-calibration` rather than in it: the trigger needs honest flow
position by position on a 10 s clock, which that study's hourly cohorts and per-hour attack sums
cannot give, so its model is its own (`trigger/`); it reads the sibling studies' code and results
without changing them.

**Honest flow, position by position** (`trigger/honest.py`, `trigger/paths.py`). Each open hour of
a market's bars becomes 360 bins of 10 s: a Brownian bridge from the hour's open to its close with
the hour's Parkinson variance, so the hour's volatility, gaps and sessions carry over; each market's
mean open-hour return is taken off, as the cap study does. On that path every flow class brings its
share of the market's margin at the steady state's $181.7M a day (the mean of the reference venue's
last seven full days), as positions arriving at random over the market's open time:

- a position's notional from the kind's size profile on the market's margin bands, counted by
  position; its hold from a lognormal with the class's measured mean hold and mean square-root
  hold; its side from the class's long share plus the market's hourly deviation;
- hedged flow opens pairs in two accounts that end together when either leg is closed out;
- every fill pays the market's size spread through its bands on the opening and the closing slice
  (`V52`); a position is closed out once its equity falls below half its margin, at the first bin
  past that level, and loses at most its balance; the rest is bad debt;
- a winning close pays the fee at its hold: D7's function with the full fee to 10 s and 10% at
  15 minutes, at tier 3's discount (8% past the curve), the least any tier pays, as #697's honest
  share does.

Each close's loss to the house is the client's realised PnL at the spread-adjusted prices less its
fee. Three readings follow from it, each summed over a trailing window:

- **realised**: the house's realised loss to clients, net of fees and spreads, the form #549 and
  the plan sketched;
- **marked**: the realised loss less the change in clients' unrealised PnL, which is the house's
  NAV loss to its clients over the window, net of fees and spreads;
- **bad debt**: what closed-out accounts lost past their balance.

**The trigger** (`trigger/windows.py`) follows `stress.rs`: the level rises at once when the reading
meets the threshold, and steps down once the five-minute cooldown has passed and the reading is
under it again. A threshold is set at the honest flow's fire rate, the share of hours in which the
reading meets it at some moment; the time it holds the level raised is measured on the same series.

**Room for an attacker.** While the level is down the reading is under the threshold, so over any
window an attacker adds at most the threshold less honest flow's loss in that window, plus the one
command that crosses it (the overshoot). Summed over a day's disjoint windows this bounds what the
attacker realises in a day before the overshoots; the bound is loose by the honest flow's own
income, which an attacker can take without the house's net loss rising.

**Bad debt on recorded trades** (`run.py grid`). On bridged bars no price jumps past a closed-out
position's balance inside an hour, so honest bad debt on the crypto markets is measured on the
rogue study's sampled hours: each hour's recorded index on 10 s bins after 96 hours of synthetic
history at the market's median hourly volatility, with the year weight of its stratum.

**The stress day's pairs** (`trigger/pairs.py`, `run.py stress`). #549's causal rule on
2025-10-10's four recorded hours on BTC-USD and ETH-USD, at $50M a side, 1% margin and tier 3's 8%
fee, as #697 runs it; with #697's conventions it gives #697's $30.56M exactly. Three things change:

- each fill pays the engine's spread at its stress level, `min_spread + multiplier x first tier`
  (`execution/mod.rs`), where #697 added the level's minimum alone;
- a spell's move counts from no earlier than the market's last repricing: the engine keeps a
  source's quote only if it is no older than the one it holds (`marks.rs`, `store_quotes`), so once
  the market has priced again no later index can show an older price, where #549's rule counted from
  3 s before each start;
- the operator either reopens one pair before each spell at the level then, as #549's does, or opens
  its pairs ahead at level 0, one per spell, so a crash's raised levels reach only their closes.

The loss trigger runs beside the fast-move trigger, fed by honest flow's per-bin loss over that day
and the day before on the bridged bars (`honest`), with the stress markets' bad debt in the four hours
from honest flow on their recorded trades, and each spell's loss added when the market prices again, and the level re-read
after every loss, as the engine reads it on every command. While the trigger holds level 2 the
operator either waits or runs spells with the rule's target re-derived from its costs; each candidate
keeps the better of the two over the day. Under a realised reading the operator keeps each winning
leg open, so the reading sees only the losing leg's close-out.

**Latency arbitrage under the trigger** (`run.py latency`). The operator's 3 s view and an honest
node's fastest client at the 10 s curve, as #697 measures them (`carrot.best_responses` on
`latency_hours.csv.gz`), sit out the time the level is raised: each crypto market's take falls by the
share of it earned in raised time, from honest flow's raised time in each volatility stratum, and
each FX and index market's by its raised time overall.

**The honest share at the 10 s curve** (`run.py share`). #697's `owner_income` on the cap study's
corrected income model, with the hold curve at the full fee to 10 s, on current and target flow.

**The proposal** (`run.py recommend`). The house-wide marked reading at the proposed trigger and at
its alternative, at their exact thresholds, on both flows: fire rate, time raised and its periods,
the room a day and in the worst window, and the stress day's pairs.

## Limits

- **Honest flow is not refused by the caps.** On BTC-USD the simulated house exposure passes the cap
  at the retained NAV 6.9% of the time and on ETH-USD 1.5%; the engine would refuse orders there, so
  honest loss on those markets is overstated and the fire rates are high rather than low. At launch,
  with NAV near zero, the caps refuse most flow (#500); the day's threshold of zero does not depend on
  the flow's size, but this study does not measure launch's flow.
- **Bridged hours have no jumps.** Bars give each hour's range, not its path, so honest bad debt and
  the shortest windows' tails come from the recorded crypto hours, which cover three markets.
- **The flow does not respond to prices or to the trigger.** Honest income uses the genesis's bands
  on unrefused flow, so it is above #697's figures ($6.81M a day on current flow against #697's
  $5.70M of distributions). Less income would fire the day's trigger more often: $200k a day less on
  target flow is a threshold of -$200k on these results, which fires in about 1.3% of hours. Financing
  is left out; it adds to the house's income.
- **Each attack is measured alone**, with honest flow's loss as the background; together they add
  to the reading, which raises the fire rate a little and lowers what each takes.
- **Latency under the trigger** is #697's best response with its take cut by honest flow's raised time
  in each volatility stratum, not a strategy re-optimised around the level.
- **The stress day's operator** picks, in hindsight for the whole day, whether to wait or run spells
  while the level is raised; #549's causal rule is one rule family, and the pairs opened ahead are
  taken as unlimited, each used once.
- **The fastest client's levers** are on its upper bound (the whole position at the first tier, no
  stick); its measured take with the size tiers is lower, so less would reach it.

## Re-running

From a virtualenv outside the repo with `requirements.txt`, in this directory, each stage reading
committed data and earlier stages' results:

```
python -I run.py honest --flow current
python -I run.py honest --flow target
python -I run.py grid --flow current
python -I run.py stress --flow current
python -I run.py stress --flow target
python -I run.py latency
python -I run.py recommend
python -I run.py share --paths 1000 --workers 2
python -I run.py report
python -I -m pytest tests
```

`honest` writes each flow's per-bin series for BTC-USD, ETH-USD and the house to the gitignored
`data/`, which `stress` and `recommend` read; `latency` reads both flows' `honest` results. `share`
takes about half an hour a flow on two workers.

## Files

- `run.py`: the stages.
- `trigger/`: `inputs.py` (the sibling studies and the owner's values), `paths.py` (10 s paths
  and first crossings), `honest.py` (honest flow position by position), `windows.py` (the reading
  and the level), `pairs.py` (#549's unpriced spells under the trigger), `report.py`.
- `tests/`: the paths, the crossings, the honest flow's accounting, the level and the spells on hand
  cases.
- `results/`: each stage's output (`honest_*.json`, `grid_current.json`, `stress_*.json`,
  `latency.json`, `recommended.json`, `share.json`), `report.md` and `criterion.json`.
