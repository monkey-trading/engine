# Launch exposure caps

This study sets the house's exposure caps for the 34 launch markets, at the pricing that ships. A
market's exposure cap limits the net position that the house may hold against its clients in that
market, and the total cap limits the house's net positions across all markets added together,
whatever their direction. Each cap is the larger of a share of the house's net asset value (NAV) and
a dollar floor, so that trading can start from a house with no capital. The study answers three
questions: how large each market's floor should be, how large the total floor should be, and at what
share of its cap a market's stress level 1 should start to widen spreads.

The launch values are the study's proposal, rounded to three figures. The floors are $31.6M on
BTC-USD, $17.8M on ETH-USD, US500 and US100, $10M on the other FX, index and commodity markets,
$5.62M on SOL-USD and $3.16M on each share, $244M in all. The total floor is $61M. Stress level
1 fires when the house's net position in a market reaches 80% of the market's cap. On the
[cap calibration](../cap-calibration/README.md)'s client flow, no set of floors from $100k to $30M a
market puts house NAV or house cash below zero in any of 2,000 paths, because every trade that opens
a position pays its spread to the house as it is executed. The floors therefore trade refused orders
against the exposure that the house carries with no capital behind it. The launch floors are the
least that serve every market's flow, and refuse 0.1% to 0.3% of directional notional over 90 days.

A pair of opposite positions in two accounts leaves the house's net exposure unchanged, so no cap
limits it. The study measures two such attacks. At 1% margin on BTC-USD and ETH-USD, pairs opened
ahead of a move that the operator leaves unpriced take $12.9M of house loss over the four stress
hours of 10 October 2025. Pairs held across weekends pay at the market's own leverage on WTI, Japan
225, silver and US Tech 100. The weekend margins that stop them, 7.5% on WTI, 1.5% on Japan 225 and
silver, and 1% on US Tech 100, are the launch values.

Rates are given in basis points, abbreviated to bps. One basis point is one hundredth of one
percent, so a utilisation level of 8000 bps is 80% of the cap.

- [Results](#results): [the launch values](#the-launch-values), [floors](#floors),
  [the total floor](#the-total-floor), [floors on target flow](#floors-on-target-flow),
  [floors under the main sensitivities](#floors-under-the-main-sensitivities),
  [utilisation levels](#utilisation-levels),
  [retained NAV and the honest share](#retained-nav-and-the-honest-share),
  [paired positions across unpriced moves](#paired-positions-across-unpriced-moves),
  [paired positions across session gaps](#paired-positions-across-session-gaps)
- [Method](#method)
- [Data and sources](#data-and-sources)
- [Limits](#limits)
- [Re-running](#re-running)

## Results

Every stage runs at both ends of the volume-tier range. At tier 0, every account is a new wallet,
which pays the most spread and fee per unit of notional and takes at most 20x. At tier 3, every
account pays the least and takes the market's own leverage. A set of values meets a target only if
it meets it at both ends. [Method](#method) explains why.

### The launch values

| Setting | Launch value |
|---|---|
| `house_cap_floor` on BTC-USD | $31.6M |
| `house_cap_floor` on ETH-USD, US500 and US100 | $17.8M |
| `house_cap_floor` on EUR-USD, GBP-USD, USD-JPY, AUD-USD, US30, JP225, XAU-USD, XAG-USD and WTI | $10M |
| `house_cap_floor` on SOL-USD | $5.62M |
| `house_cap_floor` on each of the 20 shares | $3.16M |
| The sum of the market floors | $244M |
| `house_total_cap_floor` | $61M |
| Level 1 `utilisation_bps` | 8000 |
| Level 2 `utilisation_bps` | 9500 |
| Weekend margin | 7.5% on WTI, 1.5% on JP225 and XAG-USD, 1% on US100 |
| House capital at launch | None |

The study's floors are values of the cap calibration's floor grid, which the launch values round to
three figures. `config/production-genesis.toml` holds each of these values.
[`results/proposal.toml`](results/proposal.toml) holds the proposed floors and levels in the
genesis's fields, and [`results/report.md`](results/report.md) holds every stage's tables.

### Floors

The cap calibration's per-market sweep and joint run are repeated at each end of the tier range, on
2,000 paths of 90 days, from a house with no capital, with seed 1, on the 34 launch markets. The
candidates are each measure's risk-level floors from each end's per-market sweep, and the same floor
on every market from $100k to $30M. Each candidate runs on both ends' paths
(`results/tier0/joint.csv` and `results/tier3/joint.csv`).

**No set of floors puts the house below zero on the cap calibration's flow.** At both ends, from
$100k on every market ($3.4M in total) to $30M on every market ($1.02B), no path has house NAV or
house cash below zero at any hour. The one-sided 95% bound on that rate is under 0.15%. At its 1st
percentile, the lowest NAV of a path is positive: $25k to $119k at tier 0, and $46k to $384k at tier
3.

Without spreads or the holding-time fee, the same model puts NAV below zero in 0.85% of paths at $1M
a market, and in 5.85% at $10M a market. Every trade that opens a position pays its spread to the
house as it is executed. Over 90 days, spreads bring $143M to $155M at tier 0 and $457M to $490M at
tier 3, against $22M to $58M of profit fees, so the house is ahead from its first hour. The floors
therefore trade refusals against the exposure that the house may carry with no capital behind it,
and targets of 1%, 0.1% and 0.01% all choose the same set.

**The proposal** is the smallest set that also serves every market's flow. It refuses at most 1% of
each market's directional notional over the 90 days at both ends (`house.propose`). Each market's
floor is the one at which its own refusals end at tier 3, $243.8M in total.

| Floors | Total | Refused in launch week, tier 0 / tier 3 | Refused over 90 days | Worst market over 90 days | Median NAV on day 90, tier 0 / tier 3 |
|---|---|---|---|---|---|
| $100k on every market | $3.4M | 48.5% / 55.5% | 7.0% / 6.7% | 7.8% / 7.3% (BTC-USD) | $166M / $513M |
| $1M on every market | $34M | 29.1% / 37.7% | 4.4% / 4.5% | 5.2% / 5.2% (BTC-USD) | $171M / $525M |
| Each market's floor at which it alone never dips, at most where its refusals end at tier 3 | $147M | 2.6% / 3.2% | 0.3% / 0.4% | 2.6% / 1.5% (WTI) | $179M / $548M |
| **Each market's floor where its refusals end at tier 3 (launch)** | **$244M** | **0.2% / 2.4%** | **0.1% / 0.3%** | **0.4% / 0.4% (WTI, BTC-USD)** | **$180M / $549M** |
| $10M on every market | $340M | 5.6% / 12.3% | 1.1% / 1.5% | 1.6% / 2.3% (BTC-USD) | $178M / $543M |
| $30M on every market | $1.02B | 0.0% / 2.2% | 0.0% / 0.3% | 0.0% / 0.5% (BTC-USD) | $180M / $549M |

- **The house's exposure stays far below the floors.** The flow nets out. The house's gross net
  exposure is the sum of its net positions in every market, whatever their direction. The median
  over paths of its largest value is $32M at tier 0 and $66M at tier 3 at the launch floors, and
  $24M and $59M at $100k a market. A floor binds mostly through the size of the largest order it
  admits, so most of the $244M is never carried at once.
- **Smaller sets refuse more in launch week and on single markets.** The $147M set gives 12 share
  markets no floor at all, so they refuse every order until house NAV builds their share of the cap,
  and it leaves WTI refusing 2.6% over 90 days. A floor of $1M a market, the cap calibration's
  choice without spreads, refuses 29% to 38% of launch-week notional.
- **Without a floor,** every cap is zero while house NAV is zero. No trade that opens a position
  could then be executed, and utilisation would read full on every market.

### The total floor

At the launch market floors, a total floor of 25% of their sum ($61M) up to 100% ($244M) leaves
every measure unchanged at tier 0. At tier 3, a total floor of 25% raises launch-week refusals from
2.40% to 2.57%, and every market still refuses under 0.5% over the 90 days
(`results/tier*/total.csv`). The median of the house's largest gross net exposure is $32M at tier
0 and $64M to $66M at tier 3, so a total floor much below 25% would start to bind. The launch value
is the smallest share tested that meets the target and serves every market at both ends, a
`house_total_cap_floor` of $61M.

### Floors on target flow

Target flow is the [spread calibration](../spread-calibration/README.md)'s reshaping of the client
flow towards clients who hold positions for 15 minutes or more. It keeps the flow held an hour or
more, the flow held 15 to 60 minutes, and a tenth of the rest (`results/tier*/joint_target.csv`).
That flow brings a third of the spread income, and an early dip in NAV returns:

| Floors | NAV below zero at some hour, tier 0 / tier 3 | Cash below zero | NAV below -$100k | NAV below -$1M | Lowest NAV, 1st percentile | Refused in launch week |
|---|---|---|---|---|---|---|
| $1M on every market | 0.20% / 0.40% | 0.20% / 0.40% | 0 / 0 | 0 / 0 | $10k / $15k | 38.9% / 63.0% |
| $3M on every market | 0.25% / 0.85% | 0.25% / 0.65% | 0 / 0.20% | 0 / 0 | $13k / $10k | 21.0% / 40.8% |
| **Launch ($244M)** | **3.25% / 3.00%** | **0.20% / 0.95%** | **1.10% / 1.80%** | **0 / 0.50%** | **-$115k / -$462k** | **0.3% / 2.6%** |
| $10M on every market | 1.70% / 1.15% | 0.20% / 0.90% | 0.20% / 0.35% | 0 / 0 | -$17k / -$14k | 5.8% / 14.3% |

- **At the launch floors, house cash falls below zero in under 1% of paths on target flow,** and in
  0.95% at tier 3. NAV falls below zero in 3% of paths, within the first hours. The median first dip
  comes 2 to 6 hours after launch, when the house holds exposure but has collected few spreads. The
  dip is shallow. At its 1st percentile the lowest NAV is -$115k at tier 0 and -$462k at tier 3, and
  it passes -$1M in 0.5% of tier 3's paths.
- **Only small floors hold NAV below zero to under 1% of paths on target flow.** Every set from
  $100k to $3M a market does so, at 0.2% to 0.85%, but refuses 21% to 87% of launch-week notional.
  At $3M a market, launch-week refusals are 21% to 41%.
- **Seed capital removes the dip at the launch floors, which floors cannot do.** Opening capital of
  about $0.5M brings the dip to about 1% of paths, and $1M brings it to 0.5% at tier 3. The house
  launches with no capital.
- On target flow, `propose` finds no set that meets 0.1% on either measure (`results/proposal.json`,
  the `@target` keys). At 1%, the house-cash measure keeps every market under 1% refused only at
  $30M a market, and the NAV measure's set that refuses least is $3M a market.

### Floors under the main sensitivities

The launch floors, and 0.3 times them, run under the cap calibration's main sensitivities on
1,000 paths (`results/tier*/sensitivity.csv`):

| Scenario, at the launch floors | NAV below zero at some hour, tier 0 / tier 3 | Cash below zero | Mean bad debt over 90 days |
|---|---|---|---|
| Baseline | 0 / 0 | 0 / 0 | $74k / $418k |
| Target flow | 3.2% / 3.0% | 0.3% / 1.0% | $74k / $418k |
| A quarter of the flow | 0 / 0 | 0 / 0 | $18k / $104k |
| No farming, so no hedged flow | 0.3% / 0.2% | 0 / 0 | $15k / $120k |
| Twice the volatility | 0.3% / 0.1% | 0 / 0 | $528k / $1.47M |
| Every client on one side in every market | 0 / 0 | 0 / 0 | $74k / $417k |

At 0.3 times the floors, target flow puts NAV below zero in 1.4% and 1.2% of paths, and cash in 0.4%
and 1.1%. Every other scenario stays at or below 0.2%. A quarter of the flow is safe because it
keeps churn, the flow held for under an hour, whose spreads come with little exposure. Target flow
removes most of the churn.

### Utilisation levels

Utilisation is the share of its cap that the house's net position in a market uses. The cap
calibration's utilisation stage (`model/stress.py`) runs at each end on 2,000 paths. For each market
and candidate level, it counts the share of open hours in which the book left at the end of the hour
sits at or above that level of the cap.

For level 1, the rule takes the lowest candidate whose fire rate stays under 2%, weighting each
market by its share of the flow. Every market that carries 1% of the flow or more must also stay
under 5%. Level 2 stays at 9500, the last widening before refusals start. A level must pass at both
ends.

| Level 1, bps | Tier 0, flow-weighted (BTC-USD) | Tier 3, flow-weighted (BTC-USD) |
|---|---|---|
| 5000 | 0.36% (0.7%) | 6.93% (11.7%) |
| 6000 | 0.08% (0.1%) | 4.38% (7.5%) |
| 7000 | 0.01% (0.0%) | 2.65% (4.7%) |
| 7500 | 0.00% (0.0%) | 2.00% (3.6%) |
| **8000, launch** | **0.00% (0.0%)** | **1.47% (2.7%)** |
| 8500 | 0.00% (0.0%) | 1.02% (1.9%) |
| 9000 | 0.00% (0.0%) | 0.60% (1.1%) |
| 9500, level 2 | 0.00% (0.0%) | 0.22% (0.4%) |

- **Level 1 at 8000.** At the launch floors, tier 0's lower leverage leaves every cap nearly empty,
  and tier 3 passes from 8000. A level of 7000 fires in 2.65% of tier 3's open hours. BTC-USD, which
  carries most of the flow, is the market that fills its cap. Without spreads, no level below
  9500 passes at 100x. Larger floors, not pricing, make the difference.
- **Smaller floors fire constantly.** At $1M a market, level 1 at 8000 fires in 6.1% of tier 0's
  open hours and 4.0% of tier 3's. At $10M it fires in 3.1% of each, and the rule's choice at $10M
  is 9000.
- **The other triggers do not depend on the floors.** The fast-move thresholds, 75 and 200 bps over
  60 seconds, come from each market's volatility. The divergence shares, 6001 and 9500, come from
  the genesis's price rules. The cap calibration's stress stage sets both on the 34 launch markets.

`results/tier*/stress_utilisation.csv` holds every market's rates, and `stress_choice.json` the
rule's choice for each family of floors.

### Retained NAV and the honest share

In the retained-NAV run (the cap calibration's `model/retained.py`, through `house.income`), the
house opens with each floor of the retained grid as its capital. It distributes everything above
that floor every hour, and trades under the genesis's caps as shares of NAV, on 1,000 paths, on
current flow and on target flow. The operator's honest share is 20% of what the house distributes.

| Tier and flow | Retained NAV | Spread income, 90 days | Profit fees | Distributed | Honest share a day |
|---|---|---|---|---|---|
| Tier 0, current | $67.1M | $154M | $23.4M | $179M | $397k |
| Tier 3, current | $107M | $489M | $57.7M | $548M | $1.22M |
| Tier 0, target | $67.1M | $49.2M | $17.7M | $68.0M | $151k |
| Tier 3, target | $107M | $145M | $36.3M | $183M | $407k |

The attack classes are the [rogue-operator study](../rogue-operator/README.md)'s. Each is counted as
the operator's take, which is 80% of its own accounts' profit. The latency takes, the held sync and
skimming are the spread calibration's figures under the launch fee curve, which takes the whole gain
on a position closed within 10 seconds. The pairs are this study's.

| The operator's take a day | Current flow | Target flow |
|---|---|---|
| Latency arbitrage on crypto, from a 3-second view | $160k | $160k |
| Latency arbitrage on FX and indices, at the proposed first bands | $63k | $63k |
| Unpriced pairs at 1% margin, tier 2, opened ahead, before financing (an upper bound) | $64k | $64k |
| Pairs across session gaps, $50M a side on each market where they pay, tier 2 | $55k | $55k |
| The held sync's kill option | $2k | $2k |
| Skimming client fills, which the loss-rate trigger cannot see | $159k | $47k to $51k |
| **Every class together** | **$504k** | **$391k to $395k** |
| Honest share, every account at tier 3 | $1.22M | $407k |
| Honest share, every account at tier 0 | $397k | $151k |

- **At tier 3,** the launch criterion holds on both flows before the
  [loss-rate trigger](../loss-rate-trigger/README.md) acts. On target flow it holds by $16k a day,
  with both pair classes at their upper bounds. At first-band spreads and a fee curve that takes the
  whole gain only to 1 second, the spread calibration finds a share of $385k on target flow. The
  spread bands and the 10-second curve raise it to $407k.
- **If every account were a new wallet,** the share would fall to $397k on current flow and $151k on
  target flow, because 20x buys a fifth of the notional on most of the flow. The classes then take
  more than either share. The share rises as accounts earn tiers. The study brackets how fast that
  happens but does not measure it.
- **The retained NAV** is $107M at tier 3, as the spread calibration finds, and $67.1M at tier 0,
  whose lower leverage needs less NAV behind the same caps.
- **The fastest honest client** still takes $102k a day under the 10-second curve. The loss-rate
  trigger study finds that the trigger leaves $101k of it.

### Paired positions across unpriced moves

The rogue-operator study's operator holds a long position in one account and a short position in
another, on BTC-USD or ETH-USD. It leaves the market unpriced as a fast move starts, and prices it
again once the move has run. The losing position's close-out then runs past its balance, and the
loss beyond the balance falls on the house. `caps/unprice.py` runs this attack at the launch rules,
as [Method](#method) describes.

House loss at $50M a side over BTC-USD and ETH-USD, for the rule that unprices for up to 300 seconds
once the venues move 40 bps in 5 seconds (`results/unpriced_pairs.json` has every rule; this one is
the attacker's best on the year in every variant):

| Accounts | Opening | Closing level | A day over the year | 95% interval | Four stress hours of 10 October 2025 |
|---|---|---|---|---|---|
| Tier 2, 9% fee | At the trigger | The engine's level | -$20.6k | -$187k to $227k | -$4.67M |
| Tier 2 | At the trigger | The move's level | -$34.7k | -$200k to $210k | -$0.40M |
| Tier 2 | Standing, at level 0 | The engine's level | $80.3k | -$121k to $397k | $12.9M |
| Tier 2 | Standing, at level 0 | The move's level | $66.2k | -$135k to $380k | $17.2M |
| Tier 3, 8% fee | At the trigger | The engine's level | -$11.3k | -$181k to $245k | -$3.91M |
| Tier 3 | Standing, at level 0 | The engine's level | $90.0k | -$116k to $416k | $13.9M |
| Tier 3 | Standing, at level 0 | The move's level | $75.3k | -$131k to $399k | $18.2M |

- **Only pairs opened ahead pay.** When the pair is reopened before each spell, at the crash's
  raised stress level, every variant loses over the stress hours. Opened ahead, tier 2's pairs take
  $12.9M over the stress hours. That is 136 pairs of $50M a side, about $95k each, with $1M of
  margin and 50 to 100 tier-2 accounts behind each pair. The operator keeps 80% of its accounts'
  take, $10.3M of the $12.9M.
- **On the year,** pairs opened ahead take $66k to $90k a day before financing, and the intervals
  span zero. A pair that stands ready across each day's 22:00 rollover pays about $13.7k a day of
  financing at $50M a side, which is the 5% financing markup on both positions. These figures leave
  that financing out.
- **The figures agree with the other studies' under their conventions.** On the same hours, this
  code gives the spread calibration's $30.6M under its rules: the size band not multiplied by the
  stress level, every move counted from 3 seconds before its spell, and each position closed at the
  move's level (`check_697`). It gives the loss-rate trigger study's -$0.58M for pairs reopened
  before each spell and $18.0M for pairs opened ahead, under that study's rules (`check_734`).
- **The remaining differences.** Both other studies charge the fee on the gain less the closing
  spread, while the engine charges it net of the opening spread too, so tier 3's $18.2M at the
  move's level is $0.14M above the loss-rate trigger study's $18.0M. Tier 2's $12.9M differs from
  that figure by tier 2's 9% fee and by the engine's closing level. The engine holds level 2 through
  its cooldown more often than the move alone reads it, in 83 of 136 closes against 8.

The launch values keep 1% margin on BTC-USD and ETH-USD at tiers 2 and 3. The
[loss-rate trigger](../loss-rate-trigger/README.md) study measures what the trigger leaves of these
pairs.

### Paired positions across session gaps

A pair opened at the last hour's close before a market closes, and closed at the open when it
reopens, leaves the house's net exposure unchanged. No cap limits it except through the size of each
order, and the size of an order repeats across as many accounts as the attacker holds. Each
position's loss stops at its balance, so a gap past the balance falls on the house, and the other
position keeps its gain less the fee.

The pair's payoff a year at each tier, and the house's bad debt on it at tier 2, in bps of each
position's notional, for a pair held across every closure. The attacker's gain is the house's loss.

| Market | Closures a year | Mean gap | Tier 0 (20x) | Tier 1 (50x) | Tier 2 | Tier 3 | Past a tier-2 position's balance | House's bad debt, tier 2 |
|---|---|---|---|---|---|---|---|---|
| EUR-USD | 57 | 13 bps | -237 | -237 | -176 | -169 | 2.7% | 53 |
| GBP-USD | 56 | 11 bps | -225 | -225 | -176 | -170 | 3.6% | 43 |
| USD-JPY | 56 | 17 bps | -284 | -284 | -184 | -175 | 4.6% | 91 |
| AUD-USD | 54 | 16 bps | -263 | -263 | -140 | -132 | 5.7% | 115 |
| US500 | 259 | 10 bps | -1,462 | -1,462 | -920 | -897 | 5.1% | 519 |
| US100 | 258 | 14 bps | -1,750 | -1,729 | -797 | -765 | 7.5% | 922 |
| US30 | 260 | 8 bps | -1,422 | -1,406 | -1,044 | -1,025 | 3.7% | 359 |
| JP225 | 259 | 17 bps | -1,937 | -1,631 | -298 | -258 | 7.7% | 1,599 |
| XAU-USD | 256 | 12 bps | -2,795 | -2,780 | -2,487 | -2,462 | 2.4% | 284 |
| XAG-USD | 256 | 23 bps | -3,023 | -2,201 | -1,015 | -963 | 7.2% | 2,008 |
| WTI | 267 | 38 bps | -2,349 | -357 | 1,271 | 1,364 | 9.0% | 4,752 |

A selective attacker holds pairs only across the closures that pay. Across weekends alone:

| Market | Closures a year | Mean gap | Tier 0 (20x) | Tier 1 (50x) | Tier 2 | Tier 3 | Past a tier-2 position's balance | House's bad debt, tier 2 |
|---|---|---|---|---|---|---|---|---|
| EUR-USD | 52 | 14 bps | -228 | -228 | -168 | -161 | 2.9% | 53 |
| GBP-USD | 52 | 12 bps | -215 | -215 | -166 | -161 | 3.9% | 43 |
| USD-JPY | 52 | 18 bps | -274 | -274 | -174 | -166 | 4.9% | 91 |
| AUD-USD | 52 | 17 bps | -260 | -260 | -137 | -128 | 5.9% | 115 |
| US500 | 52 | 30 bps | -513 | -513 | -51 | -36 | 20.6% | 447 |
| US100 | 52 | 38 bps | -599 | -578 | 146 | 165 | 27.5% | 725 |
| US30 | 52 | 26 bps | -492 | -476 | -145 | -132 | 15.7% | 334 |
| JP225 | 52 | 51 bps | -682 | -376 | 717 | 743 | 28.4% | 1,374 |
| XAU-USD | 52 | 36 bps | -809 | -794 | -524 | -507 | 8.8% | 268 |
| XAG-USD | 52 | 73 bps | -946 | -356 | 545 | 582 | 28.4% | 1,508 |
| WTI | 52 | 139 bps | -117 | 1,815 | 3,225 | 3,295 | 33.3% | 4,497 |

- **Pairs lose on every FX market at every tier, and on every market at tier 0.** At 20x, a pair
  needs a gap of 5% past the spreads. At tier 1's 50x, only WTI's weekends pay.
- **At the market's own leverage, weekend pairs pay on WTI, Japan 225, silver and US Tech 100.** A
  pair held over every WTI weekend at 100x returns about a third of its positions' notional a year,
  sixteen times the margin it posts, and the house's bad debt on it is 45% of that notional a year.
- **The take scales with capital.** It is a return on the margin posted, not a figure that a cap
  bounds. At $50M a side on each market, across each kind of closure that pays at tier 2, the house
  loses about $69k a day.
- **The margin that ends it.** At tier 3's fee, the least that any tier pays, weekend pairs stop
  paying once each position holds 1% on US Tech 100, 1.5% on Japan 225 and silver, and 7.5% on WTI.
  At 1.5%, silver is about even. At 5%, WTI's weekends still pay 0.2% of a position's notional a
  year. At their own first bands, every other market's weekend pairs already lose
  (`results/session_gaps.json`, `weekend_breakeven`). These weekend margins are the launch values,
  and tier 0's limit of 20x already stops the pairs.
- **WTI's weekend gaps come from a series that rolls on its own calendar.** The public front-month
  series switches contracts on its own calendar, which the engine's roll-adjusted index does not, so
  a gap in the series may include a change of contract. The largest of WTI's 102 weekend gaps is
  +21%, on 6 March 2026 (`results/session_gaps.json`, `largest`).

## Method

**The launch pricing.** `caps/pricing.py` builds what each volume tier pays from
`config/production-genesis.toml`:

| Tier | Best-price size | Initial margin floor | Profit fee past the curve |
|---|---|---|---|
| 0 | A quarter of the first spread band | 5% (20x) | 10% |
| 1 | Half of the first band | 2% (50x) | 10% |
| 2 | The whole first band | None | 9% |
| 3 | 2.5 times the first band, at most the second band's bound | None | 8% |

- **Spreads** are the genesis's bands, from the spread calibration, with FX and index first bands in
  tenths of a basis point. Every fill pays the bands on the part of the position it opens or closes,
  so a position opened and closed in one order pays its bands twice. The cap model prices each order
  on its own notional (the cap calibration's `Scenario.spread_bands`). In each hour, the orders that
  fit within a side's remaining room under the cap pay their own mean rate, so a small floor
  executes only small orders, at the first band. Each cell of the size profile pays the rate at its
  lower bound. A position pays its closing spread when it closes, out of what it leaves its client,
  so the house's early cash holds only the opening spreads of the positions still open.
- **The closing spread cap** never binds at stress level 0. Every market's widest band is below its
  closing spread cap, for example 8 bps against 30 on BTC-USD, and 12 against 27 on the widest share
  (`pricing.check_ceilings`). The model prices every fill at level 0, which is the least a fill pays
  at any level, even with the cap applied.
- **The fee** is `max(base x (1 - discount), C(hold))`, on the engine's table of the launch curve,
  which takes the whole gain to 10 seconds and 10% at 15 minutes (`calib.fee.curve_points`). It is
  charged on realised profit after both spreads, as the spread calibration's corrected income model
  charges it. The genesis's `profit_fee_curve` is the same table, to the millisecond.
- **Leverage.** The tier's margin floor applies to the initial requirement only, and close-outs keep
  the market's bands. At tier 0, every market that allows more than 20x takes a flat 5% for new
  risk: BTC-USD, ETH-USD and the FX, index and commodity markets. The market's close-out level stays
  at half of its first margin band, so a tier-0 position on BTC-USD loses 4.5% before it is closed
  out.

**The tier distribution.** The derived flow has no account volumes, so how the flow spreads across
tiers is not known. Every stage therefore runs at both ends of the range. Every account starts at
tier 0, and tier 2 takes $25M of counted volume over five days or more, so the house's first days,
when the floors carry it, belong to tier 0. A set of floors meets a target only if it meets it at
both ends.

**Tier 0's flow profile is approximated.** The derived data has no client-flow profile for a flat 5%
schedule (`pricing.with_flat_schedule`). The papertrade.xyz leverage bins above 50x buy exactly 20x
per unit of margin there. The bin from 1x to 50x buys `E[min(L, 20)]`, which lies between what it
buys at 10x, SOL-USD's schedule, and its mean on papertrade.xyz. Those bounds are 13.3 to 16.5 for
directional flow and 17.1 to 18.5 for hedged flow, and the model takes the midpoints, 14.9 and 17.8.
Order sizes up to the 10x schedule's $1M bound scale by the kind's change in notional per unit of
margin, a factor of 1.96. That schedule's bands cut larger orders, so their margin buys 20x.

**The house-model stages.** `caps/house.py` runs the cap calibration's per-market sweep, joint run,
total-floor sweep, utilisation stage, sensitivities and retained-NAV run at each tier.
`house.propose` chooses the floors across the two ends.

**Paired positions across unpriced moves.** `caps/unprice.py` keeps the rogue-operator study's
trigger rule. The operator unprices a market once the venues have moved 40 bps within 5 seconds. It
prices the market again once the move has covered the margin divided by one less the fee, plus 0.3%,
or after 300 seconds. The study applies the launch rules to it:

- **Accounts.** A margin of 1% needs tier 2 or above, and holds only on each account's first $2M.
  Each position stays within its tier's best-price size, $2M on BTC-USD and $1M on ETH-USD at tier
  2, so every fill pays the first band, 2 bps on BTC-USD and 3 on ETH-USD. A pair of $50M a side
  therefore spans 25 tier-2 accounts a side on BTC-USD and 50 on ETH-USD. The winning position pays
  the fee past the curve, as if the pair had stood for 15 minutes.
- **Balances.** The engine admits a fill only if the account's equity after it, at the mid, covers
  the requirement (`check_risk`). Each account therefore holds 1% plus its opening spread, and that
  is the most its position loses. The fee is on the realised profit at the execution prices.
- **Stress spreads.** A stress level multiplies the size spread. A fill pays the level's minimum
  plus the band times its multiplier: 2 bps plus twice the band at level 1, and 10 bps plus four
  times the band at level 2.
- **The move.** The engine keeps a source's quote only if it is no older than the one it holds
  (`marks.rs`, `store_quotes`). A spell's move therefore counts from no earlier than the market's
  last repricing. The rogue-operator study's rule counts it from 3 seconds before every spell.
- **The opening.** In the rogue-operator study's run of the rule, each spell opens a new pair just
  before it, at the level that the engine's index sets then (`trigger`). A pair opened ahead has
  stood for 15 minutes and was opened in a calm market, at level 0 (`standing`). There is one per
  spell, each $50M a side.
- **The close.** Both positions close at one level, including the loser's close-out. The engine's
  level is the level the engine sets at the repricing (`stress.rs`). Its move window keeps only the
  trailing 60 seconds, so a market unpriced for longer reads no move when it is priced again. A
  raised level steps down only once 300 seconds have passed since it last changed, so in a crash a
  short move can still close at level 2. The move's level is the level the move itself reads as,
  which is the convention of the spread calibration and the loss-rate trigger study.

**Paired positions across session gaps.** `caps/gaps.py` prices each closure in the cap
calibration's hourly bars per unit of each position's notional, for each tier:

- The balance is the tier's initial requirement plus the opening fill's spread, which the account's
  equity must cover after the fill (`check_risk`).
- Both fills of both positions pay the first band.
- The winning position pays the fee past the curve on its realised profit.
- Each position pays financing at every rollover it crosses.

## Data and sources

The study adds no market data. It reads:

- the [cap calibration](../cap-calibration/README.md)'s hourly bars for every launch market, over
  102 weeks to 12 October 2026, from Binance for the crypto markets and Yahoo Finance for the rest;
- the cap calibration's client flow, ten days of papertrade.xyz's trading at up to 1000x, carried
  over by margin, and its house model;
- the [rogue-operator study](../rogue-operator/README.md)'s year estimates from 156 sampled hours of
  venue trades, and the four stress hours of 10 October 2025;
- the [spread calibration](../spread-calibration/README.md)'s target flow (`calib/target.py`), fee
  curve (`calib.fee.curve_points`), and latency, held-sync and skimming figures
  (`owner_latency.json` and `owner_fx.json`);
- `config/production-genesis.toml`, for the launch markets, their bands, margins and caps.

## Limits

- **The flow is papertrade.xyz's.** The client flow, with its sizes, holds and leverage, is the cap
  calibration's: ten days of a crypto venue at up to 1000x, carried over by margin. Its limits apply
  here. BTC-USD's and ETH-USD's 100x profiles are approximated from the 100x oil schedule, and tier
  0's from the 10x schedule, as [Method](#method) describes.
- **The tier distribution is bracketed, not measured.** Tiers 1 and 2 lie between the two ends in
  every input, but the floors' risk need not change monotonically between them.
- **Spread income is a lower bound, given the flow.** Fills are priced at stress level 0 and pay no
  toxicity add-on, and each size cell pays its lower bound's rate. The flow trades as it does at no
  spread. Target flow and a quarter of the flow are the sensitivities that bound this.
- **The fee at tier 3** is the least that any tier pays past the curve. Churn's fee takes the least
  that its recorded hold moments allow, as the spread calibration's lower bound does.
- **Unpriced pairs** keep the rogue-operator study's trigger rule and its year estimates from
  156 sampled hours, and the interval on the year spans zero. Pairs opened ahead need one standing
  pair per spell, and pay financing while they stand, which the figures leave out. Neither
  closing-level convention models the divergence trigger at the repricing, which can only raise the
  level.
- **Session gaps** are the hourly bars' reopening gaps, read from Yahoo's public series. Each gap is
  the first trade of the reopening hour, and the stress level at the reopening is taken as 0.
  Rollovers are counted at 22:00 UTC throughout, as the cap model counts them.

## Re-running

From a virtualenv outside the repository with `requirements.txt` (Python 3.12 or newer), in this
directory. The house-model stages take hours, so run them under `nice`:

```shell
python -I run.py sweep --tier 0                  # and --tier 3: about an hour each
python -I run.py joint --tier 0                  # and --tier 3, once both sweeps are done: about 50 minutes each
python -I run.py joint --tier 0 --flow target    # and --tier 3: the target client's flow, about 80 minutes each on two workers
python -I run.py propose
python -I run.py total --tier 0                  # and --tier 3: about 20 minutes each
python -I run.py stress --tier 0                 # and --tier 3: about 20 minutes each
python -I run.py sensitivity --tier 0            # and --tier 3: about 70 minutes each
python -I run.py income --tier 0 --flow current  # and target, and both at --tier 3: about 17 minutes each
python -I run.py gaps
python -I run.py unprice
python -I run.py report
python -I -m pytest tests
```

Each stage is seeded and deterministic. `results/manifest_*.json` records each house-model stage's
scenario, the hashes of the derived data it read, and its run time. The times above are with three
workers under `nice -n 10`, on a machine shared with other builds, as the committed results ran.
`gaps`, `unprice` and `report` take seconds. Each reproduces its committed results, apart from the
run time that `unpriced_pairs.json` records.

The code is in `caps/`, the tests in `tests/`, and every stage's output in [`results/`](results/),
with each end's runs under `results/tier0/` and `results/tier3/`.
