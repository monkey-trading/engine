# Spread, fee and toxicity calibration

This study sets the prices that keep an operator from profiting by attacking the house, while
charging honest clients as little as possible. The operator runs the trading server. It sees venue
prices before the engine applies them, and within the rules that the enclave enforces it can delay,
reorder or hold back instructions. The [rogue-operator study](../rogue-operator/README.md) measured
what such an operator can take. This study calibrates four defences against it and values each one
against the launch criterion, which requires everything the operator can take by attacking the house
to be less than its honest share of the house's income.

The four defences are a spread on the first band of every market, a profit fee that falls with how
long a position is held, a toxicity score that round trips and trade size cannot dilute, and volume
tiers that a new wallet starts without. Rates are given in basis points, abbreviated to bps. One
basis point is one hundredth of one percent.

At the launch values, the operator's honest share is at least $1.14M a day on the client flow that
the study measures, and at least $385k a day on target flow, which keeps only the clients who hold
positions for 15 minutes or more. Under the launch fee curve, which takes the whole gain on a
position closed within 10 seconds of opening, every attack class together takes about $289k a day on
target flow, which is within the share. Two exposures remain for the
[house loss-rate trigger](../loss-rate-trigger/README.md) to bound. Paired positions across unpriced
moves take $24.45M over the four stress hours of 10 October 2025, and the fastest honest client
takes $102k a day.

- [Results](#results): [the launch criterion](#the-launch-criterion), [spread bands](#spread-bands),
  [the holding-time fee](#the-holding-time-fee), [the toxicity score](#the-toxicity-score),
  [markout-spread floors](#markout-spread-floors), [volume tiers](#volume-tiers),
  [resting-order limits](#resting-order-limits),
  [retained NAV and house income](#retained-nav-and-house-income),
  [BTC-USD and ETH-USD at 100x](#btc-usd-and-eth-usd-at-100x),
  [the attack classes at the first proposals](#the-attack-classes-at-the-first-proposals),
  [the values chosen](#the-values-chosen)
- [Method](#method)
- [Data and sources](#data-and-sources)
- [Limits](#limits)
- [Re-running](#re-running)

## Results

The operator's take from an attack is counted as 80% of its own accounts' profit, as the
rogue-operator study counts it. Its honest share is 20% of what the house distributes, which is the
part that goes to the operator in effect. Each figure is a day's average.

Two client flows are compared. Current flow is the flow that the
[cap calibration](../cap-calibration/README.md) derives from a live house-counterparty venue's
on-chain trading. Target flow keeps the part of that flow held an hour or more, the part held 15 to
60 minutes, and a tenth of the rest (`calib/target.py`).

### The launch criterion

The figures in this section use the corrected income model, which [Method](#method) describes, on
the 34 launch markets. Every account pays tier 3's fee, which is the least any tier pays, and short
holds pay the least fee that the flow's recorded holds allow. The honest share is therefore a lower
bound. Each attack class in the table is measured under a fee curve that takes the whole gain only
on positions closed within 1 second, and reaches 10% at 15 minutes.

| A day | Current flow | Target flow |
|---|---|---|
| Honest share, lower bound | $1.14M | $385k |
| Honest share, central estimate | $1.17M | $389k |
| Honest share, with no tier discount | $1.16M | $405k |
| Latency arbitrage on crypto, from the operator's 3-second view | $265k | $265k |
| Latency arbitrage on FX and indices, at the proposed first bands | $197k | $197k |
| Unpriced pairs at 1% margin, at tier 3's 8% fee | $17k | $17k |
| Skimming client fills | $159k | $47k |
| The held sync's kill option | $2k | $2k |
| Censoring orders that close positions | $0 | $0 |
| **Every class together** | **$640k** | **$528k** |
| The fastest honest client, with a 0.25-second lead | $102k | $102k |

The classes are the rogue-operator study's:

- **Latency arbitrage:** the operator trades its own accounts against an index that it holds up to 3
  seconds behind the venues.
- **Skimming client fills:** the operator fills each client order at the worst index of the last 3
  seconds. Clients pay for it, and the house receives what they pay.
- **The held sync's kill option:** the operator trades, holds the batch's write to storage for up to
  5 seconds, and restarts the trading server if the trade lost.
- **Censoring:** the operator delays other clients' orders that would close their positions.
- **Unpriced pairs:** the operator holds a long position in one account and a short position in
  another. It holds a venue back across a fast move, so that the market has no price and the losing
  position is not closed out in time. The loss beyond that position's balance falls on the house.

The fastest honest client sees venue prices 0.25 seconds before the engine prices them, and moves to
a new wallet whenever its toxicity score would add to its spread.

**At the launch fee curve.** The launch curve takes the whole gain on positions closed within 10
seconds, and still reaches 10% at 15 minutes. It cuts the operator's crypto take from $265k to $160k
a day, and its FX and index take from $197k to $63k, so that every class together takes about $289k
a day on target flow. Clients who hold for 15 minutes or more pay no more under it, while positions
closed within 15 minutes pay 44% of their gains instead of 33%. A curve that takes the whole gain up
to 60 seconds gives the same takes, and charges those positions 61% of their gains. Because a
steeper curve charges every hold at least as much, the honest share at the launch curve is at least
the share in the table.

**What the loss-rate trigger must bound.** The house pays for latency arbitrage, unpriced pairs and
the held sync's kill option, so the loss-rate trigger can see those three classes. Clients pay for
skimming and censoring. On target flow, under the 1-second curve, the criterion holds only if the
classes the trigger sees cost the house at most $423k a day, against $601k unbounded. That is a cut
of at least 30%. On current flow their budget is $1.22M a day, which they are within. The daily
figures leave out two exposures that the trigger must also bound:

- Over the four stress hours of 10 October 2025, unpriced pairs cost the house $30.56M, of which the
  operator takes $24.45M.
- The fastest honest client takes $102k a day: $79k on the crypto markets and $23k on the FX and
  index markets. The launch rule is that the trigger brings this take to zero or less.

The tables are in [`results/owner_values.md`](results/owner_values.md), and the trigger's budgets in
`results/owner_criterion.json`.

**The first income model.** Churn is the flow held for less than an hour. The study's first stage,
in [`results/report.md`](results/report.md), charges churn the profit fee on its gross price gain at
each class's mean hold, and adds both spreads beside it. The engine charges the fee on realised
profit after both spreads, so that model overstates the house's income. At the launch values it
overstates the honest share by $103k a day on current flow and $13k on target flow against the lower
bound, and by $73k and $10k against the central estimate.

**FX and index first bands.** At the proposed first bands, latency arbitrage on the eight FX and
index markets costs the house $246k a day, of which the operator takes $197k. A first band of 1 bp
would leave $161k of house loss, and a reference schedule of 0.1 bp to $2M, 1 bp to $10M and 3 bps
above would leave $553k. These figures are measured at each market's exact spread, with the size
bands, the toxicity score and wallet recycling, as the crypto takes are. An upper bound that puts
the whole position at the first band, with no toxicity score, gives $676k, $456k and $1.17M. On that
bound, every class together takes $984k a day on current flow and $872k on target flow. Measured at
the spread, EUR-USD and GBP-USD at 1 bp each take about $1.6k a day. The first stage's figure of
zero for those two markets comes from an interpolation that reads anything past its largest measured
spread as zero.

**The fee base and tier discounts.** The operator's bounds admit a profit fee base from launch's 10%
up to 15%, and tier discounts no larger than launch's, ordered by tier. The study checks all 60
admitted combinations, at every hundredth of a second up to an hour. On each one, every tier's fee
falls with the hold, a higher tier never pays more than a lower tier, and every tier pays at least
its launch fee at every hold.

- A higher base raises the honest share, to $1.16M and $405k at 12.5%, and to $1.18M and $425k at
  15%. At 15%, the fastest client's take falls to $82k, and the pairs' year turns negative.
- The operator's latency take does not move with the base or the discounts. Its best holds are 3 to
  5 seconds, where the curve sets the fee, and its best strategy recycles wallets at tier 0, which
  has no discount.
- A cut of the base to 5%, which the bounds refuse, would put every tier up to 5 percentage points
  under its launch fee from 15 minutes.

### Spread bands

Each market's first band covers the mean move of the index over 3 seconds in the market's
90th-percentile open hour, plus ordinary flow's measured edge against the house, which is 0.18 bps
of notional. Three seconds is about the most that the price an order fills at can lag the venues.
The second band is twice the first, and the band above it four times the first, at the size bounds
of `config/production-genesis.toml`. A larger order's adverse selection grows with its size, and the
study has no measurement of how fast it grows.

The crypto markets' moves are measured on venue trades. The other markets' moves are scaled from
their hourly ranges, by the ratio the crypto data gives between a 3-second move and the hour's
Parkinson range (`calib/volatility.py`). Every first band sits within or below the published retail
spread for its class.

| Markets | 3-second move, bps | First band | Second band | Above the second band |
|---|---|---|---|---|
| BTC-USD | 1.18, measured | 2 bps to $2M | 4 bps to $10M | 8 bps |
| ETH-USD | 2.40, measured | 3 bps to $1M | 6 bps to $5M | 12 bps |
| SOL-USD | 1.67, measured | 2 bps to $1M | 4 bps to $5M | 8 bps |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD | 0.20 to 0.27, scaled | 1 bp to $10M | 2 bps to $50M | 4 bps |
| US500, US100, US30, JP225 | 0.48 to 0.80, scaled | 1 bp to $5M | 2 bps to $25M | 4 bps |
| XAU-USD, XAG-USD, WTI | 0.97 to 1.63, scaled | 2 bps to $2M | 4 bps to $10M | 8 bps |
| AMD, AVGO, LLY, NVDA, ORCL, TSLA | 1.85 to 2.70, scaled | 3 bps to $500k | 6 bps to $2M | 12 bps |
| The other 14 shares | 0.89 to 1.83, scaled | 2 bps to $500k | 4 bps to $2M | 8 bps |

[`results/spread_tiers.toml`](results/spread_tiers.toml) holds every market's bands in the genesis's
format, and `results/report.md` compares each with the published norm. Both also cover DE40, UK100
and Brent, which the launch catalogue leaves out.

FX majors need a first band of about 0.4 bps, which is a move of 0.2 bps plus the 0.18 bps edge.
Indices need 0.7 to 1.0 bps. A first band of 1 bp on these markets is therefore set by a unit of
whole basis points, not by the lag. The launch catalogue prices spreads in tenths of a basis point,
and its proposed FX and index first bands are each market's 3-second move plus 0.18 bps, rounded up
to the tenth. These are 0.4 bps on EUR-USD and GBP-USD, 0.5 on USD-JPY and AUD-USD, 0.7 on US500 and
US30, 0.9 on US100, and 1.0 on JP225. `results/report.md` tabulates what latency arbitrage takes at
first bands of 0.1, 0.25, 0.5 and 1 bp.

### The holding-time fee

The profit fee is a share of a closed position's gain, and the share depends on how long the
position was held. A position closed at a loss pays nothing. Each schedule below takes a peak share
up to a given hold, then falls linearly in the logarithm of the hold to 10%, the standard fee, and
stays there.

The cost to honest clients is measured on the holds of 15 minutes or more in the cap calibration's
holding-time profile, which stand for target flow. The profile's median hold is about a minute, and
that flow is what a zero fee pays for. On the first income model, a selection of the frontier in
`results/report.md` gives:

| Schedule | Extra fee on target flow, share of gross profit | Fee on holds under 15 minutes | Operator, 3-second view | Fastest client, 0.25-second lead |
|---|---|---|---|---|
| A flat 10% | 0.0% | 10% | $3.17M | $206k |
| 100% to 1 s, 10% at 15 minutes | 0.0% | 33% | $276k | $79k |
| **100% to 10 s, 10% at 15 minutes (launch)** | **0.0%** | **44%** | **$160k** | **$79k** |
| 100% to 60 s, 10% at 15 minutes | 0.0% | 61% | $160k | $79k |
| 100% to 10 s, 10% at 30 minutes | 1.0% | 52% | $117k | $68k |
| 100% to 300 s, 10% at 30 minutes (first proposal) | 2.9% | 89% | $115k | $61k |

The launch curve takes the whole gain on a hold of up to 10 seconds, and falls to 10% at 15 minutes
along eleven holds spaced evenly in the logarithm of the hold. It interpolates linearly between them
and rounds the fee up to the whole basis point. The study's first proposal held the whole gain to 5
minutes and reached 10% at 30 minutes. That proposal cost target flow 2.9% of its gross profit and
took 89% of the gross profit on holds under 15 minutes. A flat 10% leaves the operator's own
accounts $3.17M a day at the calibrated spreads.

**How a hold is measured** (`calib/fee.py`, `HoldClock`). The hold belongs to the position, not to
each lot:

- Opening a position sets its opening time.
- Adding to a position moves the opening time to the size-weighted average of the old opening time
  and the time of the addition, as the entry price is averaged.
- Closing part of a position pays at the hold since the opening time, and leaves that time
  unchanged.
- Reversing a position closes it at its hold, and opens the remainder at the time of the reversal.
- A close-out is treated as closing the position. If it realises a gain, the gain pays at the
  position's hold.

A position that has aged and is then adjusted in slices keeps its old opening time only in
proportion. Slices of a tenth every 10 seconds pay at a hold of 100 seconds.

### The toxicity score

Each fill's markout is the move of the index in the fill's favour 5 seconds later, in units of the
market's markout spread over the trailing hour of sequenced marks. The markout spread never falls
below a floor for each market, which the [next section](#markout-spread-floors) sets. The score is
the account's time-decayed sum of weighted markouts, divided by the square root of its decayed sum
of squared weights, with a half-life of 7 days. It measures how many standard errors the account's
recent edge sits above zero.

A fill's weight is its notional, capped at four times the account's decayed mean fill. A round trip
at the same index within the 5-second horizon scores nothing. No tier applies before 20 decayed
fills. A score of 4 adds 2 bps to the account's spread, and a score of 8 adds 5 bps. With the floor
at half the year's markout spread, on the sampled hours:

| Market | Markout spread, bps | Kurtosis | Latency trader's edge a fill, standard errors | Honest accounts reaching tier 1 in 1,000 fills | Cost of diluting under tier 1, over the edge hidden |
|---|---|---|---|---|---|
| BTC-USD | 1.67 | 12 | 1.83 | 0.15% | 8.4 times |
| ETH-USD | 2.52 | 17 | 1.96 | 0.05% | 8.6 times |
| SOL-USD | 2.52 | 9 | 1.56 | 0.35% | 10.8 times |

- Between 0.05% and 0.35% of accounts with no edge reach the first tier at some point in 1,000
  fills, and none reach the second. These rates come from the markouts themselves, whose tails are
  fat, not from a normal distribution.
- An account that trades the price lag carries 1.6 to 2.0 standard errors of edge a fill, and
  reaches the first tier at the 20-fill minimum.
- Holding such an account's score under a tier with fills that have no edge costs 8 to 11 times the
  edge it hides, at the calibrated spreads and the weight cap.

The sample holds separate hours, so each hour's own markout spread stands in for its trailing hour.
Measured against the year's single markout spread instead, accounts with no edge reach the first
tier at 0.40% to 0.55% in 1,000 fills, and a latency trader's fill carries 2.1 to 3.2 standard
errors (`results/toxicity.json`, the `fixed` entries).

### Markout-spread floors

The score divides each markout by the market's trailing-hour markout spread, never below the
market's floor. The floor keeps a quiet hour, or a host that holds the index still, from inflating
honest accounts' scores past what the floor allows. The launch floor for each market is the full
year's markout spread, which `run.py floors` measures for each of the 34 launch markets. The values
are in [`results/markout_floors.toml`](results/markout_floors.toml), in the genesis's format, and
the tables in [`results/markout_floors.md`](results/markout_floors.md).

Where there is no tick data, the year's markout spread is scaled from the market's hourly bars, as
the lag move is. A Brownian path with each open hour's Parkinson volatility `sigma` moves
`sigma * sqrt(5 / 3600)` in root mean square over 5 seconds. The crypto markets' measured spreads
are 0.87 of that scaling of their own bars: 0.92 on BTC-USD, 0.90 on ETH-USD and 0.79 on SOL-USD.
The ratio is crypto's, and applying it to FX, indices, commodities and shares is an assumption. The
floor is the result rounded up to the hundredth of a basis point:

| Markets | Floor, bps |
|---|---|
| BTC-USD, ETH-USD, SOL-USD | 1.68, 2.53, 2.53 |
| FX | 0.27 to 0.38 |
| Indices | 0.65 to 1.05 |
| Commodities | 1.22 to 2.24 |
| Shares | 1.24 to 3.47 |

**Honest accounts at the floor.** Scored against their hour's markout spread, which stands in for
the trailing hour, or against the floor in hours quieter than it, at most 0.15% of accounts with no
edge reach tier 1 within 1,000 fills on any market, and none reach tier 2.

The floor binds hardest when every markout is scored against it, as when the host holds the index
still for the trailing hour and then releases it. In that case 0.5% to 1.5% of honest accounts reach
tier 1 within 1,000 fills, and at most 0.2% reach tier 2. A separate draw of 4,000 accounts gives
1.3% at worst. With the floor at half the year's markout spread, the same case gives 16% at the
median market and 19% at worst. The latency trader still reaches tier 1 at the 20-fill minimum on
every crypto market, with 1.4 to 1.8 standard errors a fill against 1.6 to 2.0 at half the year's
spread. The study does not measure how far the host can hold an index still within its staleness
bounds, so this case is an upper bound. `results/markout_floors.md` gives the same measures at half,
three quarters and one and a half times the year's spread.

### Volume tiers

An account's tier depends on its counted volume over the last 30 days. The launch values are:

| Counted volume, 30 days | Signed requests a second | Best-price size | Initial margin floor | Profit fee base | Financing discount | MONKEY emission weight |
|---|---|---|---|---|---|---|
| Under $1M | 2 | A quarter of the first band | 5% | 10% | None | 1x |
| $1M, over 2 days or more | 5 | Half of the first band | 2% | 10% | None | 1.1x |
| $25M, over 5 days or more | 10 | The whole first band | None | 9% | 10% | 1.25x |
| $250M, over 10 days or more | 20 | 2.5 times the first band | None | 8% | 20% | 1.5x |

The best-price size is the part of the first band, by notional, that the account pays at the first
band's rate. The initial margin floor is the least margin rate the account's new positions must
post, so tier 0 trades at no more than 20x and tier 1 at no more than 50x. The best-price size of
tier 3 is at most the second band's bound. The study proposed leverage of a fifth and half of each
market's maximum at those tiers; the launch values use these floors instead.

**Counting.** Volume counts the notional of fills that change a position. It does not count a round
trip at the same index within the score's horizon, or any part of a position closed within 60
seconds of its opening time, so churn and wash trading held for seconds earn nothing. A single day
counts at most half of tier 1's threshold, a fifth of tier 2's and a tenth of tier 3's, so each tier
takes trading on that many days. A wallet that the toxicity score catches within a day stays at tier
0, whatever its size. Above tier 1, reaching a threshold with round trips that have no edge costs
more than a month of the tier's best-price benefit, because each fill pays a spread and the winning
side pays the fee.

**What recycling wallets gains and forfeits.** For the latency strategy at its best under the
study's proposals, moving to fresh wallets avoids the score's add-on, and forfeits the kept tier's
best-price size, leverage and wallets. The benefits forfeited are 35% to 70% of the add-on avoided,
so recycling still pays the strategy $1k to $31k a day per market. What it pays is the take that the
spreads and fee leave, which [the launch criterion](#the-launch-criterion) counts, not a gain over
the defences. The forfeited benefits leave out tiers 2 and 3's fee and financing discounts, which a
kept wallet's holds of 15 to 30 minutes would earn, so the figure leans towards recycling.

### Resting-order limits

The launch values let an account rest at most 25, 50, 100 and 250 orders at once at tiers 0 to 3.
`run.py order_limits` values these limits against wallet recycling, and measures what resting orders
cost a price, in [`results/order_limits.md`](results/order_limits.md).

**The limits are worth nothing against recycling.** The strategies that a recycled wallet runs,
latency arbitrage and unpriced pairs, trade with market orders. A resting order fills at the
engine's price once that price reaches it, which gives no lead over the venues. None of those
strategies rests an order, so none forfeits anything when a fresh wallet's limit falls to 25. What
recycling forfeits is the value of the other tier benefits and of the wallets, measured under the
1-second fee curve as the account's profit a day:

- with an honest trading server's 0.25-second lead, $2k to $11k a day, against $3k to $28k of add-on
  avoided;
- with the operator's 3-second view, under $500 a day, against $58k to $250k of add-on avoided.

**The limits do not bound the cost to the sequencer.** A price visits only the resting orders that
its mid has reached. With every one of 100,000 accounts holding tier 0's 25 limit orders away from
the price, a price's stamp stays at 0.11 ms at the median. The study also measures limit orders that
every price reaches but the spread keeps out of reach. On the engine it measured, each such order
stayed resting and was priced again on every price in its market:

- `crates/node/src/sequencer/resting_bench.rs` loads such orders into one market of a
  100,000-account sequencer. Each adds about 0.58 microseconds to every price.
- About 1,500 of them take a price's stamp to the 1 ms median budget that `price_wait_budget` sets.
  That is 61 wallets at tier 0's limit of 25, or 6 at tier 3's limit of 250.
- With every account at tier 0's limit, a price takes about 1.8 s.

The cost grows with the number of wallets, not with any one account's limit, so no per-account limit
bounds it. The study therefore proposed that the engine visit only the limit orders a price can
fill. The engine does so: it keys each resting limit order at the mid where its market's least fill
rate first lets it fill, and re-keys a market's limit orders when that rate changes. The measurement
ran on a shared machine under load, so the slope, not the absolute times, carries the result.

### Retained NAV and house income

The house keeps a floor of capital and distributes everything above it every hour. The retained NAV
is the least such floor that serves the flow with house cash below zero in at most 1% of paths. On
the corrected income model and the 34 launch markets, it is $107M on both flows. Over 90 days,
current flow pays $456M of spread income and $55M of profit fees, of which $30M comes from churn.
Target flow pays $135M of spread income and $36M of profit fees.

The first stage ran the cap calibration's retained-NAV model on 1,000 paths of 90 days and 37
markets, at the genesis margins with 100x on BTC-USD and ETH-USD:

| Flow and pricing | Retained NAV | Distributed, 90 days | Spread income | Profit fees | Operator's share a day |
|---|---|---|---|---|---|
| Current, spreads and the first proposed fee | $107M | $798M | $481M | $316M | $1.77M |
| Target, spreads and the first proposed fee | $107M | $217M | $142M | $73M | $483k |
| Current, no spread and a flat 10% fee | $107M | $76M | None | $74M | $168k |
| Target, no spread and a flat 10% fee | $107M | $54M | None | $52M | $119k |

The retained NAV does not move. In every variant the exposure caps' refusals set it: under 1% of
directional orders are refused at $107M, and about 4% at the grid's next floor down, $67M. House
cash stays above zero in at least 99% of paths at every floor. Spread income raises what the house
distributes, not what it must retain. Spread income assumes that the flow trades as it does at a
zero spread, so the target-flow rows are the more conservative yardstick.

### BTC-USD and ETH-USD at 100x

At launch, BTC-USD and ETH-USD charge 1% margin on a position's first $2M, which allows leverage of
100 times (100x) at tiers 2 and 3. The cap calibration's per-market stage runs each market alone at
its share of the flow, on 2,000 paths:

| Market | Margins and pricing | Refusals end at | NAV below zero at some point |
|---|---|---|---|
| BTC-USD | 10x, the cap calibration's results | $10.0M | 63% |
| BTC-USD | 100x, no spread and a flat 10% fee | $31.6M | 40% |
| BTC-USD | 100x, spreads and the first proposed fee | $31.6M | 0% |
| ETH-USD | 10x, the cap calibration's results | $5.6M | 65% |
| ETH-USD | 100x, no spread and a flat 10% fee | $17.8M | 44% |
| ETH-USD | 100x, spreads and the first proposed fee | $17.8M | 0% |

At 100x the caps bind on three times the notional, so refusals end at three times the floor. The
larger floor makes NAV below zero less likely, and with spreads and the fee it does not happen in
any path.

The hourly model closes positions out at their trigger, so it shows no bad debt from gaps in markets
that never close. The trade grids show that bad debt. A position is closed out with half its margin
left, so it loses past its balance when the index moves further than that half before the engine's
next price, which comes up to 3 seconds later. At 1% margin this happens for about 3 seconds a day
over the two markets, and the mean loss past the balance is then 0.5% of the position's notional.
Over the four stress hours of 10 October 2025, the same happens for 25 minutes. At 2% margin it
happens for under a second a day. At 5%, it never happens in the year's sample, and for 3 seconds in
the stress hours. [The attack classes](#the-attack-classes-at-the-first-proposals) give the unpriced
pairs at each margin. This study does not re-run the cap calibration's joint floors or utilisation
levels at 100x; the [launch-caps study](../launch-caps/README.md) does.

### The attack classes at the first proposals

The first stage valued each attack class with no spread on the first band, and at the study's first
proposals: the calibrated spreads, the fee that keeps the whole gain to 5 minutes, the toxicity
score and the volume tiers. Each figure is the operator's take a day at current flow, on the first
income model, from `results/report.md`:

- **Latency arbitrage on crypto, from the operator's 3-second view.** With no first-band spread,
  $8.41M a day with the toxicity add-on paid, or $64.95M if round trips launder the score. At the
  first proposals, $115k. Against target flow's share of $483k on this model, any schedule in the
  frontier with the whole gain kept to 1 second passes. Against the $169k share that the cap
  calibration found with no spread, it needs the whole gain kept to 10 seconds or more, and 10%
  reached no sooner than 15 minutes. A flat 10% leaves $3.17M.
- **Latency arbitrage on FX and indices at a 1 bp first band:** $87k a day over the ten markets, an
  upper bound, and zero on EUR-USD and GBP-USD.
- **Skimming client fills:** $159k a day at current flow. Spreads and the fee do not change it. It
  scales with client fills, so it is about a quarter of that at target flow.
- **Censoring orders that close positions, and the held sync's kill option:** no gain from
  censoring, and about $2k a day from the kill option.
- **Unpriced pairs:** $3k a day at 1% margin, within an interval from -$133k to $197k, which is
  about zero, but $19.13M over the four stress hours of 10 October 2025. At 2% margin or more, the
  pairs lose, stress day included.
- **Altering Databento quotes:** $360k a day at current flow, scaling with client fills. Only
  authenticated quotes or a second independent source stop it. The launch configuration takes no
  Databento quotes, as [Limits](#limits) explains.

Without the Databento quotes, the classes together take about $366k a day at current flow, against a
$1.77M share, and about $250k at target flow, against $483k. With the Databento quotes they take
about $725k and $340k. The fastest honest client, with a 0.25-second lead, still takes about $60k a
day over the crypto markets under the first proposal. Most of it comes from SOL-USD and ETH-USD
positions held 15 to 30 minutes after sub-second moves of 12 bps or more. That take is a cost to the
house rather than an operator's take, and the frontier gives it beside each schedule.

### The values chosen

| Question | The study's proposal | Launch value |
|---|---|---|
| Spread bands | `results/spread_tiers.toml` | The same bands on crypto, metals, oil and shares. FX and index first bands in tenths of a basis point, from 0.4 to 1.0 bps, proposed |
| Holding-time fee | The whole gain to 5 minutes, 10% at 30 minutes | The whole gain to 10 seconds, 10% at 15 minutes |
| Toxicity score | The decayed, weight-capped score, with tiers at scores of 4 and 8 | As proposed |
| Markout-spread floors | Half the year's markout spread | The full year's markout spread |
| Volume tiers | The thresholds, counting rules and benefits above, with leverage of a fifth and half the maximum at tiers 0 and 1 | As proposed, with initial margin floors of 5% and 2% in place of the leverage shares |
| Resting-order limits | 25, 50, 100 and 250, with the engine visiting only the limit orders a price can fill | As proposed |
| BTC-USD and ETH-USD | 2% margin, or a bound on how long a market stays unpriced | 1% margin at tiers 2 and 3, with unpriced pairs left to the loss-rate trigger |
| Databento quotes | Authenticate them, or require a second source | No market takes Databento quotes |

## Method

**Spreads and latency.** The latency strategy is one family of rules: it trades once the venues have
moved past a threshold within the lag, and holds for a set time. It is measured on the trade grids
with the later moves mirrored, so each take is the best of that family, not an optimum. Each take
pays the spread bands at the account's best-price size, and the toxicity add-on that its score sets
(`calib/latency.py`, `calib/spreads.py`). The FX and index takes scale BTC-USD's measured strategy
by each market's lag move.

**The corrected income model.** The engine charges the profit fee on realised profit at the
execution prices, which include the spread (`crates/core/src/execution/mod.rs`, `book`). A winning
short hold therefore pays the spread out of its gain. The cap calibration's model
(`research/cap-calibration/model/holdfee.py`) charges each churn class

```
fee(h) * sigma * sqrt(h) * psi(2 s / (sigma * sqrt(h)))
psi(u) = E[max(Z - u, 0)]
```

over its holds `h`, and charges a position held an hour or more the fee on its realised profit less
the spread. The fee is the engine's table of the curve: eleven points, linear between them, rounded
up to the basis point, and at most 0.81 percentage points above the log-linear curve.

The derived flow records each churn class's mean hold and mean square-root hold, not the
distribution, so the study bounds the fee:

- The lower bound takes the least fee that any distribution of holds with those two moments allows.
  It is a linear programme whose optimum sits on at most three holds.
- The central estimate tilts the measured within-hour holding profile to each class's moments.
- Every account pays tier 3's fee, which is the least any tier pays past the curve.

Every account trades at the market's own leverage, as tiers 2 and 3 do. An account below tier 2 pays
a higher fee, but holds less notional where the market allows more than 20x at tier 0, or more than
50x at tier 1. The flow data does not say how volume spreads across tiers. Tier 2 takes $25M of
counted volume over five days or more.

**The house model.** The retained NAV and the per-market runs are the cap calibration's hourly house
simulation. The cap calibration's `Scenario` takes two fields for this study, a spread per market
and a fee schedule, which leave its own results unchanged. Both sibling studies name their code
`model`, so one process loads only one of them (`calib/common.py`). This study's own code is in
`calib/`.

## Data and sources

The study adds no data. It reads:

- the [rogue-operator study](../rogue-operator/README.md)'s 50 ms price grids of Coinbase and Kraken
  trades, over 156 hours sampled by volatility stratum from a year of BTC-USD, ETH-USD and SOL-USD,
  with the four worst hours of 10 October 2025, its stratified year estimates and its attack
  measures;
- the [cap calibration](../cap-calibration/README.md)'s hourly bars for every launch market, its
  client flow, carried over by margin from papertrade.xyz's on-chain trading with its holding-time
  profile, leverage and sizes, and its hourly house simulation;
- `config/production-genesis.toml`, for the launch markets, their size bounds and their margins;
- `results/resting_bench.txt`, the `WAIT` lines of the sequencer benchmark that
  [Re-running](#re-running) names.

## Limits

- **Three markets measured.** The spreads, fee and score are measured on the trades of three crypto
  markets. The other markets' moves are scaled from hourly ranges, and their latency figures from
  BTC-USD's.
- **The flow does not respond to prices.** Spread income assumes that the flow trades as it does at
  a zero spread. Target flow reshapes the measured flow, and is not a measured target client.
- **One family of strategies.** The latency strategy is one rule family, measured with its later
  moves mirrored, so each take is per rule, not an optimum. The score's and size bands' effects on
  it are bounds.
- **Holds by notional.** The holding-time profile is by notional, and the study applies it to margin
  classes.
- **Tiers below tier 2 are not modelled** in the corrected income model, as [Method](#method)
  describes.
- **Adding to a position.** The study's hold clock averages the opening time over every unit. The
  engine averages it the same way, but counts units already held as no more than 15 minutes old, so
  adding to an old position cannot borrow more than 15 minutes of age.
- **Floors outside crypto.** The markout-spread floors outside crypto rest on the crypto markets'
  ratio of measured to scaled markout spread.
- **Databento.** The study's Databento class models quotes read without TLS. The trading server
  reads every venue and market-data vendor over TLS that ends in the enclave, and the launch
  configuration takes no Databento quotes, so the class does not apply to it.
- **Resting orders.** The cost of resting orders was measured on an engine that priced every limit
  order out of the spread's reach again on every price. The engine keys each limit order where it
  can first fill, so a price does not visit such orders.

## Re-running

From a virtualenv outside the repository with `requirements.txt`, in this directory, each stage
reads committed data and earlier stages' results:

```shell
python -I run.py spreads
python -I run.py latency --workers 3
python -I run.py scaled --workers 3
python -I run.py fee
python -I run.py frontier
python -I run.py fx_tiers
python -I run.py toxicity
python -I run.py floors
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
python -I run.py order_limits
python -I -m pytest tests
```

`report`, `owner_report` and `order_limits` read earlier results and finish in seconds, and `floors`
in under a minute. Each rewrites its committed files unchanged.

`order_limits` reads `results/resting_bench.txt`, which holds the `WAIT` lines of the following
command, run from the repository root in a quiet CPU window:

```shell
cargo test -p engine-node --lib sequencer::resting_bench -- --ignored --nocapture --test-threads=1
```

`owner_income` keeps the variants already in `results/owner_income.json`. The second `owner_income`
command therefore runs the sensitivities at 500 paths beside the two 1,000-path headline runs.
`retained`, `per_market` and `owner_income` run the cap calibration's corrected model. Re-running
`retained` and `per_market` therefore gives the first proposals on the corrected model, not the
committed figures, which the first model produced.
