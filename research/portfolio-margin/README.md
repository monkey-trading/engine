# Portfolio margin: offset groups, shocks, quantile and floor (#914)

The calibration study behind #913's decisions: which markets may offset each other, by how much, at
what tail quantile and horizon, with what floor. Its values reach `config/production-genesis.toml`
through #639 only once the owner confirms them on #913. Outside the build and CI.

## Result

This is the study under #913's revised D5 (2026-11-17): unmatched exposure keeps its own per-position
requirement exactly, only matched exposure within a group gets the grid's credit, that credit is floored at a
group's own share of the matched part's per-position requirement, and the requirement is never above
per-position margin. The first version of this study (PR #920) applied one share to the whole requirement.

**Matched exposure, exactly (the definition #915 builds the same way).** Per group, each member's
loading-weighted exposure is `x = beta * quantity * price` (signed). `L` is the long side's sum and `S` the
short side's absolute sum. Matched exposure is gross less the absolute net, `sum|x| - |sum x| = 2 min(L, S)`. The smaller
side is wholly matched; the larger side is matched in the ratio `min(L, S) / max(L, S)`, pro rata across its members, and
the rest of it is unmatched, with its own bands. Then

    R = PP - sum_g credit_g
    credit_g = max(0, min((1 - phi_g) PP_matched, PP_matched - grid_matched))
    grid_matched = sum_i rho_i |x_i| (over the matched part) + sum_i |matched notional_i| c_i / level

with `PP_matched` the matched part's share of the per-position requirement (each member's `PP_i` times its matched
fraction), `c_i` the close-spread ceiling and `level` the close-out level. This equals the unmatched part at bands plus
`min(PP_matched, max(phi_g PP_matched, grid_matched))`. **There is no factor shock:** the matched part's net is zero, so it
contributes nothing, and none is calibrated or proposed. A residual shock `rho_i` is in return units per unit of
loading-weighted exposure (the member's residual quantile divided by its loading). Over a weekend gap the weekend rates and
residuals replace the normal ones, and a market with no weekend rate has no requirement on its unmatched part, as today.

**Proposed standard (D2):** the 99.5th percentile of the absolute move over **60 seconds**, divided by the close-out level
(50%), so the level's share of the requirement covers the move. The horizon is the engine's own fast-move window
(`stress_move_window_ms = 60000`): it exceeds the longest a quote can wait (1 s `max_age_ms`), a feed's lag (`max_delay`, 2 s in
`config/dev.toml`) and a held batch sync (5 s, Critical `log_sync_held`) together. Only BTC-USD, ETH-USD and SOL-USD have
data that short (the rogue-operator study's 50 ms grids); every other market's 60 s quantile is its hourly quantile times
**0.129**, the larger of square-root time and the grids' ratio (0.112 to 0.122 at 99.5%). That approximation is for the owner
to approve.

**Proposed groups, shocks and floor shares, `results/proposal.toml`:**

| group | floor share | members, loadings | residual shocks (bp) | weekend residual shocks (bp) |
|---|---|---|---|---|
| fx | 0.7 | EUR-USD 0.90, GBP-USD 0.91, AUD-USD 1.19 | 5.4, 4.5, 5.0 | 77.2, 36.6, 53.9 |
| metals | 0.7 | XAU-USD 0.61, XAG-USD 1.39 | 28.0, 12.2 | 309.1, 135.7 |
| us_index | 0.6 | US500 0.96, US100 1.22, US30 0.82 | 5.6, 9.0, 14.3 | 40.2, 105.3, 104.9 |

**Dropped:** crypto (BTC-USD 0.69, ETH-USD 1.06, SOL-USD 1.25) and the US shares each need a floor share of 0.9 at the 2 times
allowance, and a hedged pair then saves 10%, under the 15% a group must save to be worth its parameters. The shares' overnight and
earnings gaps are covered by no normal or weekend shock. USD-JPY, JP225 and WTI stay alone (D3). Their loadings and shocks are in `groups.json`.

**Floor share at each allowance** (a group's hedged books may lose past balance at most this many times as often and by as
much as the same books under per-position margin, plus a slack of 0.2 points and 0.1 bp of gross; `groups.json`,
`saving_by_allowance.csv`), with the saving of a hedged pair's initial requirement at tier 2, mean over sizes (range):

| group | 1x | 1.5x | 2x (proposed) | 3x |
|---|---|---|---|---|
| fx | 0.9, saves 10% | 0.7, 26% (21 to 30) | 0.7, 26% | 0.7, 26% |
| metals | 1.0, none | 0.8, 20% | 0.7, 30% | 0.7, 30% |
| us_index | 1.0, none | 0.7, 21% (10 to 30) | 0.6, 26% (10 to 40) | 0.5, 31% (10 to 50) |
| crypto | 1.0, none | 0.9, 10% | 0.9, 10% | 0.9, 10% |
| shares | 0.9, 10% | 0.9, 10% | 0.9, 10% | 0.9, 10% |

At tier 1 the same pair saves 7% (fx, up to 27%), 15% (metals, up to 30%) and 10% (indexes, up to 40%) at the proposed shares, and at
tier 0 nothing, whose 5% margin floor is the requirement. Each tier and size is a row of `saving_by_allowance.csv`. The 2 times
allowance is this study's choice; the owner can pick another.

**Lone positions are unchanged, and the check is exact.** Over every kept group's members, sizes and tiers, `lone_leverage.csv`
gives a maximum leverage under the rule of exactly 1.00 times the bands' at every size (also with every candidate group listed and
no floor, `lone_leverage_all_groups.csv`), and the account simulator's lone legs give the same close-outs and loss past balance
under both rules (`groups.json`, `lone_unchanged`: differences of 0). BTC-USD and ETH-USD stay at 100x, SOL-USD at 10x, EUR-USD
at 200x. A book that is long on one side only (or nets on no loading) is unmatched in full.

## What the study measured

Each file is in `results/`; the stage that writes it is in `run.py`.

- **Co-movement** (`comove_pairs.csv`, `comove_groups.csv`, `comove_lags.csv`): every pair of the 34
  launch markets at one hour (cap study's hourly bars, 102 weeks), one day (2016 on, `data/derived/daily_logclose.csv`)
  and across each kind of closure, as a body correlation (each series clipped at its 1st and 99th percentiles) and the tail
  dependence (the chance one is in its 5% tail, 10% for closures, on the same side given the other is). Within a group the hourly
  body correlations are 0.66 (fx), 0.76 (metals), 0.80 (crypto) and 0.83 (indexes) at the median, tail dependence 0.47 to
  0.69 against 0.05 for independence; across groups 0.06 to 0.22. Weekend gaps agree more than hours
  (indexes 0.95, fx 0.85). **No member leads another:** correlations one to three hours apart are within 0.03 of zero,
  one day apart within 0.08, and among the indexes 0.12 to 0.16 negative either way round, so not a lead.
  Below one hour the study cannot say: the grids' sampled hours differ by market.
- **Loadings and stability** (`groups.json`): loadings are slopes on the group's equal-weighted mean (the factor, used to fit and
  not proposed); halves of the window agree within 0.15 (gold 0.72 then 0.58, silver 1.28 then 1.42, EUR-USD 0.95 then 0.82).
- **The hourly bars as jumps** (`cadence.csv`, `cadence_z_of_bands.csv`). The simulator reads one mark an hour, so each hour is one
  jump and a position closes only after it. On crypto's 60 s grids the same hours can be read every 60 s. For a long or short
  opened at an hour's start with equity `e`, closed when its loss reaches half of it, the share of hours that end past the balance
  and the mean excess loss past it, as a fraction of the hourly reading:

  | market | margin `e` | `e` over the hour's s.d. | past balance, 60 s over hourly | excess, 60 s over hourly |
  |---|---|---|---|---|
  | BTC-USD | 0.25%, 0.5%, 1%, 2% | 0.5, 1.1, 2.2, 4.3 | 0.26, 0.15, 0.05, 0.11 | 0.13, 0.06, 0.01, 0.04 |
  | ETH-USD | 0.25%, 0.5%, 1%, 2% | 0.4, 0.8, 1.5, 3.0 | 0.43, 0.08, 0.11, 0.04 | 0.08, 0.04, 0.03, 0.003 |
  | SOL-USD | 0.25%, 0.5%, 1%, 2% | 0.4, 0.7, 1.4, 2.8 | 0.26, 0.10, 0.02, 0.03 | 0.03, 0.04, 0.02, 0.03 |

  **So the hourly bars overstate loss past balance by roughly 4 to 25 times in rate and 8 to 300 times in size at the margins the launch
  bands use for crypto, and beyond 4 standard deviations there are no events left to compare.** The first bands of FX, indexes and
  metals sit at 4.3 to 5.7 (FX), 1.8 to 2.5 (indexes) and 1.5 to 3.2 (metals) hourly standard deviations: inside the range measured
  for crypto except the FX majors, whose bands are beyond it. This is read from lone legs. A hedged pair's residual needs both
  legs on one grid, and the grids share only five sampled hours between BTC-USD and ETH-USD, so the effect on a hedge, and
  on the floor shares, is not measured; the shares above are calibrated on the hourly bars and are the conservative reading. Nothing below one hour exists for
  FX, indexes or metals, so for them the crypto ratios are an indication, not a result.
- **The floor shares and the groups** (`groups.json`, `groups_rows.csv`): hedged books (a pair hedged in full and in half, long and
  short, two pairs per group) at four sizes (a quarter of the first band, the first band, 4 and 20 times it) and three tiers, held a week
  from random hours on the account simulator (below), and each leg alone to confirm it is unchanged.
- **Coverage of the final requirement** (`coverage.csv`, `coverage_books.csv`): the share of sampled moves over the horizon
  that cost a book more than its equity at the close-out trigger, with the cap, the floors, closing costs at ceilings in the grid and
  the fills' typical spreads in the loss. Joint sampling: the hours every member was open, both signs, each hour's return vector scaled by 0.129,
  so the members' joint tails stay as the data has them. **At 99.5% over 60 s every book meets the standard, per-position margin too**: today's
  first band at the close-out level is 1.2 to 2.6 times the 60-second move for ETH-USD, BTC-USD, the US indexes and the metals, 4.8 times for EUR-USD,
  10.8 times for SOL-USD and 8.5 to 23.6 times for the shares (`implied_horizon.csv`). Over one hour, lone positions fare identically under both rules (0.8% to
  4.4% fail), and hedged pairs fail 0.10% against 0.05% (fx), 0.95% against 0.49% (metals) and 0.71% against 0.53% (indexes). Over a weekend gap, the shares of
  pair books within 0.5% are 92% under the rule against 62% under the bands (fx), 79% against 46% (indexes) and 38% against 50% (metals); the rule runs
  a gap-start check on every matched book where today's runs it only on a market with a weekend rate (V60), so part of the gain is the check.
- **A hedged book against a single position** (`hedged_vs_single.csv`): the hedged pair under the rule loses past
  its balance far less often and by less than one position under its bands at the same tier, on the same history: at tier 2, fx 0.19% against 2.1%
  (size 0.01 against 0.34 bp of gross), metals 3.2% against 5.7% (0.68 against 2.5 bp), indexes 1.3% against 8.9% (0.14 against 2.3 bp). Against the same hedged
  pair under the bands the rule is no worse by more than the allowance above.
- **The long history's crisis days** (`crisis_days.csv`): each group's 12 worst days by its factor and by a hedged pair's residual since 2016 (Brexit and March
  2020; January 2026's metals fall). A day is far past a 60-second standard: a hedged pair's day loses 4.6 to 15.4 times its equity at the trigger (fx), 6.6 to 10.1 (metals)
  and 4.6 to 7.8 (indexes), and 1.8 to 5.9, 2.3 to 3.5 and 2.1 to 3.5 times per-position margin, so per-position margin is no better there.
- **Closing-cost shortfalls** (`ceiling_shortfall.csv`): at a close-out level of 25% (half launch's), per-position margin's first band falls short of its
  ceiling for 10 markets (BTC-USD, ETH-USD, the four FX majors and the four US and German indexes: BTC-USD's band is 25 bp against a 30 bp ceiling); at 50% and 75% none
  does. The cap leaves these exactly as today's.
- **The alternatives** (`alternatives.csv`): on hedged pairs, a 50% spread credit on per-position margin releases 12% to 25%, the rule 11% to 40%, and a full
  historical scenario requirement (the 60-second empirical 99.5% loss over the level) 82% to 96% and loses past balance 18% to 25% of moves over an hour: too thin
  to rely on there (its rate over 60 s is 0.5% by construction). Over hour-long moves the spread credit fails in up to 1.1% of them, the rule in up to 1.7%, the bands in up to 0.7%.
- **Outages, halts and blocked corporate actions of one member** (`outage.csv`): a matched pair of opposite accounts, one member shut 4 to 168 hours, no protection of the
  hedge (D10), over 600 starts, at tier 2. The house's result per gross (positive is its gain; the clients' losses stop at their balances) at the start an operator would
  choose is -72 to -90 bp (fx), -403 to -446 (metals) and -153 to -171 (indexes) at 24 and 168 hours under the rule, against -72 to -84, -388 to -441 and -150 to -169 under the bands. Mean results
  differ by up to 10 bp. The share of starts with a loss past balance is larger under the rule: fx 3.8% and 27% against 1.2% and 15% at 24 and 168 hours, metals 39% and 61% against 25% and 50%,
  indexes 25% and 56% against 20% and 50%. The initial requirement's treatment of a group with a shut member (D10: no offset) is not simulated.
- **Backtest** (`backtest.csv`): the hedged pair and the lone leg at each tier and size, mixed at hedged shares of flow 0, 10, 25, 50 and 100%, under both rules. At tier 2 with no hedged flow the
  rules are identical (house result per gross +2.9 bp in fx, -1.6 in metals, +5.9 in indexes; close-outs 52%, 60%, 66%; leverage unchanged). With half the flow hedged the house result goes from
  +3.2 to +2.8 bp (fx), +6.3 to +4.0 (metals) and +7.1 to +6.2 (indexes), close-outs from 39% to 62%, 50% to 64% and 49% to 70%, and leverage up 16%, 22% and 14%. With all of it hedged, leverage is up 33%, 43% and 28%.
- **Stricter bounds (D11)** (`bounds.csv`): the same groups at 99.9% and over 5 minutes and one hour; the residual shocks can rise to 3.6 times (fx), 3.2 times (metals) and 1.7 times (indexes) their proposed value before
  a hedged first-band pair needs per-position margin. Proposed: residual shocks up to the 99.9%, 5-minute values (fx 26.9, 20.9, 27.3 bp; metals 167.0, 72.5; indexes 49.7, 42.6, 66.6), and each floor share up to 1.0. The
  99.9% rows rest on an hour reference of 52 windows (the rogue grids' sampled hours), so their scales (0.205 at 60 s, 0.431 at 5 minutes) are noisy.

## Sensitivity (`sensitivity.csv`)

| quantile | horizon | scale on the hour | groups kept | floor shares (fx / metals / indexes / crypto) |
|---|---|---|---|---|
| 99% | 60 s | 0.147 | us_index, metals, fx | 0.7 / 0.7 / 0.6 / 0.9 |
| 99.5% | 60 s | 0.129 | us_index, metals, fx | 0.7 / 0.7 / 0.6 / 0.9 |
| 99.9% | 60 s | 0.205 | us_index, metals, fx | 0.7 / 0.6 / 0.5 / 0.9 |
| 99.5% | 10 s | 0.053 | us_index, metals, fx | 0.7 / 0.7 / 0.6 / 0.9 |
| 99.5% | 300 s | 0.289 | us_index, metals, fx | 0.7 / 0.7 / 0.5 / 0.9 |
| 99.5% | 3600 s | 1.000 | metals, fx | 0 / 0.7 / 0.5 / 0.9 |

A looser quantile (99%) or a shorter horizon (10 s) keeps the same three groups and the same shares (savings 27% to 31% at tier 2). A tighter
quantile (99.9%) or a longer horizon (5 minutes) keeps the groups and lowers the metals' and indexes' shares by 0.1 as the grid grows toward the bands, with smaller
savings (19% to 23%; crypto's falls to 7%). Over an hour the indexes drop out (12% saving) and fx's own share is 0, because the grid itself binds. Crypto and
the shares stay out at every standard.


## Method

- **Model.** `pm/model.py` is the requirement above, with bands as staircases (`market/mod.rs`, `staircase`) each at least the tier floor, and `R` at least the tier floor on gross.
- **Fit.** `pm/calibrate.py`. Factor (to fit only): the members' mean return; loading: the slope on it; residual: what is left, divided by the loading for the shock. Hourly quantiles over hours every
  member was open (hour moves open to close, plus the gap when the previous hour was open; closures' gaps read apart).
- **Weekend values.** The closures of 40 hours or more in the 102 weeks (about 100 weekends: the 99.5% quantile is close to the largest), raised by the ratio of
  the 2016-on Friday-to-Monday close returns' quantile to the window's where the long history is wider (indexes 2.1, fx 1.25).
- **Simulator** (`pm/sim.py`): accounts open at the tier's initial requirement under the rule, marked at each hour's close,
  closed whole below the close-out level times the requirement (per-position margin under the bands, R under the rule, both at the market's bands, without tier floors; a group closes together), at a weekend gap's start
  below the requirement in full at the weekend values (a lone position's unmatched part only where its market has a weekend rate), paying the bands' spreads and the 10% profit fee on each leg's realised gain; the loss past
  the balance is the house's. A weekend gap's start is the last open hour before a closure of 40 hours or more; today's bands run it only for
  a market with a weekend rate (V60).
- **Seeds.** 914 throughout; the stages are deterministic.

## Limits, and what is not established

- **The horizon below one hour is an approximation for every market but BTC-USD, ETH-USD and SOL-USD**: the hour's quantile
  times square-root time or the crypto grids' ratio. Index, FX and metal markets' microstructure at 60 s is not measured.
- **The standard is far inside today's bands.** The 60-second standard does not discipline the floor share; the floor shares rest on the simulator's
  hourly check, which reads each hour as one jump (see the cadence table above: for crypto's lone legs this overstates loss past balance several times over), and on the allowance of 2 times, which is this study's choice.
  A horizon of minutes or an hour makes the grid reach per-position margin for most hedged books, so groups drop out instead (`sensitivity.csv`).
- **Fitted and judged on the same 102 weeks.** Loadings, shocks, the floor share and the weekend ratio use the whole window (the weekend ratio the long history
  as well), and the simulator runs on it. Only the loadings' halves are compared; no out-of-sample test of the floor shares was run.
- **The simulator reads the hour's close,** and charges no entry spread (an account opens at the requirement exactly). A move inside an hour that the close does not show goes unseen by both rules (the bars' highs and
  lows are per market, not per book). The weekend rows have about 100 events; the metals residuals' weekend value is one observation's.
- **Not built or not run:** the engine's requirements agreeing with the simulator's on shared vectors (needs #915 merged: the engine has no portfolio requirement
  to read yet); the cap study's joint stage re-run with hedged flow to see whether the exposure-cap floors change (hours of compute on the
  cap model's flow; the study's backtest does not touch the house's caps); the D6 candidate step with several groups in one account (every account here
  holds one group's pair, where the step closes the group whole, so the step's choice among candidates is not tested); D6's profit fee at
  different holds and the fee curve (every close pays the 10% base after a hold of hours); shut legs at the initial requirement; a market's
  prolonged halt as a different case from an outage (the same frozen mark).
- **Daily data.** Sources are public: FRED where it has a series, else the public chart series; US500 uses the S&P 500 cash close from the chart
  series because FRED's holds one year. Futures-based gold and silver closes carry roll jumps the cap study's hourly bars remove; a
  roll shows in the crisis days of the metals.
- **Shares.** The group's overnight and earnings gaps are not covered by any shock; that is why it is dropped, not a measurement of its normal moves.
- **The 99.9% rows** (`sensitivity.csv`, `bounds.csv`) divide by an hour reference of 52 windows; at 99.5% the scale is square-root time.
- **Books.** The floor shares' books are pairs (and each leg alone) of the first two or three members at four sizes; baskets are in `coverage.csv` only. Partial hedges beyond half are not run.
- **The hedge in the hourly cadence.** The cadence comparison is on lone legs; no joint grid exists for a hedged residual (five shared hours).

## Data

Reused in place (not copied): `research/cap-calibration/data/derived/hourly_bars.npz` (hourly bars, 102 weeks to 2026-10-12), its fetch
and daily-close code, `research/launch-caps`' `caps/gaps.py` (the closures and their kinds), `research/rogue-operator/data/derived/grids.npz`
and `sample.json` (50 ms grids and their strata), and `config/production-genesis.toml` (bands, spreads, weekend margins, close-spread ceilings).
Committed here: `data/derived/daily_logclose.csv` (the daily log closes from 2016 for the 34 launch markets, blank where a market has no
close) and `daily_sources.csv`. Raw pulls (`data/raw/`) are gitignored; Binance, Yahoo Finance and FRED are the public sources.

## Re-running

From a virtualenv with `requirements.txt` (Python 3.12 or newer), in this directory, under `nice`:

    python -I run.py fetch        # re-pulls the daily history (public sources; FRED by curl with `cosd`)
    python -I run.py derive       # data/derived/daily_logclose.csv
    python -I run.py all          # horizon, comove, groups, coverage, checks, outage, proposal, cadence (about 5 minutes)
    python -I run.py sensitivity  # the groups stage at six standards (about 25 minutes)
    python -I -m pytest tests

Files: `pm/common.py` (paths, candidates), `pm/model.py` (the requirement), `pm/calibrate.py`, `pm/groups.py` (the floor share and
the groups), `pm/sim.py`, `pm/coverage.py`, `pm/extra.py` (crisis days, alternatives, bounds, outages), `pm/comove.py`, `pm/horizon.py`,
`pm/daily.py`, `pm/report.py`, `pm/io.py`, `pm/context.py`, `pm/stats.py`, `pm/checks.py`, `pm/hourly.py`, `results/manifest_<stage>.json`.
