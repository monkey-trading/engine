# House loss-rate trigger

This study sets the stress trigger that limits how fast the house can lose money to its clients:
what it measures, its threshold and window, and what it leaves an attacker. The trigger it proposes,
which the launch configuration carries, raises every market's stress level to 2 when the house's
marked loss to its clients over the trailing 24 hours is above zero. Honest flow at the target
client mix fires it in 0.87% of hours and holds level 2 for 0.65% of the time, and the current
client mix fires it in 0.01% of hours. While the level is down, an attacker can take at most the
day's honest income plus the one trade that crosses zero, which is $2.18M a day on average on target
flow. The trigger therefore caps how fast the house can lose. It does not cap what an attacker can
take from the house's income.

The study also tests the trigger against two launch criteria. The first is that the fastest honest
client takes nothing from the house. No loss-rate threshold meets it without firing almost always,
and that client keeps $101k a day under the trigger. The second is that the operator's attacks take
less than its honest share of distributions. That criterion holds on ordinary days, where the
attacks take $289k a day against a share of $386k on target flow. On a day like 2025-10-10, it holds
only for an operator with at most a few paired positions opened in advance. Such pairs keep $10.4M
of the $18.0M they would take without the trigger.

Amounts are in US dollars. Rates are in basis points, abbreviated to bps. One basis point is one
hundredth of one percent.

- [Results](#results)
- [Method](#method)
- [Data and sources](#data-and-sources)
- [Limits](#limits)
- [Re-running](#re-running)

## Results

Every figure is at the launch values the study read: the profit fee at 100% up to a 10 s hold,
falling to 10% at 15 minutes; the spread bands of `config/production-genesis.toml` in tenths of a
basis point; markout-spread floors at each market's markout spread over a year; and BTC-USD and
ETH-USD at 1% margin. Honest flow and paired positions pay tier 3's fee, 8% past the curve, which is
the least any tier pays. [`results/report.md`](results/report.md) holds every table, and the tables
below come from it.

Two client flows run through every stage:

- **Current flow** is the [cap calibration](../cap-calibration/README.md)'s client flow, carried
  over by margin from a live house-counterparty venue.
- **Target flow** is the [spread calibration](../spread-calibration/README.md)'s target, which keeps
  the flow held 15 minutes or more and a tenth of the rest.

### The proposed trigger

The marked loss is the realised loss to clients, net of profit fees and spreads, plus the change in
clients' unrealised profit and loss. It is the house's loss of net asset value (NAV) to its clients.
When the marked loss over the trailing 24 hours, summed over every market, is above zero, every
market's stress level rises to 2. The level steps down as it does for the other triggers, five
minutes after the trigger clears. A threshold of zero needs no scaling to NAV or to the exposure
caps, because the trigger fires exactly when the house has lost money to its clients over a day.

While the level is down, the house cannot have lost money to its clients over the day. An attacker
therefore takes at most the day's honest income plus the one command that crosses zero. On target
flow that is $2.18M a day on average, $3.18M at the 95th percentile, and $4.41M on the worst day of
102 weeks.

### The attack classes against the launch criteria

At the fee curve held at 100% to 10 s, each class takes, per day:

| A day | Without the trigger | Under it |
|---|---|---|
| Latency arbitrage, crypto, with the operator's 3 s view | $160k | $158k |
| Latency arbitrage, FX and indices, spreads in tenths of a bp | $63k | $63k |
| Unpriced pairs at 1% margin, ordinary days | $17k | at most $17k |
| Held sync's kill option | $2k | at most $2k |
| Skimming client fills, target flow, which the trigger cannot see | $47k | $47k |
| **All together, target flow** | **$289k** | **$287k** |
| Honest share, target flow | $386k | $386k |
| Fastest honest client (criterion: zero) | $102k | $101k |
| Unpriced pairs over 2025-10-10's four stress hours, house loss | $18.0M | $10.4M |

The [rogue operator study](../rogue-operator/README.md) describes each attack class. The operator's
take is 80% of its own accounts' profit, and its honest share is 20% of the house's distributions.

- **The operator criterion holds on ordinary days, before the trigger.** Every class together takes
  $289k a day against a $386k share on target flow, and $402k against $1.15M on current flow. For
  the classes the trigger can see, the criterion allows $424k a day of house loss on target flow.
  Unbounded, those classes take $303k.
- **The fastest honest client keeps $101k a day.** It makes 22 trades a day on the crypto markets,
  and each adds about $3.6k to a reading that honest flow moves by hundreds of thousands of dollars
  an hour. It also sits out the 0.65% of the time the level is raised. Nothing short of holding
  level 2 almost all the time reaches it.
- **Paired positions opened ahead keep $10.4M of the stress day's $18.0M.** Once a pair is open,
  level 2 raises only the spread on closing it. A move of 1.4% already raises the market to level 1,
  where closing costs 6 to 8 bps, and level 2 raises that cost to 18 to 22 bps. A pair opened at
  level 0 still pays from a 1.31% move on BTC-USD and 1.37% on ETH-USD, which 2025-10-10 offered
  again and again. Each pair opened ahead is $1M of margin over two accounts that have reached tier
  2, and the day takes 100 of them for $10.4M. Every candidate trigger tried, including thresholds
  that fire in half of all hours, leaves them $6.7M or more.

### Honest flow's loss over one to sixty minutes

Across the house, honest current flow earns the house $6.81M a day and target flow $2.18M, net of
what the house pays winners. Over short windows the house still loses often. Over windows of one to
five minutes, the highest marked reading in an hour is above zero in more than half of all hours.
The 99th percentile of each hour's highest marked loss is:

| Window | Current flow | Target flow |
|---|---|---|
| 1 minute | $124k | $131k |
| 5 minutes | $214k | $248k |
| 15 minutes | $266k | $344k |
| 30 minutes | $273k | $418k |
| 60 minutes | $224k | $485k |
| 6 hours | -$734k | $628k |
| 24 hours | -$4.74M | -$74k |

By market, the loss follows the flow. On current flow, the 60-minute threshold that fires in 1% of
hours is $263k on BTC-USD, $175k on ETH-USD, $30k on SOL-USD and $38k on US500, and a few thousand
dollars or less on the FX majors (`results/report.md`, "Per market"). The realised loss, without the
unrealised change, has smaller tails over the long windows, and the same order over the short ones.

Honest bad debt is rare. On the rogue operator study's recorded hours, honest flow leaves bad debt
in 0.08% of BTC-USD's hours and 1.4% of ETH-USD's. It never leaves more than $1k in an ordinary
hour, and at most $33k in one of 2025-10-10's four stress hours. On the hourly bars, the markets
that gap carry it: WTI, US100, silver and US500 at their reopenings. On target flow, more than $25k
of bad debt across the house within 60 minutes occurs in 0.16% of hours.

### The measure

- **Marked, not realised.** A realised reading sees what the house pays when a client closes a
  position. An operator holding pairs across an unpriced move lets the engine close out the losing
  leg, which the house realises as a gain of that leg's balance, and keeps the winning leg open. On
  the stress day, no realised threshold holds such pairs, and they take $17.9M to $18.0M. The marked
  reading counts the winning leg's gain when the market is priced again. The engine computes house
  NAV from each market's client quantity and cost (`engine/house.rs`), so the reading is the fall in
  house NAV over the window. It leaves out house capital credits and distributions, and needs no
  state beyond the window's buckets.
- **House-wide, not per market.** The markets' losses partly offset each other, so one house-wide
  threshold usually leaves an attacker less room than per-market thresholds that fire as often. Of
  the eight daily fire rates tried, from 0.1% to 50% of honest hours, the house-wide threshold
  leaves the stress day's pairs less at six (`results/stress_target.json`). On target flow, the
  launch trigger, house-wide over a day at a threshold of zero, leaves them $10.4M. Per-market
  thresholds over a day, set to fire in 1% of honest hours, leave them $12.0M. Each market's honest
  time at level 2 stays about the same. A per-market reading would catch an attack on one small
  market sooner, but no measured class needs it.
- **Against zero.** Honest flow's income and the spread of its losses both grow with its volume.
  Whether the house lost money over a day therefore does not change when the same flow grows or
  shrinks, and the threshold needs neither NAV nor the caps to scale it. It does depend on the
  flow's mix. Target flow has less short-hold income against the same long-hold exposure, and fires
  the trigger in 0.87% of hours, against 0.01% for current flow. A threshold against NAV or the
  total cap would have to be negative, at -$4.74M, to fire in 1% of hours over a day on current
  flow, and would then fire most of the time if the flow halved. The 60-minute alternative needs a
  positive threshold, and 22.5 bps of the total exposure cap, `max(NAV x 200%, total floor)`, keeps
  it scaled to the house.
- **Bad debt as a reading of its own** was tried. At $25k over 60 minutes, it fires in 0.16% of
  hours on target flow and leaves the stress day's pairs $8.2M. That is about what the marked
  reading leaves, since a pair opened ahead still pays at level 2.

### The threshold and the window

The room that a threshold firing in 1% of hours leaves an attacker each day falls as the window
lengthens. Honest flow's loss over a window grows more slowly than the number of windows in a day
shrinks. On target flow the room is $191M a day at one minute, $35M at 15 minutes, $13.8M at
60 minutes, $4.7M at six hours and $2.1M at a day. A day's window at zero is the tightest bound that
fires in at most 1% of hours on both flows.

| Trigger | Flow | Threshold | Hours fired | Level 2 | Periods a year, mean length | Room a day, mean / p95 | 2025-10-10, pairs opened ahead |
|---|---|---|---|---|---|---|---|
| A day, zero | target | $0 | 0.87% | 0.65% | 74, 46 min | $2.18M / $3.18M | $10.4M |
| A day, zero | current | $0 | 0.01% | 0.00% | 1, 5 min | $6.81M / $8.57M | $13.1M |
| 60 minutes, 22.5 bps of the total cap | target | $483k | 1.00% | 0.36% | 177, 11 min | $13.8M / $14.8M | $9.2M |
| 60 minutes, 22.5 bps of the total cap | current | $483k | 0.32% | 0.09% | 53, 9 min | $18.4M / $20.2M | $11.4M |

The latency classes, running beside honest flow, add about $300k a day of house loss to the day's
reading on target flow. That would raise its fire rate to between 1% and 2% of hours.

### What the trigger does when it fires

The trigger raises every market to level 2, which adds a 10 bps minimum spread and multiplies the
size spread by four. A market stays at level 2 for the five-minute cooldown, and until the day's
marked loss is back below zero, as `stress.rs` steps any level down. Level 1, with 2 bps and twice
the size spread, would do less. The latency classes do not fire the trigger, and a pair opened ahead
pays at level 1 from a 1.19% move on BTC-USD, against 1.31% at level 2.

The spread on closing a position stays clamped at its market's closing spread cap. Closing a
position within the first band costs 18 bps at level 2 on BTC-USD and SOL-USD and 22 bps on ETH-USD,
under caps of 30, 35 and 35 bps. The cap does bind on a large BTC-USD position. A $50M position
averages 7.1 bps through its bands, which is 38 bps at level 2, so closing it costs the 30 bps cap.

### The bound on each attack class

- **The most the house can lose in a day** is its honest income for the day, plus the one command
  that crosses zero, plus what is lost at level 2 while the level holds. The largest single command
  on the stress day, a pair's repricing, loses $0.82M. A latency trade loses its $6.9M position
  times the move it catches. On the worst single day of 102 weeks, the room is $4.41M on target flow
  and $10.9M on current flow.
- **Latency arbitrage on crypto, with the operator's 3 s view,** takes $160k a day at the 10 s fee
  curve. Its best strategy holds each position for 15 minutes and recycles wallets at tier 0. Under
  the trigger, which it sits out, it takes $158k (`run.py recommend`, at the exact threshold on
  target flow).
- **Latency arbitrage on FX and indices,** with spreads in tenths of a bp, takes $63k a day, with or
  without the trigger.
- **Unpriced pairs at 1% margin on ordinary days** take $17k a day, as the spread calibration
  measures them. The trigger can only lower that figure.
- **Unpriced pairs over 2025-10-10's four stress hours.** With the engine's level spreads, an
  operator that reopens one $50M pair before each spell loses $0.58M, with or without the trigger.
  Pairs opened ahead take $18.0M without the trigger, over 137 spells and so 137 pairs, and $10.4M
  under it on target flow, over 100. The first pairs used take little: 10 pairs take $181k, 25 pairs
  $1.75M, and 50 pairs $6.71M.
- **The held sync's kill option** takes $2k a day, with or without the trigger.
- **The fastest honest client** takes $102k a day without the trigger and $101k under it.

### The launch criteria at the 10 s fee curve

The criteria are that the trigger bring the fastest honest client's take to zero or less, and that
it hold the operator's take below its honest share, with the stress day's paired positions at 1%
margin included.

No loss-rate threshold can meet the first criterion. The client's trades are too few and too small
to move the reading, and only a level held nearly always would price its entries. On its upper
bound, with its whole position at the first tier, the client reaches zero only with about 20 bps
more on every fill, or with a fee floor of 95% to 100% at every hold. Such a floor would cost the
target client 85% to 90% of its gross profit.

The second criterion is met on ordinary days before the trigger fires. On a stress day it is met
only for an operator that holds at most a few pairs opened ahead. The day's remaining budget is
$424k less the $281k that the other classes take, about $140k, and ten pairs opened ahead already
exceed it. At 2% margin on BTC-USD and ETH-USD, the stress day's pairs take only $0.32M. Cutting
unpriced spells to 60 s or 30 s leaves them $12.3M or $8.1M.

The honest share, on the spread calibration's corrected income model at the 10 s fee curve, is $386k
a day on target flow and $1.15M on current flow. That model runs 1,000 paths, with tier 3's fee and
the least fee that the moments of churn's holds allow. At a curve that holds the full fee to 1 s,
the share is $385k and $1.14M, so the steeper short end adds little to churn's lower bound. Every
class together takes $289k on target flow and $402k on current flow, so the operator criterion holds
before the trigger on both flows. The classes the trigger can see take $303k a day of house loss,
and the trigger must hold them to $424k on target flow and $1.24M on current flow
(`results/criterion.json`).

### Proposals

1. **The trigger.** A day's marked loss above zero. Status: in the launch configuration, as level
   2's `house_loss = "0"` in `config/production-genesis.toml`. The faster alternative, a 60-minute
   window at 22.5 bps of the total exposure cap ($483k at the retained NAV), answers a sudden drain
   within the hour but leaves an attacker $13.8M a day. Status: not adopted.
2. **The fastest honest client.** The trigger cannot bring this client to zero. On the client's
   upper bound, about 20 bps more on every fill, or a fee near 95% at every hold, would do so. That
   fee would cost the target client 85% of its gross profit. Status: the launch rules add neither,
   so the client's $101k a day stands.
3. **The stress day's pairs.** The trigger roughly halves them. At 2% margin on BTC-USD and ETH-USD,
   they take $0.32M of house loss on the stress day without the trigger. At 1% margin, each pair an
   operator opens ahead, with $1M of margin, takes $104k on average under the trigger and $132k
   without it. Status: BTC-USD and ETH-USD launch at 1% margin, with this trigger as the bound on
   paired positions.

The study also corrects the spread calibration's stress-day figure of $30.6M of house loss, of which
$24.4M goes to the operator. That figure charged each raised level's minimum spread but not its size
multiplier, and let a spell count a move from before the market's last repricing. With the engine's
level spreads and quote order, an operator that reopens one $50M pair before each spell loses $0.58M
on the stress day, and only pairs opened ahead pay. The
[launch caps study](../launch-caps/README.md) runs the pairs at the engine's level spreads.

## Method

The study's model is its own (`trigger/`). The trigger needs honest flow settled position by
position on a 10 s clock, which the spread calibration's hourly cohorts and per-hour sums of each
attack cannot give. The model reads the sibling studies' code and results without changing them.

**Honest flow, position by position** (`trigger/honest.py`, `trigger/paths.py`). Each open hour of a
market's bars becomes 360 bins of 10 s. The path is a Brownian bridge from the hour's open to its
close with the hour's Parkinson variance, so the hour's volatility, gaps and sessions carry over.
Each market's mean return over its open hours is taken off, as the cap calibration does. On that
path, every flow class brings its share of the market's margin at the steady state of $181.7M a day,
the mean of the reference venue's last seven full days. Positions arrive at random over the market's
open time:

- A position's notional comes from its kind's size profile on the market's margin bands, counted by
  position. Its hold comes from a lognormal distribution with the class's measured mean hold and
  mean square-root hold. Its side comes from the class's long share plus the market's hourly
  deviation.
- Hedged flow opens pairs in two accounts, which end together when either leg is closed out.
- Every fill pays the market's size spread through its bands, on the part that opens and on the part
  that closes. A position is closed out once its equity falls below half its margin, at the first
  bin past that level, and loses at most its balance. The rest is bad debt.
- Closing a winning position pays the fee at its hold: the fee curve with the full fee to 10 s and
  10% at 15 minutes, at tier 3's discount of 8% past the curve, the least any tier pays, as the
  spread calibration's honest share does.

Each close's loss to the house is the client's realised profit at the spread-adjusted prices, less
its fee. Three readings follow from it, each summed over a trailing window:

- **realised**, the house's realised loss to clients, net of fees and spreads;
- **marked**, the realised loss plus the change in clients' unrealised profit, which is the house's
  NAV loss to its clients over the window, net of fees and spreads;
- **bad debt**, what accounts lost past their balance when they were closed out.

**The trigger** (`trigger/windows.py`) follows `stress.rs`. The level rises at once when the reading
meets the threshold, and steps down once the five-minute cooldown has passed and the reading is
under the threshold again. A threshold is set at honest flow's fire rate, the share of hours in
which the reading meets it at some moment. The time the level is held raised is measured on the same
series.

**Room for an attacker.** While the level is down, the reading is under the threshold. Over any
window, an attacker therefore adds at most the threshold less honest flow's loss in that window,
plus the one command that crosses it, the overshoot. Summed over a day's disjoint windows, this
bounds what the attacker realises in a day before the overshoots. The bound is loose by honest
flow's own income, which an attacker can take without the house's net loss rising.

**Bad debt on recorded trades** (`run.py grid`). On the hourly bars, no price jumps past a
closed-out position's balance inside an hour. Honest bad debt on the crypto markets is therefore
measured on the rogue operator study's sampled hours. Each hour's recorded index runs on 10 s bins
after 96 hours of synthetic history at the market's median hourly volatility, with the year weight
of its stratum.

**The stress day's pairs** (`trigger/pairs.py`, `run.py stress`). The study runs the rogue operator
study's causal rule over 2025-10-10's four recorded hours on BTC-USD and ETH-USD, at $50M a side, 1%
margin and tier 3's fee of 8%, as the spread calibration runs it. With that study's conventions, it
reproduces that study's $30.56M exactly. Three things change:

- Each fill pays the engine's spread at its stress level, `min_spread + multiplier x first tier`
  (`execution/mod.rs`). The spread calibration added the level's minimum alone.
- A spell's move counts from no earlier than the market's last repricing. The engine keeps a
  source's quote only if it is no older than the one it holds (`marks.rs`, `store_quotes`), so once
  the market has been priced again, no later index can show an older price. The rogue operator
  study's rule counted the move from 3 s before each spell started.
- The operator either reopens one pair before each spell at the level then in force, as the rogue
  operator study's rule does, or opens its pairs ahead at level 0, one per spell. In the second
  case, a crash's raised levels reach only the spreads on closing.

The loss trigger runs beside the fast-move trigger. It is fed by honest flow's loss in each bin over
that day and the day before on the hourly bars (`honest`), and by the stress markets' bad debt over
the four hours from honest flow on their recorded trades. Each spell's loss is added when the market
is priced again, and the level is read again after every loss, as the engine reads it on every
command. While the trigger holds level 2, the operator either waits or runs spells with the rule's
target derived again from its costs, and each candidate keeps the better of the two over the day.
Under a realised reading, the operator keeps each winning leg open, so the reading sees only the
losing leg's close-out.

**Latency arbitrage under the trigger** (`run.py latency`). The operator's 3 s view and an honest
trading server's fastest client at the 10 s fee curve, as the spread calibration measures them
(`carrot.best_responses` on `latency_hours.csv.gz`), sit out the time the level is raised. Each
crypto market's take falls by the share of it earned in raised time, from honest flow's raised time
in each volatility stratum. Each FX and index market's take falls by its raised time overall.

**The honest share at the 10 s fee curve** (`run.py share`). The study runs the spread calibration's
`owner_income` on the cap calibration's corrected income model, with the hold curve at the full fee
to 10 s, on current and target flow.

**The proposal** (`run.py recommend`). The house-wide marked reading runs at the proposed trigger
and at its alternative, at their exact thresholds, on both flows. It gives the fire rate, the time
raised and its periods, the room a day and in the worst window, and the stress day's pairs.

## Data and sources

The study adds no data. It reads:

- the [cap calibration](../cap-calibration/README.md)'s 102 weeks of hourly bars for every genesis
  market, and its client flow, carried over by margin from a live house-counterparty venue, with its
  holding-time classes, sizes, leverage, and the long share's hourly AR(1);
- the [spread calibration](../spread-calibration/README.md)'s target flow, which keeps the flow held
  15 minutes or more and a tenth of the rest;
- the [rogue operator study](../rogue-operator/README.md)'s 50 ms grids of Coinbase trades over
  52 hours a market, sampled by volatility stratum from a year of BTC-USD, ETH-USD and SOL-USD, and
  2025-10-10's four most volatile hours;
- the spread calibration's profit-fee function on the engine's table (`calib/fee.py`), and its
  measured takes at the launch values (`results/owner_latency.json`, `owner_fx.json`,
  `owner_criterion.json` and `latency_hours.csv.gz`);
- `config/production-genesis.toml`, for the markets, their margin and spread bands in tenths of a
  bp, the caps, the stress levels and the closing spread caps.

## Limits

- **The caps do not refuse honest flow.** On BTC-USD, the simulated house exposure passes the cap at
  the retained NAV 6.9% of the time, and on ETH-USD 1.5% of the time. The engine would refuse orders
  there, so honest loss on those markets is overstated, and the fire rates err high rather than low.
  At launch, with NAV near zero, the caps refuse most flow. The day's threshold of zero does not
  depend on the flow's size, but this study does not measure the flow at launch.
- **The hourly bars have no jumps within an hour.** Bars give each hour's range, not its path, so
  honest bad debt and the tails of the shortest windows come from the recorded crypto hours, which
  cover three markets.
- **The flow does not respond to prices or to the trigger.** Honest income uses the genesis bands on
  flow that the caps do not refuse, so it is above the spread calibration's figures: $6.81M a day on
  current flow, against that study's $5.70M of distributions. Less income would fire the day's
  trigger more often. On these results, $200k a day less on target flow acts as a threshold of
  -$200k, which fires in about 1.3% of hours. Financing is left out, and it adds to the house's
  income.
- **Each attack is measured alone,** with honest flow's loss as the background. Together the attacks
  add to the reading, which raises the fire rate a little and lowers what each takes.
- **Latency under the trigger** is the spread calibration's best response, with its take cut by
  honest flow's raised time in each volatility stratum. It is not a strategy optimised again around
  the level.
- **The stress day's operator** chooses with hindsight, for the whole day, whether to wait or run
  spells while the level is raised. The rogue operator study's causal rule is one family of rules,
  and the pairs opened ahead are taken as unlimited, each used once.
- **The fastest client's levers** are measured on its upper bound, with its whole position at the
  first tier and no toxicity add-on. Its measured take with the size tiers is lower, so less of an
  extra spread or fee would reach it.

## Re-running

From a virtualenv outside the repo with `requirements.txt`, in this directory. Each stage reads
committed data and the results of earlier stages:

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
`data/`, which `stress` and `recommend` read. `latency` reads both flows' `honest` results. `share`
takes about half an hour a flow on two workers. The model is in [`trigger/`](trigger), its tests in
[`tests/`](tests), and each stage's output in [`results/`](results).
