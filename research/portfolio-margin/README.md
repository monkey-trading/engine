# Portfolio margin: offset groups, shocks, quantile and floor (#914)

The calibration study behind #913's decisions: which markets may offset each other, by how much, at
what tail quantile and horizon, with what floor. Its values reach `config/production-genesis.toml`
through #639 only once the owner confirms them on #913. Outside the build and CI.

## Result

**Proposed standard (D2):** the 99.5th percentile of the absolute move over **60 seconds**, divided by
the close-out level (50%), so the level's share of the requirement covers the move. The horizon is the
engine's own fast-move window (`stress_move_window_ms = 60000`): the node reads a move over that
span, and it exceeds the longest a quote can wait (1 s `max_age_ms`), a feed's lag (`max_delay`, 2 s in
`config/dev.toml`) and a held batch sync (5 s, Critical `log_sync_held`) together. Only BTC-USD, ETH-USD and SOL-USD
have data that short (the rogue-operator study's 50 ms grids); every other market's 60 s quantile is its
hourly quantile times **0.129**, the larger of square-root time (`sqrt(60/3600)`) and the grids' ratio
(0.112 to 0.122 for the three at 99.5%). That approximation is for the owner to approve: below one hour there
is no data for FX, indexes or metals.

**Proposed groups (D3), `results/proposal.toml`:**

| group | members and loadings | factor shock | residual shocks | weekend factor / residuals |
|---|---|---|---|---|
| crypto | BTC-USD 0.69, ETH-USD 1.06, SOL-USD 1.25 | 72.0 bp | 22.9, 28.9, 31.4 bp | none (always open) |
| fx | EUR-USD 0.90, GBP-USD 0.91, AUD-USD 1.19 | 9.6 bp | 4.9, 4.0, 5.9 bp | 117.6 bp / 69.5, 33.3, 64.1 bp |
| metals | XAU-USD 0.61, XAG-USD 1.39 | 58.6 bp | 17.0, 17.0 bp | 296.3 bp / 188.6, 188.6 bp |
| us_index | US500 0.96, US100 1.22, US30 0.82 | 26.3 bp | 5.4, 11.0, 11.7 bp | 406.4 bp / 38.6, 128.5, 86.1 bp |

**Floor share (D5): 0.80** of the per-position requirement, capped at it. With one share for all groups,
a hedged pair's saving is at most 20% of per-position margin: on the simulator its initial requirement is
10% to 20% lower at tier 2 (crypto 11% to 20%, indexes 10% to 20%, fx and metals 20%), 0% to 20% at tier 1
(means 5% to 13%) and nothing at tier 0, whose 5% margin floor is the requirement.

**Dropped:** the US shares need a floor share of 1.0 (their overnight and earnings gaps are covered by no
normal or weekend shock). USD-JPY, JP225 and WTI stay alone (D3). The shares' loadings and shocks are in
`groups.json`.

Shocks are stored to a tenth of a basis point, rounded up; loadings are fitted to a hundredth and stored in
thousandths. A weekend shock is the gap's 99.5% quantile, undivided: the weekend close-out reads the requirement
in full.

## What the study measured

Each file is in `results/`; the stage that writes it is in `run.py`.

- **Co-movement** (`comove_pairs.csv`, `comove_groups.csv`, `comove_lags.csv`): every pair of the 34
  launch markets at one hour (cap study's hourly bars, 102 weeks), one day (2016 on, `data/derived/daily_logclose.csv`)
  and across each kind of closure (weekend, overnight closure, daily break), as a body correlation
  (each series clipped at its 1st and 99th percentiles) and the tail dependence (the chance one is in its
  5% tail, 10% for closures, on the same side given the other is). Within a group the hourly body
  correlations are 0.66 (fx), 0.76 (metals), 0.80 (crypto) and 0.83 (indexes) at the median, tail dependence 0.47 to
  0.69 against 0.05 for independence; across groups 0.06 to 0.22. Weekend gaps agree more than hours
  (indexes 0.95, fx 0.85). **No member leads another:** correlations one to three hours apart are within 0.03 of zero,
  one day apart within 0.08, and among the indexes 0.12 to 0.16 negative either way round, so not a lead.
  Below one hour the study cannot say: the grids' sampled hours differ by market.
- **Loadings and stability** (`groups.json`): loadings are slopes on the group's equal-weighted mean; halves of
  the window agree within 0.15 (gold 0.72 then 0.58, silver 1.28 then 1.42, EUR-USD 0.95 then 0.82, BTC-USD 0.63 then 0.77).
- **The floor share and the groups** (`groups.json`, `groups_rows.csv`): books of two pairs, each hedged in full
  and each leg alone, long and short, at four sizes (a quarter of the first band, the first band, 4 and 20 times it) and three
  tiers, held a week from random hours on the account simulator (below). The floor share is the smallest on a
  0.1 grid at which every book's loss past balance (rate and size) is within twice per-position margin's plus
  a slack (0.2 points, 0.1 bp of gross). `phi_min_by_allowance` has 1, 1.5, 2 and 3 times: crypto 1.0, 0.9, 0.8, 0.8;
  fx 0.8, 0.7, 0.7, 0.7; metals 1.0, 0.8, 0.8, 0.7; indexes 1.0, 0.8, 0.6, 0.5; shares 1.0, 1.0, 1.0, 0.9. At 1 times (no more loss
  past balance than per-position margin's) every group but fx needs 1.0. The 2 times allowance is this study's choice; the owner can pick another.
  The proposal's 0.8 is crypto's and metals'. A group is dropped when no share below 1 passes, or when its saving at the proposal's share is
  15% or less (none is).
- **Coverage of the final requirement** (`coverage.csv`, `coverage_books.csv`): the share of sampled moves over the horizon
  that cost a book more than its equity at the close-out trigger, with the cap, the floor share, closing costs at ceilings in `K` and
  the fills' typical spreads in the loss. Joint sampling: the hours every member was open, both signs, each hour's return vector scaled by 0.129,
  so the members' joint tails stay as the data has them. **At 99.5% over 60 s every book meets the standard, per-position margin too**: today's
  first band at the close-out level is 1.2 to 2.6 times the 60-second move for ETH-USD, BTC-USD, the US indexes and the metals, 4.8 times for EUR-USD,
  10.8 times for SOL-USD and 8.5 to 23.6 times for the shares (`implied_horizon.csv`: the horizon at which the first band equals the
  99.5% move is 87 s for ETH-USD, 192 s BTC-USD, 137 s to 252 s for the US indexes, 88 s silver, 396 s gold, 23 minutes for EUR-USD,
  and 1.2 to 5.4 hours for the shares). So the standard alone does not set the floor share. Over one hour the rate rises to 0.8% to 5.7% for lone
  positions under both rules (0.05% to 0.9% for pairs), and the rule is within 0.4 points of the bands on each. Over the weekend gap, the shares of
  books within 0.5% are 100% of fx pairs under the rule (62% under the bands), 92% of index pairs (46%) and 50% of metals pairs (50%); the rule runs
  a gap-start check on every grouped market where today's runs it only on a market with a weekend rate (V60), so part of that gain is the check.
- **A hedged book against a single position** (`hedged_vs_single.csv`): the hedged pair under the rule loses past
  its balance far less often and by less than one position under its bands at the same tier, on the same history:
  at tier 2, crypto 2.8% against 12.3% (size 0.81 against 5.8 bp of gross), fx 0.15% against 2.1% (0.01 against 0.34 bp), metals 2.2% against 5.7%
  (0.52 against 2.5 bp), indexes 1.1% against 8.9% (0.13 against 2.3 bp). Against the same hedged pair under the bands the rule is no
  worse by more than the allowance above.
- **The long history's crisis days** (`crisis_days.csv`): each group's 12 worst days by its factor and by a hedged
  pair's residual since 2016 (crypto from 2017; Brexit and March 2020; January 2026's metals fall). A day is far past a 60-second
  standard: a hedged pair's day loses 9.6 to 17.2 times its equity at the trigger (crypto), 4.4 to 14.8 (fx), 5.7 to 8.9 (metals) and 4.6 to
  7.8 (indexes), and 4.3 to 7.7, 1.8 to 5.9, 2.3 to 3.5 and 2.1 to 3.5 times per-position margin, so per-position margin is no
  better there: the floor share multiplies that by 1.25.
- **Lone and large positions** (`lone_leverage.csv`, `lone_leverage_all_groups.csv`): with a floor share of 0.8 no lone
  position exceeds its own bands' leverage at the same size by more than 1.25 times (1 over the share). Against its launch maximum, BTC-USD and
  ETH-USD stay at 100x: their closing cost alone (`K`, 30 and 35 bps over the level) holds `M` above the first band, so the floor never binds.
  Where the bands are loose against the shocks the floor binds at every size: SOL-USD reaches 12.5x against 10x; EUR-USD 210x and GBP-USD 214x
  against 200x (at about $100M); gold 101x; the rest stay at launch. With every candidate group listed and no floor, BTC-USD would reach 3.65 times
  its bands' leverage but stays at 100x, ETH-USD 2.76 times and 100x, SOL-USD 52x, and AAPL 74.7x against its 10x; at a share of 0.5 the shares reach 20x.
- **Closing-cost shortfalls** (`ceiling_shortfall.csv`): at a close-out level of 25% (half launch's), per-position
  margin's first band falls short of its ceiling for 10 markets (BTC-USD, ETH-USD, the four FX majors and the four US and German indexes: BTC-USD's band is 25 bp
  against a 30 bp ceiling); at 50% and 75% none does. The cap leaves these exactly as today's.
- **The alternatives** (`alternatives.csv`): on hedged pairs, a 50% spread credit on per-position margin releases 12% to 25%, the
  factor grid at 0.8 releases 10% to 20%, a full historical scenario requirement (the 60-second empirical 99.5% loss over the level)
  releases 79% to 96% and loses past balance 18% to 25% of moves over an hour: too thin to rely on there. Its rate over 60 s is 0.5% by construction.
  The spread credit and the factor grid lose past balance in up to 1.9% and 1.5% of hour-long moves, against up to 1.0% for the bands.
- **Outages, halts and blocked corporate actions of one member** (`outage.csv`): a matched pair of opposite accounts, one
  member shut 4 to 168 hours, no protection of the hedge (D10), over 600 starts, at tier 2. The house's result per gross
  (positive is its gain; the clients' losses stop at their balances) at the start an operator would choose is
  -542 to -680 bp (crypto), -66 to -87 (fx), -289 to -398 (metals) and -179 to -200 (indexes) under the rule, against -574 to -680, -65 to -84, -287 to -398 and
  -179 to -197 under the bands: about the same. The mean result differs by up to 6 bp. The share of starts with a loss past balance is larger under the rule:
  fx 0.3% to 22.7% against 0% to 11.5% (twice at 168 hours), metals 14% to 56% against 12% to 46%, indexes 8% to 57% against 8% to 49%, crypto 27% to 69% against 25% to 67%.
  The initial requirement's treatment of a group with a shut member (D10: no offset) is not simulated; the runs hold positions already open.
- **Backtest** (`backtest.csv`): the hedged pair and the lone leg at each tier and size, mixed at hedged shares of flow
  0, 10, 25, 50 and 100%, under both rules: close-outs, loss past balance, leverage, released margin, the house's result. At tier 2, with no hedged flow,
  the house's result per gross (positive is its gain) is +8.9 bp (bands) and +8.0 (rule) in crypto, +2.9 and +3.6 in fx, -1.6 and +0.2 in metals, +5.9 and +4.1 in indexes;
  with half the flow hedged +18.8 and +17.6, +3.2 and +3.1, +6.3 and +5.4, +7.1 and +5.2. Accounts close out more often under the rule (fx 72% against 52% with no hedged flow, metals 75% against 60%,
  indexes 81% against 66%, crypto 83% against 80%, because the large lone positions hold less margin) and hold 6% to 18% more leverage.
- **Stricter bounds (D11)** (`bounds.csv`): the same groups at 99.9% and over 5 minutes and one hour; a shock can rise to 1.4 times
  (crypto), 3.6 times (fx), 3.2 times (metals) and 1.7 times (indexes) its proposed value before a hedged first-band pair needs per-position margin. Proposed:
  shocks up to the 99.9%, 5-minute values (factor: crypto 335 bp, fx 51.8, metals 274.9, indexes 154.9), and the floor share up to 1.0. The 99.9% rows rest on
  an hour reference of 52 windows (the rogue grids' sampled hours), so their scales (0.205 at 60 s, 0.431 at 5 minutes) are noisy.

## Sensitivity (`sensitivity.csv`)

| quantile | horizon | scale on the hour | share | groups kept | fx / indexes / metals factor shock (bp) |
|---|---|---|---|---|---|
| 99% | 60 s | 0.147 | 0.8 | crypto, us_index, metals, fx | 9.0 / 23.6 / 52.0 |
| 99.5% | 60 s | 0.129 | 0.8 | crypto, us_index, metals, fx | 9.6 / 26.3 / 58.6 |
| 99.9% | 60 s | 0.205 | 0.7 | fx | 24.7 / 73.6 / 130.7 |
| 99.5% | 10 s | 0.053 | 0.8 | crypto, us_index, metals, fx | 4.0 / 10.8 / 24.0 |
| 99.5% | 300 s | 0.289 | 0.8 | metals, fx | 21.4 / 58.8 / 131.0 |
| 99.5% | 3600 s | 1.000 | 0.7 | metals | 74.2 / 203.6 / 453.6 |

A looser quantile (99%) or a shorter horizon (10 s) keeps the same four groups and share; a tighter quantile (99.9%) or a longer horizon moves
the shocks toward the bands, so the offsets shrink and groups drop out: at 99.9% over 60 s only fx remains (at 0.7), over 5 minutes fx and metals (0.8),
over an hour only metals (0.7).

## Method

- **Model.** `pm/model.py` is the requirement as designed on #913: `PP = sum B_m(|N_m|)`, `L_g = F_g |sum beta_m N_m| + sum rho_m |N_m|`,
  `M = sum L_g + ungrouped bands + K`, `K = sum |N_m| c_m / level`, `R = min(PP, max(phi PP, M))`, at least the tier floor on gross,
  with the weekend values and rates at a gap's start. Margin bands are staircases (`market/mod.rs`, `staircase`).
- **Fit.** `pm/calibrate.py`. Factor: the members' mean return; loading: the slope on it; residual: what is left. Hourly quantiles over hours every
  member was open (hour moves open to close, plus the gap when the previous hour was open; closures' gaps read apart).
- **Weekend values.** The closures of 40 hours or more in the 102 weeks (about 100 weekends: the 99.5% quantile is close to the largest), raised by the ratio of
  the 2016-on Friday-to-Monday close returns' quantile to the window's where the long history is wider (indexes 2.1, fx 1.25).
- **Simulator** (`pm/sim.py`): accounts open at the tier's initial requirement under the rule, marked at each hour's close,
  closed whole below the close-out level times the requirement (per-position margin under the bands, R under the rule, both at the market's bands, without tier floors; a group closes together), at a weekend gap's start
  below the requirement in full at the weekend values, paying the bands' spreads and the 10% profit fee on each leg's realised gain; the loss past
  the balance is the house's. A weekend gap's start is the last open hour before a closure of 40 hours or more; today's bands run it only for
  a market with a weekend rate (V60).
- **Seeds.** 914 throughout; the stages are deterministic.

## Limits, and what is not established

- **The horizon below one hour is an approximation for every market but BTC-USD, ETH-USD and SOL-USD**: the hour's quantile
  times square-root time or the crypto grids' ratio. Index, FX and metal markets' microstructure at 60 s is not measured.
- **The standard is far inside today's bands.** The 60-second standard does not discipline the floor share; the share rests on the simulator's
  hourly check and the allowance of 2 times, which is this study's choice. A horizon of minutes or an hour makes `M` reach per-position margin for lone positions and
  for most hedged ones, so groups drop out instead (`sensitivity.csv`).
- **Fitted and judged on the same 102 weeks.** Loadings, shocks, the floor share and the weekend ratio use the whole window (the weekend ratio the long history
  as well), and the simulator runs on it. Only the loadings' halves are compared; no out-of-sample test of the 0.8 was run.
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
- **Books.** The floor share's books are pairs (and each leg alone) of the first two or three members at four sizes; baskets are in `coverage.csv` only.

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
    python -I run.py all          # horizon, comove, groups, coverage, checks, outage, proposal (about 3 minutes)
    python -I run.py sensitivity  # the groups stage at six standards (about 13 minutes)
    python -I -m pytest tests

Files: `pm/common.py` (paths, candidates), `pm/model.py` (the requirement), `pm/calibrate.py`, `pm/groups.py` (the floor share and
the groups), `pm/sim.py`, `pm/coverage.py`, `pm/extra.py` (crisis days, alternatives, bounds, outages), `pm/comove.py`, `pm/horizon.py`,
`pm/daily.py`, `pm/report.py`, `pm/io.py`, `pm/context.py`, `pm/stats.py`, `pm/checks.py`, `pm/hourly.py`, `results/manifest_<stage>.json`.
