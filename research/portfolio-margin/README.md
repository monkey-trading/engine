# Portfolio margin

Portfolio margin lowers the margin on positions that offset each other, such as a long position in
one currency pair against a short position in another that moves with it. This study asks which
launch markets may offset each other, by how much, at what tail quantile and horizon, and with what
floor under the reduced requirement.

The study proposes three offset groups: EUR-USD, GBP-USD and AUD-USD; gold and silver; and US500,
US100 and US30. At volume tier 2, a hedged pair's initial requirement falls by 26% on average in FX,
20% in metals and 21% in indices. The study leaves crypto and US shares out, because a hedged pair
there would save only 10%, below the 15% at which a group is worth its parameters. A position that
offsets nothing keeps exactly the margin it has today. Every value here is proposed. The trading
server computes the portfolio requirement and shows it on the account read, but nothing the engine
decides depends on it, and the production configuration lists no group.

- [Results](#results)
- [Method](#method)
- [Data and sources](#data-and-sources)
- [Limits](#limits)
- [Re-running](#re-running)

## Results

Rates are in basis points (bps), where one basis point is one hundredth of one percent, or in
percentages of position value. Each file named below is in `results/`, and `run.py` names the stage
that writes it.

### The rule

The study calibrates the following rule. In each offset group, a position's loading-weighted
exposure is its loading times its quantity times its price. A loading measures how far a market
moves when its group moves. The exposure on the group's smaller side, long or short, is matched in
full, and it offsets as much exposure on the larger side, shared across that side's positions in
proportion to their size. The rest of the larger side is unmatched.

Unmatched exposure keeps its per-position requirement exactly, including its margin bands. Matched
exposure receives a credit. The credit is the matched part's per-position requirement less the
largest loss that a grid of shocks gives it, together with the cost of closing it at its
close-spread ceilings. The credit is at most a set share of that requirement, so the requirement on
the matched part never falls below the group's floor share of it. The portfolio requirement is
therefore never above per-position margin. [Method](#the-requirement) gives the formula.

The matched part's net exposure is zero, so a move of the whole group cancels on it. Only each
member's own move, its residual, can lose money there, and the grid holds one residual shock for
each member. The study calibrates no shock for the group's common move, and proposes none.

### The proposed standard

A shock is the 99.5th percentile of a market's absolute move over 60 seconds, divided by the
close-out level of 50%, so that the close-out level's share of the requirement covers the move. The
horizon is the engine's own fast-move window (`stress_move_window_ms = 60000`). It exceeds the
longest a quote can wait (1 second, `max_age_ms`), a feed's lag (2 seconds, `max_delay` in
`config/dev.toml`) and a held write of the log (5 seconds) together.

Only BTC-USD, ETH-USD and SOL-USD have data that short, in the 50-millisecond price grids of the
[rogue-operator study](../rogue-operator/README.md). Every other market's 60-second quantile is its
hourly quantile multiplied by 0.129 (proposed). That factor is the larger of the square-root-of-time
scale and the ratio the grids show, which is 0.112 to 0.122 at the 99.5th percentile.

### The proposed groups

`results/proposal.toml` holds these values, with loadings in thousandths and shocks in tenths of a
basis point, rounded up. A weekend shock replaces the normal one over a weekend gap.

| Group | Floor share | Members and loadings | Residual shocks | Weekend residual shocks |
| --- | --- | --- | --- | --- |
| FX | 0.7 | EUR-USD 0.90, GBP-USD 0.91, AUD-USD 1.19 | 5.4, 4.5, 5.0 bps | 77.2, 36.6, 53.9 bps |
| Metals | 0.8 | XAU-USD 0.61, XAG-USD 1.39 | 28.0, 12.2 bps | 309.1, 135.7 bps |
| US indices | 0.7 | US500 0.96, US100 1.22, US30 0.82 | 5.6, 9.0, 14.3 bps | 40.2, 105.3, 104.9 bps |

A residual shock is in units of return per unit of loading-weighted exposure: the member's residual
quantile divided by its loading.

The study drops two candidate groups:

- **Crypto** (BTC-USD 0.69, ETH-USD 1.06, SOL-USD 1.25) needs a floor share of 0.9 at the proposed
  allowance, and a hedged pair then saves 10%.
- **US shares** need a floor share of 0.9 at every allowance, for the same saving of 10%. No normal
  or weekend shock covers the shares' overnight and earnings gaps.

USD-JPY, JP225 and WTI belong to no group. `results/groups.json` holds the loadings and shocks of
every candidate.

### Floor shares and savings

The allowance sets each floor share. Under an allowance of 2 times, a group's hedged books may lose
past their balance at most twice as often, and by at most twice as much, as the same books under
per-position margin. A slack of 0.2 percentage points in the rate and 0.1 bps of gross exposure in
the size is added to each bound. The allowance of 2 times is the study's choice. The table gives the
floor share at each allowance, and the average saving on a hedged pair's initial requirement at tier
2 across sizes, with its range where the sizes differ (`groups.json`, `saving_by_allowance.csv`):

| Group | 1 time | 1.5 times | 2 times (proposed) | 3 times |
| --- | --- | --- | --- | --- |
| FX | 1.0, no saving | 0.9, 10% | 0.7, 26% (21% to 30%) | 0.7, 26% |
| Metals | 1.0, no saving | 0.9, 10% | 0.8, 20% | 0.7, 30% |
| US indices | 1.0, no saving | 0.9, 10% | 0.7, 21% (10% to 30%) | 0.7, 21% |
| Crypto | 1.0, no saving | 0.9, 10% | 0.9, 10% | 0.9, 10% |
| US shares | 0.9, 10% | 0.9, 10% | 0.9, 10% | 0.9, 10% |

At the proposed floor shares, the same pair saves less at lower tiers. At tier 1 it saves 7% in FX,
up to 27%, 10% in metals, up to 20%, and 8% in indices, up to 30%. At tier 0 it saves nothing,
because tier 0's margin floor of 5% sets the requirement. Each tier and size is a row of
`saving_by_allowance.csv`.

### Single positions

A position that offsets nothing keeps exactly its current margin. Over every kept group's members,
sizes and tiers, `lone_leverage.csv` gives a maximum leverage under the rule of exactly 1.00 times
that under the margin bands, at every size. The same holds with every candidate group listed and no
floor share (`lone_leverage_all_groups.csv`). The account simulator's single positions are closed
out at the same times, and lose the same amounts past their balance, under both rules
(`lone_unchanged` in `groups.json` shows differences of 0). BTC-USD and ETH-USD stay at 100 times
leverage, SOL-USD at 10 times and EUR-USD at 200 times. A book that is long on one side only, or
whose loadings net to nothing, is unmatched in full.

### How the markets move together

`comove_pairs.csv`, `comove_groups.csv` and `comove_lags.csv` compare every pair of the 34 launch
markets over one hour, over one day and across each kind of market closure. The body correlation
clips each series at its 1st and 99th percentiles. The tail dependence is the chance that one market
is in its 5% tail on the same side, given that the other is, or its 10% tail across closures.

- **Within a group,** the median hourly body correlation is 0.66 in FX, 0.76 in metals, 0.80 in
  crypto and 0.83 in indices. The tail dependence is 0.47 to 0.69, against 0.05 for independent
  markets. Across groups, the body correlation is 0.06 to 0.22.
- **Over weekend gaps,** members agree more than they do hour by hour: 0.95 for indices and 0.85 for
  FX.
- **No member leads another.** Correlations one to three hours apart are within 0.03 of zero, and
  one day apart within 0.08. Among the indices, the correlation one day apart is 0.12 to
  0.16 negative, whichever market comes first, so it is not a lead. Below one hour the study cannot
  say, because the grids' sampled hours differ by market.

The loadings are slopes on the group's equal-weighted average return, which the study uses only to
fit them. The two halves of the window give loadings within 0.15 of each other: gold 0.72 and then
0.58, silver 1.28 and then 1.42, and EUR-USD 0.95 and then 0.82.

### Hourly marks against marks every minute

The account simulator reads one mark an hour, so each hour is a single jump, and a position is
closed out only after it. On the crypto grids, the same hours can be read every 60 seconds, leaving
out hours of stress. The table opens a long or short position at the start of an hour with equity
`e`, closes it when its loss reaches half of `e`, and compares the two readings. It gives the share
of hours that end past the balance, and the mean loss past it, each read every 60 seconds as a
fraction of the hourly reading (`cadence.csv`):

| Market | Margin `e` | `e` in hourly standard deviations | Hours past balance, 60 s over hourly | Loss past balance, 60 s over hourly |
| --- | --- | --- | --- | --- |
| BTC-USD | 0.25%, 0.5%, 1%, 2% | 0.5, 1.1, 2.2, 4.3 | 0.26, 0.15, 0.05, 0.11 | 0.13, 0.06, 0.01, 0.04 |
| ETH-USD | 0.25%, 0.5%, 1%, 2% | 0.4, 0.8, 1.5, 3.05 | 0.43, 0.08, 0.11, 0.04 | 0.08, 0.04, 0.03, 0.003 |
| SOL-USD | 0.25%, 0.5%, 1%, 2% | 0.4, 0.7, 1.4, 2.8 | 0.26, 0.10, 0.02, 0.03 | 0.03, 0.04, 0.02, 0.03 |

**The hourly bars therefore overstate loss past the balance by roughly 2 to 50 times in rate, and
8 to 300 times in size, at margins of 0.25% to 2%.** Each cell at 2%, 3 to 4 standard deviations
from the open, rests on a single stratum-weighted event, so the series is noisy there, and beyond it
no events remain to compare.

The first bands of FX, US indices and metals sit at 4.3 to 5.7, 1.8 to 2.5 and 1.5 to 3.2 hourly
standard deviations (`cadence_z_of_bands.csv`). Indices and metals are inside the range measured for
crypto, and the FX majors are beyond it. The comparison uses single positions. A hedged pair's
residual needs both members on one grid, and the grids share only five sampled hours between BTC-USD
and ETH-USD. The effect on a hedge, and so on the floor shares, is not measured, and the floor
shares above, calibrated on the hourly bars, are the conservative reading. No data below one hour
exists for FX, indices or metals, so for them the crypto ratios are an indication, not a result.

### Coverage of the requirement

`coverage.csv` and `coverage_books.csv` give the share of sampled moves over the horizon that cost a
book more than its equity at the close-out level. The requirement includes the cap and the floors,
and the loss includes the cost of closing at each market's ceiling and the typical spreads of the
trades. The moves are sampled jointly, from the hours in which every member was open, in both
directions, with each hour's returns scaled by 0.129. The members' joint tails therefore stay as the
data has them.

- **At the 99.5th percentile over 60 seconds, every book meets the standard,** under per-position
  margin too. Today's first band at the close-out level is 1.2 to 2.6 times the 60-second move for
  ETH-USD, BTC-USD, the US indices and the metals, 4.8 times for EUR-USD, 10.8 times for SOL-USD and
  8.5 to 23.6 times for the shares (`implied_horizon.csv`).
- **Over one hour,** single positions fare identically under both rules, and 0.8% to 4.4% of moves
  cost more than their equity. Hedged pairs fail on 0.10% of moves under the rule against 0.05%
  under the bands in FX, 0.76% against 0.49% in metals and 0.67% against 0.53% in indices.
- **Over a weekend gap,** the share of pair books that lose no more than 0.5% is 54% under the rule
  against 62% under the bands in FX, 25% against 50% in metals and 38% against 46% in indices. The
  simulated rule is looser there, because it charges the matched part no more than per-position
  margin, which over a weekend gap is zero for a market with no weekend rate. The grid's weekend
  shocks bind only for the markets that have one.

### Hedged pairs against single positions

At the same tier and on the same history, a hedged pair under the rule loses past its balance far
less often, and by less, than a single position under its bands (`hedged_vs_single.csv`). At tier 2:

| Group | Hedged pair, rule | Single position, bands |
| --- | --- | --- |
| FX | 0.20% of moves, 0.02 bps of gross | 2.1% of moves, 0.34 bps of gross |
| Metals | 2.9% of moves, 0.65 bps of gross | 5.7% of moves, 2.5 bps of gross |
| US indices | 1.9% of moves, 0.22 bps of gross | 8.9% of moves, 2.3 bps of gross |

Against the same hedged pair under the bands, the rule is no worse by more than the allowance.

### Crisis days

`crisis_days.csv` lists each group's 12 worst days since 2016, by the group's common move and by a
hedged pair's residual. They include the day after the United Kingdom's referendum on leaving the
European Union in June 2016, March 2020, and the fall in metals in January 2026. A day is far beyond
a 60-second standard. On those days a hedged pair loses 4.6 to 15.4 times its equity at the
close-out level in FX, 5.7 to 8.9 times in metals and 4.6 to 7.8 times in indices. Measured against
per-position margin, the losses are 1.8 to 5.9, 2.3 to 3.5 and 2.1 to 3.5 times, so per-position
margin is no better on such days.

### Closing costs at a lower close-out level

At a close-out level of 25%, half the launch level, the first band of per-position margin falls
short of the market's close-spread ceiling for 10 markets: BTC-USD, ETH-USD, the four FX majors and
four indices (`ceiling_shortfall.csv`). BTC-USD's band then gives 25 bps against a 30-bp ceiling. At
50% and 75%, no market falls short. The rule never raises a requirement above per-position margin,
so it leaves these markets exactly as they are today.

### Other requirements compared

On hedged pairs, `alternatives.csv` compares the rule with two other requirements:

- A credit of 50% of the spread on per-position margin releases 12% to 25% of the requirement, and
  the rule releases 11% to 30%.
- A full historical scenario requirement, the empirical 99.5th percentile 60-second loss divided by
  the close-out level, releases 82% to 96%. It then loses past the balance on 18% to 25% of
  hour-long moves, which is too thin to rely on. Its rate over 60 seconds is 0.5% by construction.
- Over hour-long moves, the spread credit fails on up to 1.1% of them, the rule on up to 1.2% and
  the bands on up to 0.7%.

### Outages, halts and blocked corporate actions

`outage.csv` takes a matched pair of opposite accounts at tier 2 and shuts one member for 4 to
168 hours, over 600 starting times, with no protection for the hedge while the member is shut. The
house's result per unit of gross exposure, where a positive number is its gain, at the start an
operator would choose is:

| Group | Rule, 24 and 168 hours | Bands, 24 and 168 hours |
| --- | --- | --- |
| FX | -72 to -90 bps | -72 to -84 bps |
| Metals | -398 to -446 bps | -388 to -441 bps |
| US indices | -153 to -171 bps | -150 to -169 bps |

Clients' losses stop at their balances. Mean results differ by up to 7 bps. The share of starts that
end with a loss past the balance is larger under the rule. In FX it is 3.8% and 27% at 24 and
168 hours, against 1.2% and 15%. In metals it is 34% and 57%, against 25% and 50%, and in indices
25% and 56%, against 20% and 50%.

### Backtest

`backtest.csv` mixes the hedged pair and the single position at each tier and size, with hedged
shares of the flow of 0%, 10%, 25%, 50% and 100%, under both rules. At tier 2 with no hedged flow,
the rules are identical. The house's result per unit of gross exposure is +2.9 bps in FX, -1.6 bps
in metals and +5.9 bps in indices, 52%, 60% and 66% of accounts are closed out, and leverage is
unchanged.

With half of the flow hedged, the rule changes the house's result from +3.2 to +3.0 bps in FX, from
+6.3 to +5.5 bps in metals and from +7.1 to +6.9 bps in indices. Close-outs rise from 39% to 43%,
from 50% to 53% and from 49% to 53%, and leverage rises by 16%, 13% and 11%. With all of the flow
hedged, leverage rises by 33%, 25% and 21%.

### Bounds for later changes

`bounds.csv` fits the same groups at the 99.9th percentile, and over 5 minutes and one hour. A
member's residual shock can rise to 3.6 times its proposed value in FX, 3.2 times in metals and
1.7 times in indices before a hedged first-band pair needs per-position margin. The study proposes
stricter bounds within which these parameters may later be moved: each residual shock up to its
99.9th-percentile, 5-minute value, and each floor share up to 1.0. Those shocks are 26.9, 20.9 and
27.3 bps in FX, 167.0 and 72.5 bps in metals, and 49.7, 42.6 and 66.6 bps in indices. The 99.9% rows
divide by an hourly reference of 52 windows, the rogue-operator grids' sampled hours, so their
scales of 0.205 at 60 seconds and 0.431 at 5 minutes are noisy. At 99.5% the scale is the square
root of time.

### Sensitivity

`sensitivity.csv` repeats the groups stage at six standards:

| Quantile | Horizon | Scale on the hour | Groups kept | Floor shares: FX, metals, indices, crypto |
| --- | --- | --- | --- | --- |
| 99% | 60 s | 0.147 | US indices, metals, FX | 0.7, 0.8, 0.7, 0.9 |
| 99.5% | 60 s | 0.129 | US indices, metals, FX | 0.7, 0.8, 0.7, 0.9 |
| 99.9% | 60 s | 0.205 | Metals, FX | 0.7, 0.7, 0.7, 0.9 |
| 99.5% | 10 s | 0.053 | US indices, metals, FX | 0.8, 0.8, 0.8, 0.9 |
| 99.5% | 300 s | 0.289 | US indices, metals, FX | 0.7, 0.7, 0.7, 0.9 |
| 99.5% | 3600 s | 1.000 | Metals, FX | 0, 0.7, 0.5, 0.9 |

- A looser quantile of 99% keeps the same three groups and floor shares, with savings of 20% to 27%.
- A shorter horizon of 10 seconds raises every floor share to 0.8.
- A tighter quantile of 99.9% lowers the metals' floor share to 0.7, and drops the indices, whose
  saving falls to 15%.
- A longer horizon of 5 minutes keeps the groups at 0.7 to 0.8, with the indices' saving at 15%.
- Over an hour the indices drop out, with a saving of 12%. FX's own floor share is 0, because the
  grid itself binds.
- Crypto and the shares stay out at every standard.

## Method

### The requirement

`pm/model.py` computes the requirement. For each group, a member's loading-weighted exposure is
`x = beta * quantity * price`, signed. `L` is the sum over the long side and `S` the absolute sum
over the short side. Matched exposure is gross exposure less the absolute net,
`sum|x| - |sum x| = 2 min(L, S)`. The smaller side is matched in full. The larger side is matched in
the ratio `min(L, S) / max(L, S)`, pro rata across its members, and the rest of it is unmatched,
with its own bands. Then:

```
R = PP - sum_g credit_g
credit_g = max(0, min((1 - phi_g) PP_matched, PP_matched - grid_matched))
grid_matched = sum_i rho_i |x_i| (over the matched part) + sum_i |matched notional_i| c_i / level
```

`PP` is the per-position requirement, and `PP_matched` is the matched part's share of it: each
member's `PP_i` multiplied by its matched fraction. `phi_g` is the group's floor share, `c_i` the
member's close-spread ceiling and `level` the close-out level. The requirement equals the unmatched
part at its bands plus `min(PP_matched, max(phi_g PP_matched, grid_matched))`. Over a weekend gap,
the weekend rates and residual shocks replace the normal ones. In the simulator, a market with no
weekend rate has no requirement on its unmatched part over the gap, as under per-position margin.
The bands are staircases, as in `market/mod.rs` (`staircase`), each at least the tier's floor, and
`R` is at least the tier's floor on gross exposure.

### Fitting the groups

`pm/calibrate.py` fits each group. The group's common move, used only to fit, is the members' mean
return. A member's loading is its slope on that move, and its residual is what remains, divided by
the loading to give the shock. The hourly quantiles use the hours in which every member was open. An
hour's move runs from open to close, plus the gap from the previous close when the previous hour was
open. Gaps across closures are read separately.

The weekend values use the closures of 40 hours or more in the 102 weeks, about 100 weekends, so the
99.5th percentile is close to the largest. Where the long daily history since 2016 is wider, the
study raises them by the ratio of that history's quantile of Friday-to-Monday close returns to the
window's quantile: 2.1 for indices and 1.25 for FX.

### The account simulator

`pm/sim.py` opens accounts at the tier's initial requirement under the rule, and marks them at each
hour's close. An account below the close-out level times its requirement is closed in full. Under
the bands that requirement is per-position margin, and under the rule it is `R`, both at the
market's bands without the tier floors, and a group closes together. At the start of a weekend gap,
an account below its full requirement at the weekend values is closed. For a single position, only
the unmatched part of a market with a weekend rate counts. Each leg pays the bands' spreads, and the
10% profit fee on its realised gain. An account that survives the week is closed at its end on the
same terms. Any loss past the balance is the house's.

A weekend gap starts at the last open hour before a closure of 40 hours or more. Under today's
bands, the weekend close-out runs only for a market with a weekend rate. The floor shares rest on
hedged books: a pair hedged in full and in half, long and short, two pairs per group, at four sizes
(a quarter of the first band, the first band, and 4 and 20 times it) and three tiers. Each book is
held for a week from random hours, and each leg alone confirms that it is unchanged (`groups.json`,
`groups_rows.csv`). The seed is 914 in every stage, and the stages are deterministic.

## Data and sources

The study reads these files in place, without copying them:

- the [cap calibration](../cap-calibration/README.md)'s hourly bars,
  `research/cap-calibration/data/derived/hourly_bars.npz`, 102 weeks to 12 October 2026, with its
  fetch and daily-close code;
- the [launch-caps study](../launch-caps/README.md)'s `caps/gaps.py`, which finds the closures and
  their kinds;
- the [rogue-operator study](../rogue-operator/README.md)'s 50-millisecond grids,
  `research/rogue-operator/data/derived/grids.npz`, and their strata, `sample.json`;
- `config/production-genesis.toml`, for the margin bands, spreads, weekend margins and close-spread
  ceilings.

The study commits `data/derived/daily_logclose.csv`, the daily log closing prices since 2016 for the
34 launch markets, left blank where a market has no close, and `data/derived/daily_sources.csv`. The
raw pulls in `data/raw/` are gitignored. The public sources are Binance, Yahoo Finance and FRED. The
study uses FRED where it has a series, and otherwise the public chart series. US500 uses the S&P
500 cash close from the chart series, because FRED's series holds only one year. Futures-based
closes of gold and silver carry roll jumps, which the cap calibration's hourly bars remove, and a
roll shows among the metals' crisis days.

## Limits

- **Below one hour, the horizon is an approximation for every market but BTC-USD, ETH-USD and
  SOL-USD.** For the others, the hour's quantile is scaled by the square root of time or by the
  crypto grids' ratio. The 60-second behaviour of index, FX and metal markets is not measured.
- **The standard is far inside today's bands.** The 60-second standard does not discipline the floor
  share. The floor shares rest instead on the simulator's hourly check and on the allowance of
  2 times, which is the study's choice. The hourly check reads each hour as one jump, which
  overstates loss past the balance several times over for crypto's single positions. A horizon of
  minutes or an hour makes the grid reach per-position margin for most hedged books, so groups drop
  out instead (`sensitivity.csv`).
- **The study fits and judges on the same 102 weeks.** Loadings, shocks, floor shares and the
  weekend ratio use the whole window, the weekend ratio the long history as well, and the simulator
  runs on it. Only the loadings' halves are compared, and no out-of-sample test of the floor shares
  was run.
- **The simulator reads the hour's close,** and charges no spread on opening, since an account opens
  at exactly its requirement. A move inside an hour that the close does not show goes unseen under
  both rules, because the bars' highs and lows are per market, not per book. The weekend rows rest
  on about 100 events, and the metals' weekend residual on a single observation.
- **The engine is stricter than the simulator in two cases.** At initial bands, the engine gives no
  credit to the matched part of a market with no weekend rate, from two hours before the group's
  weekend gap until its members reopen. The engine also gives a group no credit while a member the
  account holds cannot trade. The simulator models neither, so its weekend and outage comparisons
  show the rule looser than the engine applies it.
- **Not built or not run:** - no test compares the engine's requirement with the simulator's on
  shared vectors; - the cap calibration's joint stage has not been re-run with hedged flow, to see
  whether the
    exposure-cap floors change, since that takes hours on the cap model's flow, and the backtest
    here does not touch the house's caps;
  - every account holds one group's pair, and the close-out closes the group whole, so the
    close-out's choice among candidate steps in an account holding several groups is not tested;
  - every position pays the 10% base profit fee after a hold of hours, so the fee at shorter holds
    and the fee curve are not tested;
  - a market's prolonged halt is not run as a case apart from an outage, which freezes the same
    mark.
- **Shares.** No shock covers the group's overnight and earnings gaps, which is why the group is
  dropped. The study does not measure the shares' normal moves for that decision.
- **Books.** The floor shares' books are pairs, and each leg alone, of each group's first two or
  three members at four sizes. Baskets appear only in `coverage.csv`, and partial hedges beyond half
  are not run.

## Re-running

From a virtualenv with `requirements.txt` installed, using Python 3.12 or newer, run these commands
in this directory, under `nice`:

```shell
python -I run.py fetch        # re-pulls the daily history (public sources; FRED by curl with `cosd`)
python -I run.py derive       # data/derived/daily_logclose.csv
python -I run.py all          # horizon, comove, groups, coverage, checks, outage, proposal, cadence (about 5 minutes)
python -I run.py sensitivity  # the groups stage at six standards (about 25 minutes)
python -I -m pytest tests
```

`pm/model.py` holds the requirement, `pm/groups.py` the floor shares and groups, `pm/sim.py` the
account simulator and `pm/extra.py` the crisis days, alternatives, bounds and outages. Each stage
records its inputs in `results/manifest_<stage>.json`.
