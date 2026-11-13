# Cap calibration

## Summary

The results below are at the launch margins, with BTC-USD and ETH-USD at 100:1 (1% to $2M of
notional, 2% to $10M, 5% above, #500). Moving them from 10:1 (10% to $1M, then 20%) changes the
answers more than any other input: the same margin buys several times the notional on the markets
carrying most of the flow, so those markets fill their caps sooner and lose more when they gap. The
section "BTC-USD and ETH-USD at 100:1" lists every value that moved. This study changes no genesis
value.

Two measures of insolvency set the floors, and at 100:1 they agree at the 1% target. Over 90 days
from a house with no capital, on 2,000 simulated paths, each cell giving the measured share of
paths with the one-sided 95% upper bound beside it:

| Target | NAV never below zero | House cash never below zero (the vault covers client balances) |
|---|---|---|
| 1% | $37M in total: $1M on every market (0.85%; 17 paths, bound <1.27%). Refuses 64% of launch-week opening notional, 23% over 90 days. | $37M in total: the same $1M on every market (0.85%; 17 paths, bound <1.27%). The floors where each market's refusals end ($274M) measure 3.50% (70 paths). |
| 0.1% | $0 in total: nothing opens. The best candidate with any floor, $100k on every market, measures 0.35% (bound <0.66%). | $0 in total: nothing opens, at the same 0.35%. |
| 0.01% | $0 in total: nothing opens. | $0 in total: nothing opens. |

The 1% choice for both measures is the uniform family: $1M on every market. The risk-level method
below sets each market's floor from the market run alone, where a small market's own fees cannot
absorb its gap losses; alone, no market stays at or below 1% at any floor, so at 1% and below that
family puts $0 on every market and nothing opens. With every market at $1M, pooled crypto fees
carry the small markets. For 0.1% and 0.01%, every candidate with any floor measures above the
target, so the recommendation is $0. Against the 10:1 results, the house-cash 1% row falls from
$240M (refusing 0.1% over 90 days) to $37M (refusing 23%): at 100:1 the floors where refusals end
are no longer safe, because the caps there let BTC-USD and ETH-USD carry net exposures of $31.6M
and $17.8M, three times the $10M and $5.62M at 10:1, on a 1% margin.

- **Early dips are rarer and shallower at small floors, larger at large ones.** With $10M on every
  market, NAV falls below zero at some hour in 5.85% of paths (117 of 2,000), at a median of 0.1
  days after launch, before fees have built any capital; it falls below -$100k in 1.30% and never
  below -$1M. House cash goes below zero in 1.55% (31 paths), no withdrawal ever waits in the #49
  queue, and NAV ends the 90 days at a median of $75.7M. At the floors where each market's refusals
  end ($274M), NAV falls below zero in 24.6% of paths, below -$100k in 14.7% and below -$1M in
  1.9%.
- **Order size, not exposure, is what small floors refuse.** An order larger than the cap is
  refused whatever flows around it. $1M of margin buys $26.4M of BTC-USD or ETH-USD under the
  bands, so a floor under a few million refuses most of the flow on the markets that carry most of
  it: 91% of launch-week notional at $100k a market, 64% at $1M and 39% at $3M, against 13% at
  $10M, papertrade's position cap.
- **Along the risk-level allocations, the dip rises quickly with the floors.** From 1.45% at
  $1.18M in total (the 3% level), it passes 3.85% at $15.2M, 13.45% at $38.5M and 24.75% at $143M,
  reaching 24.6% at $274M, where every market sits at its refusal-ending floor. At 1% and below
  the family sets $0 on every market, since alone no market stays at or below the level.
- **Pooling carries the small markets.** Each market alone, at the floor where its refusals end,
  dips below zero in 39.5% (XAU-USD) to 73.0% (DE40) of paths, because its own fees are small next
  to its gap losses. Together at $10M a market the house earns a mean of $70.7M of fees over the 90
  days, and the crypto markets fund the others' risk.
- **The total floor does not bind.** At the chosen per-market floors ($1M on every market, $37M in
  total), a total floor at 25% to 100% of their sum leaves the measured risk unchanged and moves
  the refused share by 0.2 points in the first week and none over 90 days.

The assumptions that move these numbers most, each at the floors both measures choose at 1% ($37M
of floors), on 1,000 paths (the baseline gives NAV below zero 0.80% and house cash below zero
0.80%):

| Assumption | NAV<0 ever | Cash<0 | NAV<-$100k | Bad debt mean |
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

- **Farming is what makes the house solvent early.** Hedged flow pays about two thirds of the fees.
  Without it, NAV dips below zero in 12.6% of paths and house cash in 13.2%, at floors that
  otherwise measure 0.8%. Every scaling of the floors (x0.1 to x1) leaves them above 1% (`results/sensitivity.csv`).
- **Correlated client direction is among the largest market risks.** When clients lean the same way
  in every market, the house's net exposure adds up across markets instead of netting: 2.3% and
  2.2%.
- **Pairs held through closures are a risk floors barely touch.** A pair adds no net exposure, so a
  cap limits it only through the size of its orders, while a gap past a leg's balance is bad debt:
  $6.56M over 90 days in this stress, all of it on markets that close (BTC-USD and ETH-USD never
  do). Margin over closures, not floors, controls it.
- **How much flow trades at the maximum leverage decides the fees.** The baseline caps each
  position's leverage at the market's maximum, so most flow trades at it. Mapped instead to the
  same fraction of each market's maximum as of papertrade's 1000:1, fees fall from $58.4M to
  $3.17M over 90 days, with NAV below zero in 6.3% of paths and house cash in 3.6%.

Decisions for the owner:

1. **The measure and the target.** At 100:1 NAV below zero at any hour and house cash below zero
   choose the same floors at 1%, $1M on every market. A depth tolerance, such as NAV never below
   -$100k, admits more: $3M on every market measures 0.05% (1 path), refusing 39% of launch-week
   notional and 16% over 90 days, and $10M measures 1.30%.
2. **Whether to seed the house.** The early dip happens before fees accrue. Seed capital, not
   floors, removes it.
3. **Leverage on gap-prone markets.** Bad debt from gaps past 100:1 and 200:1 margin (oil, silver,
   indices), 10:1 earnings gaps on shares, and BTC-USD's and ETH-USD's 1% margin is untouched by
   floors. The hourly model fills a crypto close-out at its trigger, so it shows no bad debt there;
   the trade-level measure is in the 100:1 section.
4. **The utilisation stress level.** No level below 9500 keeps the fire rate under the bounds at
   100:1 on the launch-relevant floors; see "Stress triggers".
5. **#111's last acceptance criterion.** It puts the chosen values into the production genesis.
   This study recommends values but changes no config.

## BTC-USD and ETH-USD at 100:1

#500 moves BTC-USD and ETH-USD from 10:1 (10% margin to $1M of notional, 20% above) to 100:1 (1% to
$2M, 2% to $10M, 5% above, the genesis bands). The model caps each position's leverage at the
market's maximum, so most of the flow on the two markets, 71% of the whole, moves with the margin:
$1M of margin buys $26.4M of notional where it bought $5.5M. Every stage was re-run on the
committed `data/derived/` with the genesis bands and nothing else changed. The values that moved
against the committed (10:1) results, at the same seed and path counts:

| Value | 10:1 (committed) | 100:1 |
|---|---|---|
| BTC-USD alone: floor where refusals end | $10M | $31.6M |
| ETH-USD alone: floor where refusals end | $5.62M | $17.8M |
| BTC-USD alone at that floor: NAV<0 / cash<0 | 63.00% / 21.90% | 40.35% / 12.85% |
| ETH-USD alone at that floor: NAV<0 / cash<0 | 64.80% / 23.35% | 43.70% / 14.80% |
| BTC-USD, ETH-USD alone: mean fees over 90 days | $6.33M, $4.28M | $36.2M, $23.2M |
| Floors at 1%, NAV measure | $1M on every market ($37M; 1.00%) | $1M on every market ($37M; 0.85%) |
| Refused at those floors, week 1 / 90 days | 47.0% / 20.1% | 64.1% / 22.9% |
| Floors at 1%, house-cash measure | $240M (0.80%) | $37M (0.85%) |
| Refused at those floors, week 1 / 90 days | 0.3% / 0.1% | 64.1% / 22.9% |
| Refusal-ending floors, every market: total | $240M | $274M |
| at them: NAV<0 / cash<0 / NAV<-$100k | 19.7% / 0.80% / 3.60% | 24.6% / 3.50% / 14.7% |
| at them: NAV day 90 p50 / mean fees | $26.2M / $25.3M | $75.7M / $74.1M |
| $10M on every market: NAV<0 / cash<0 / NAV<-$100k | 18.75% / 0.85% / 3.40% | 5.85% / 1.55% / 1.30% |
| Bad debt, mean over 90 days, at the refusal-ending floors | $632k | $632k |
| Utilisation level 1 (`utilisation_bps`) | 7000 | none below 9500 passes |

**Floors.** The 1% choice for both measures is now $1M on every market. The floors where refusals
end let BTC-USD and ETH-USD carry net exposures three times those at 10:1 on a margin a tenth the
size, and the house-cash measure at them rises from 0.80% to 3.50% (70 paths). The floors the
owner has yet to set therefore trade a larger refused share (64% of launch-week notional at $1M a
market, 13% at $10M, against 1.9% at $10M at 10:1) for a smaller dip. NAV and fees are larger
throughout because the flow carries more notional per margin; that rests on the approximated
100:1 flow profile (Limits).

**Bad debt.** The mean is unchanged because the hourly model fills a crypto close-out at its
trigger, so BTC-USD and ETH-USD show $0 at either margin; the $632k comes from the markets that
close. The 100:1 gap risk is therefore not in this study's results. Paired positions held in two
accounts at maximum leverage are the case the net exposure caps cannot limit: a leg whose price
jumps past its margin with no close-out in between is bad debt to the house, which takes a jump of
about 1.1% at 1% margin against about 11% at 10:1. The spread study (#623) measures it on 50 ms
trade grids of Coinbase and Kraken, at 1% margin:

- the index passes the half of the margin left at the close-out trigger within the engine's next
  price (up to 3 s) for about 3 seconds a day over the two markets, with a mean loss past the
  balance of 0.5% of the position's notional, and for 25 minutes of 2025-10-10's four stress hours;
  at 2% margin it is under a second a day, and at 5% none in the year sample and 3 seconds on the
  stress hours;
- paired positions across unpriced moves take about nothing a day, but $19.1M over those four
  stress hours, and stop paying at 2% margin.

**Utilisation levels.** The fire rates (table in "Stress triggers") rise at every level because
BTC-USD fills its cap far more often: at 7000 it fires in 19.0% of open hours at the refusal-ending
floors (2.9% at 10:1) and 51.7% at a uniform $10M. No candidate below 9500 keeps the flow-weighted
rate under 2% on both launch-relevant floor families. The genesis values (7000 and 9500) are
unchanged; whether to move them, or to give BTC-USD and ETH-USD their own, is the owner's.

**Proposed to `config/production-genesis.toml`:** nothing. #500 changes no genesis value, and the
move and divergence levels do not depend on margin: the stage reproduces the committed 75/200 bps
and 6001/9500.

Provenance: `run.py per-market`, `joint`, `sensitivity` and `stress` on the committed
`data/derived/`, seed 1, 2,000 paths (1,000 for the sensitivities), three workers; 21, 44, 89 and
16 minutes (`results/manifest_*.json`).

## The question

The house is the counterparty to every trade and starts with no capital (#15 §2). Each exposure
cap, per market and in total, is the larger of a share of house NAV and a fixed dollar floor (#45,
PR #99). At launch, NAV is near zero, so the floors are the whole of the house's exposure: they
decide how much unbacked risk the house carries before trader losses have built its capital, and
how many orders it refuses while it is small (#111).

This study estimates, for each market and in total, how the floors trade the probability of house
insolvency over the first 90 days against the share of opening notional refused for capacity, from
papertrade.xyz's on-chain trader behaviour and the launch markets' price history. It recommends
floors at three insolvency targets and reports the sensitivity to the main assumptions. The values
are not applied to `config/production-genesis.toml`.

## Insolvency measures

Each is a probability over 90 days of simulated trading, starting from a house with no capital.

| Measure | Event |
|---|---|
| NAV below zero, ever | House NAV (house cash less clients' unrealised PnL) is below zero at some hour-end. |
| House cash below zero | At some hour-end house cash is below zero: the vault holds less USDC than clients' balances, which bound from above what clients may withdraw on demand. This is NAV below the withdrawals owed, on a realised basis. |
| Queue | Under the modelled withdrawals, some withdrawal waits in the #49 first-in-first-out queue because the vault cannot pay it. |
| NAV below zero at day 90 | House NAV is below zero at the end. |
| NAV below -$100k, -$1M | The lowest NAV falls below that depth. |

The calibration uses the first two. Each is the share of simulated paths in which the event
happens, with the count of those paths and the exact one-sided 95% upper bound beside it; nothing
is extrapolated past what the paths count. On 2,000 paths a target of 1% allows 20 failing paths,
0.1% allows 2, and 0.01% allows none, and no failing path bounds the rate only below 0.15%: the
two smaller targets are met in the paths, not established at 95%.

The cost side is the share of opening notional refused for capacity (directional flow and churn),
in the first week, when the floors carry the house, and over the 90 days.

## Data

### papertrade.xyz trader behaviour

papertrade.xyz runs a house-counterparty perpetuals venue on Hyperliquid's EVM (HyperEVM, chain
999): an LP that starts at zero takes the other side of every trade, losers pay nothing beyond
their loss, winners pay a 2% fee after an impact haircut that keeps at most 90% of a gain, and
losses mint the PAPER token, 100 per $1 while the LP is below $2M. It lists BTC and ETH at up to
1000:1 with isolated margin and a $10M position cap ([docs](https://docs.papertrade.xyz),
[contract addresses](https://docs.papertrade.xyz/dev/contract-addresses/),
[impact haircut](https://docs.papertrade.xyz/how/asymmetric-impact/),
[mint curve](https://docs.papertrade.xyz/how/mint-curve/)).

`fetch/papertrade.py` pulls every event log of the Exchange proxy
`0x6cd5661646289fb6e65ea5c032310fded797d0a2` from its creation block 47,854,719 to block
49,109,718 (2026-10-21 05:42 UTC) from six public HyperEVM RPC endpoints, 1,255 chunks of 1,000
blocks, and block timestamps every 10,000 blocks. 25 randomly chosen chunks, fetched again from an
endpoint chosen at random, matched their stored log counts. The raw pull is 212 MB and gitignored.

The Exchange's source is not verified on the explorer, so `derive/papertrade.py` infers the event
layouts from the logs and checks them:

- every one of the 1,580,269 closes and 132,923 liquidations matches one of the 1,715,360 opens;
- the median ratio of a close's fee to its haircut PnL is 0.0200, the documented 2% win fee and
  LP-side loss fee, over the 1,374,753 closes with non-zero haircut PnL;
- BTC entry prices sit within 0.1% (median) of Binance's BTCUSDT close of the same hour;
- deposits ($351.2M) less withdrawals ($305.9M) give $45.3M, against the $45.36M of USDC the
  explorer showed the Exchange holding on 2026-10-21.

The contract's open-interest event is not used: its changes do not follow position flows, and
until block 48,217,087 it carried a fixed offset of about $5B a side.

Trading opened at 2026-10-10 14:00 UTC, so the data covers 10.6 days: 18,504 accounts and $713B
of notional. Both legs of a hedge are *hedged*, for the notional they offset: the part of a
position that offsets its account's open opposite notional in the same asset not already offset,
with the same amount of those opposite positions, earliest first (when a position closes, what it
offset can be offset again); and two accounts' opposite positions of the same notional that open
and close within two blocks of each other, neither hedged within its account. A partly hedged
position counts in both kinds by share.

The derived dataset, committed under `data/derived/`, is the flow profile the model samples
(`papertrade_flow.csv`: margin by kind, hold class, and leverage bin), the size profile
(`papertrade_sizes.csv`: by kind and launch margin schedule, the margin-weighted distribution of
each position's notional on that schedule, under either leverage mapping), the leverage profile
(`papertrade_leverage.csv`: the notional each leverage bin's margin buys on each schedule), the
daily series (`papertrade_daily.csv`), the holding-time table (`papertrade_holding.csv`), and the
summary (`papertrade_summary.json`). `results/report.md` tabulates them. The flow, size, and
leverage profiles and the long-share fit count the closed positions opened on the ten full days
that also set the daily margin path (1,608,754 positions); the partial first and last days and
the 2,168 positions still open at the end, the longest holds among them, are left out.

What the data says about the brief's farming premise:

- Hedged flow is 41% of all notional and 67% of exposure-time, but the launch afternoon was the
  most hedged: on the ten full days it is 24% of notional and 41% of margin, which the model
  uses. Within accounts, 60% of gross exposure-time is offset by the same account's opposite
  positions, and market-wide 87%.
- Holds are short: the median position lasts 64 seconds, but 71% of exposure-time comes from
  positions held over an hour and 9% from positions held over a day.
- Hedged flow pays $0.235 of haircut and fee on its winning legs per $1 lost on its losing legs.
  On this engine a winning leg pays a 10% profit fee and no haircut, so the same farming costs about
  $0.10 per $1 of loss, and the house takes less than half of what papertrade takes from it.
- Flow fell from $373M of margin on the first full day to $79M on the last.

### Launch markets

The study runs on the markets `config/production-genesis.toml` lists: 34 since #685 deferred DE40,
UK100 and BRENT (#673). `data/derived/` keeps the bars and statistics of all 37, and `run.py`
drops the ones the genesis no longer lists. The committed `results/` are from the 37-market
genesis; in the `retained` stage the 37-market count is now the 34 launch markets and three clones.
A re-run on main moves these figures this README and the genesis cite. **Toxicity (#130):** not
because of the deferral, since the same run on the 37-market genesis gives the same values, but
because #535 set BTC-USD and ETH-USD to 1% margin after the calibration (#408), which raises their
median fill from $100k to $1M and their p99 from $3.2M to $14.8M, so ETH-USD rather than Brent binds
the memory. `memory` rises from $11,259,036 to $17,626,099 and `prior` from $5,629,518 to
$8,813,050, a toxic account reaches tier 1 within 100 median-size fills rather than 75, and the
half-second burst crosses tier 1 1.0% of the time rather than 3.9%. The tier thresholds (1 and 2
bps) and their whole-bps spreads (2 and 3) do not change. The genesis keeps the committed values
until the owner decides; F3 (#690) replaces this score. **Stress (#133):** every chosen value is
unchanged (the 60 s window, 75 and 200 bps moves, divergence share 6001, utilisation 9500); the
flow-weighted utilisation fire rates in that section rise by up to 1.5 points (on a uniform $10M,
29.4% to 30.5% at 7000 and 12.7% to 13.2% at 9000), and still no level below 9500 passes. The
`per-market` stage prices each market alone, so the deferral leaves it as it was. The `joint`,
`sensitivity` and `retained` stages were not re-run on 34 markets; their totals scale with the
market count (at $1M a market, $34M rather than $37M), and their tables are the 37-market record.

When the study ran, the genesis listed 37 markets. `fetch/markets.py` pulls, for each:

| Data | Source |
|---|---|
| Hourly OHLC, 2024-10-28 to 2026-10-12 (102 weeks) | Crypto: Binance spot klines (`data-api.binance.vision`). Everything else: Yahoo Finance chart API (`query1.finance.yahoo.com/v8/finance/chart`), which serves hourly bars for the last 730 days. |
| Daily closes from 2016 | FRED (St. Louis Fed) where an official series exists: `DEXUSEU`, `DEXUSUK`, `DEXJPUS`, `DEXUSAL`, `SP500`, `NASDAQ100`, `DJIA`, `NIKKEI225`, `DCOILWTICO`, `DCOILBRENTEU`. Otherwise Binance daily klines (from August 2017; SOL from August 2020) or Yahoo daily bars, dividend-adjusted for shares. |
| Benchmark rates | FRED `SOFR` (3.87%), `IUDSOIA` (SONIA, 3.73%), `ECBESTRVOLWGTTRMDMNRT` (ESTR, 2.44%), each on 2026-10-19 or the latest day before, read into `benchmarks.csv`; TONA 0.75% and AONIA 3.60% are assumptions. Until #33 sequences rates the engine counts a benchmark as zero; the model charges these. |
| Market mix | Hyperliquid's public info API (`api.hyperliquid.xyz/info`, daily candles 2026-09-20 to 2026-10-19): each market's mean daily notional on its Hyperliquid listing, main dex or the builder-deployed `xyz` dex. |

Each launch market's stand-in series (`catalogue.py`): Binance BTCUSDT, ETHUSDT, SOLUSDT; spot FX;
CME E-mini S&P 500, Nasdaq-100 and Dow front months for US500, US100 and US30, as the venue
calendars use; CME's Nikkei yen future for JP225; COMEX gold and silver and NYMEX WTI and Brent
front months; the shares' regular sessions. DE40 and UK100 use the DAX and FTSE 100 cash indices,
as the venue futures are not public.

`derive/markets.py` cleans the hourly bars:

- A gap across a one-hour step is set to zero. The public futures series alternates between
  front- and next-month contracts around a roll (consecutive bars jump by ±80 to 170 bp and back),
  and a continuously traded market's next bar opens at the previous close.
- For crypto, FX, and futures, a gap after a hole of 3 to 24 hours is set to zero unless the hole
  touches a US market holiday or early close, or the day either side: the futures series switches
  contract at the other holes (ES jumped 2.6% across one on 2024-12-20). Gaps over the daily
  break, weekends, holiday halts, and holidays are kept.
- DE40 and UK100 fold each weekday overnight gap into continuous movement, since their venue
  futures trade 20 or more hours a day, and keep weekend gaps, taken to the first bar's close
  because the FTSE series opens at the previous close.

`market_stats.csv` gives each market's volatility, the reopening gaps a year past half its first
margin tier (where a position at the maximum leverage is closed out) and past the whole tier (where
it loses more than its balance), and the largest hourly and daily moves, over the hourly window
and over the daily history from 2016. `results/report.md` shows it in full.

For the ten markets with an official FRED series, `market_stats.csv` also compares week-on-week
returns of the hourly data, at Friday's last bar, with the FRED series. Correlation is 0.978 for
US500, 0.981 for US100, 0.943 for US30, 0.937 for WTI, and 0.935 to 0.953 for the FX pairs, whose
FRED rates are the noon New York fixings. JP225 (0.803) and Brent (0.816) are compared with the
Tokyo close and the London Brent spot assessment. Volatility ratios are 0.94 to 1.13. The hourly window is a high-volatility one for metals and several shares (gold 26% a
year against 17% since 2016, silver 57% against 35%) and a calm one for crypto (BTC 44% against
67%).

## Model

`run.py` runs the Monte Carlo (`model/`) hour by hour over 90 days, from a house with no capital.

**Prices.** A path is 13 historical weeks, drawn in blocks of four consecutive weeks from the 102,
the same weeks for every market. That keeps cross-market correlation, volatility clusters,
weekend and holiday gaps, and each venue's sessions. Each market's mean hourly return over the
window is removed, so trends in the history do not decide who wins.

**Flow.** papertrade's flow is carried over by margin, the collateral traders commit, not by
notional, because these markets allow 10:1 to 200:1, not 1000:1. Each day brings papertrade's
margin of the same day after launch (its ten full days in order, then the mean of the last seven,
$182M a day). Each market takes its Hyperliquid volume share of it (crypto 76.5%, indices 8.8%,
oil 6.0%, metals 3.1%, shares 5.5%, FX 0.1%), spread over its open hours. A market with no listing
takes its asset class's smallest listed share. Each papertrade class keeps its share of margin,
hold, and long share. Each position's leverage is capped at the market's maximum, the first margin
tier, so most flow trades at the maximum, and its notional is no more than its margin buys under
the market's margin bands, which charge each slice of notional at its band's rate: $1M of margin
buys $10M of a 10:1 market in one tier, $3.3M of a share (10% to $500k, 20% to $2M, then 50%), and
$26.4M of BTC-USD or ETH-USD (1% to $2M, 2% to $10M, then 5%).

- *Directional* positions open at the hour's close and settle after their class's hold (1, 4, 16,
  or 48 hours); a planned exit inside a closure waits for the reopening. The long share follows an
  hourly AR(1) around each class's measured share, fitted to papertrade's BTC flow on the full
  days (coefficient 0.69, residual 0.12, and +0.05 per unit of the last hour's log return).
- *Hedged* positions open as offsetting pairs in two accounts, since an engine account nets to
  one position per market. A pair ends when either leg is closed out, the other leg closing with
  it, or at its hold.
- Positions held under an hour (churn) pay the profit fee on their expected gain over their hold,
  at the hour's Parkinson volatility. Their mean open net notional over the hour competes for cap
  room and takes the hour's move from its open, so not a reopening gap, and churners fund what
  they owe.

Every opening order, directional, paired, or churn, must be one the engine fills. An order whose
fill would take the house's net exposure further past a cap is refused, whatever flows around it;
one that shrinks the exposure fills whatever its size (`check_risk`). So each side of each class
keeps only the share of its margin in orders small enough for its side's room, from papertrade's
size profile on the market's margin schedule: up to the room under the market and total caps for
the side that grows the exposure, and up to that plus twice the exposure for the side that shrinks
it. A pair's two accounts open first the leg that fits. With no room, as at zero NAV, a zero
floor, and no exposure, nothing opens. Of what fits, the side that reduces the house's net
exposure fills in full and the crowded side up to the cap. A pair leaves the house's net exposure
unchanged, but the engine checks each leg in turn and the returning leg lands the exposure back
where the pair found it, so once that state is past a cap — as when NAV has fallen — no pair
opens, whichever leg opens first.

**Engine rules** (`model/accounting.py`, each against the engine function it mirrors; the tests
check them on hand cases from the engine's own tests):

- a 10% profit fee on realised gains, nothing on losses (`profit_fee`);
- financing at each 22:00 rollover on every open position's notional at the mark: longs pay
  benchmark + markup, shorts markup - benchmark, at the genesis markups and day bases
  (`daily_financing`);
- close-out when equity falls below 50% of margin, at the trigger in continuous trading or at the
  bar's open when the market gaps through it, only while the market is open (`closeout.rs`);
- negative balance protection: a loss beyond the account's balance, after the financing it has
  paid or received, is bad debt against the house, each account of a pair on its own
  (`settle_bad_debt`);
- USD settlement (#98): USD-JPY, Japan 225 (yen), Germany 40 (euros), and UK 100 (pounds) accrue
  PnL in their quote currency and convert it at the engine's own USD-JPY, EUR-USD, or GBP-USD
  price, from the same bootstrapped weeks, so their margin, close-outs, caps, and NAV are in USD;
- each cap the larger of its genesis NAV share (`house_cap_bps`, `house_total_cap_bps`) and its
  floor; an order that would take the house's net exposure past a cap is refused (`check_risk`).

**Sensitivities.** Thirteen variants start from the floors the house-cash measure chooses at 1%:
no hedged flow, twice the hedged flow (directional flow unchanged in both), hedged flow that
halves every 30 days with the mint rate, pairs held through closures,
volatility x1.5 and x2, leverage mapped relative to each limit, markets bootstrapped
independently, one client long share for every market, flow x0.25 and x4, exits before closures,
and history with its drift kept. In the closure variant, at the last open hour before each closure
of 3 hours or more, farmers open pairs at the maximum leverage worth 10% of the margin the market
accepted in hedged pairs held an hour or more over about the past week (not hedged churn, whose
margin turns over within the hour), in orders that fit, and close them at the reopening, where a
gap past a leg's balance is bad debt. Each variant runs with every floor scaled
together by x0.1, x0.3, and x1.

**MONKEY.** Losing trades mint MONKEY at a rate per dollar of realised loss (#36), but genesis
sets neither the rate nor the half-life, and MONKEY moves no house cash. The model reports the
loss basis it mints on. The incentive enters through the hedged flow, which the farming share
scales, and a half-life on it is a sensitivity.

**Vault and withdrawals.** Exit proceeds go to clients' free balances. New margin is drawn from
free balances first and deposited for the rest. Half of each hour's remaining exit proceeds is
withdrawn, and free balances leave at 5% a day, or 55% a day while NAV is below zero (a run on a
public log), each compounded hourly. A withdrawal the vault cannot pay joins the #49 queue and is
paid as deposits arrive.

**Spreads and a holding-time fee.** The genesis prices every fill at the index, so by default the
model charges no spread and the standard 10% profit fee. `Scenario.spread_bps` charges each fill a
spread per market, which the house keeps as `spread_income`, and `Scenario.fee_schedule` replaces
the 10% with a fee that depends on the hold (`accounting.HoldFee`): the engine's table under #614's
decision D7, `max(base x (1 - discount), C(hold))`, or #623's log-linear family. The engine charges
the fee on realised PnL at the spread-adjusted fill prices, so with either field set the fee is on
the gain after both fills' spreads: a position held an hour or more pays it on its realised PnL less
the spread, and churn on its expected gain over its class's holds,
`fee(h) * sigma * sqrt(h) * psi(2 s / (sigma * sqrt(h)))` (`holdfee.py`). The derived flow records two
moments of each churn class's holds, not their distribution, so `Scenario.churn_holds` takes either
the least fee any distribution with those moments allows (`bound`, the default) or the measured
within-hour holding profile tilted to them (`central`). Runs record both (`churn_fee`,
`churn_fee_central`) and the fee at the mean hold on the gain before spreads (`churn_fee_gross`).
`research/spread-calibration` (#615, #697) uses them, and `flow.with_genesis_profiles`, which gives the
genesis's 100x bands on BTC-USD and ETH-USD a size and leverage profile approximated from a profiled
schedule with the same first rate. No stage here uses them, so the committed results keep their
values; a re-run adds `spread_income_mean` (zero without spreads) and `filled_mean` columns and the
fields to the manifests.

**Calibration.** Each market alone, at its share of the flow, gives each measure as a function of
its floor, over 18 floors from $0 to $100M. A *risk level* `q` sets every market's floor to the
largest floor at which that market alone stays at or below `q`, every smaller floor too. That
floor is capped where the market stops refusing orders (under 1% refused over 90 days) and is zero
when even a zero floor exceeds `q`. The total floor is the sum. A market alone cannot pool its
fees with the others', so a second family of candidates puts the same floor on every market:
$100k, $300k, $1M, $3M, and $10M. All markets together then run every candidate, twelve levels
for each measure and the five uniform floors, on the same 2,000 paths. For each measure and
target, the recommendation is the candidate that meets the target and refuses the least
directional notional over the 90 days. At the choice for 1%, a second run sets the total floor to
25% to 100% of the sum, where the total cap starts to bind.

## Results

`results/report.md` holds every table: the data summaries, each market alone, all markets
together at each risk level with the floors chosen per target, the total-floor sweep, and the
sensitivities. The three tables below are all markets together: the risk-level floors for each
measure, then the same floor on every market.

Floors set for NAV below zero at some hour (each market's floor at the largest of its own tested
floors that stays at or below the level when the market runs alone):

| Level | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 | Fees mean | Bad debt mean |
|---|---|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 | $0 | $0 |
| 0.01% to 1% | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 | $0 | $0 |
| 3% | $1.18M | 2 | 1.45% | 1.55% | 0 | 81.6% | 28.3% | $61M | $54.1M | $509k |
| 10% | $15.2M | 12 | 3.85% | 2.00% | 0.40% | 35.3% | 13.4% | $72.6M | $66.5M | $574k |
| 20% | $21.1M | 15 | 4.30% | 1.75% | 0.55% | 28.6% | 11.6% | $74M | $68.3M | $586k |
| 30% | $38.5M | 17 | 13.45% | 2.15% | 5.10% | 18.6% | 7.0% | $75.4M | $71.4M | $596k |
| 50% | $143M | 29 | 24.75% | 3.50% | 14.60% | 3.6% | 1.0% | $75.7M | $74M | $630k |
| binding | $274M | 37 | 24.60% | 3.50% | 14.70% | 2.4% | 0.7% | $75.7M | $74.1M | $632k |

Floors set for house cash below zero at some hour:

| Level | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 |
|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 0.01% to 1% | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 3% | $278k | 2 | 1.00% | 1.05% | 0 | 94.3% | 35.6% | $55.3M |
| 10% | $29.9M | 12 | 16.35% | 2.60% | 6.20% | 23.5% | 7.7% | $75.2M |
| 20% | $104M | 20 | 24.85% | 3.50% | 14.60% | 3.9% | 1.0% | $75.6M |
| 30% | $210M | 26 | 24.65% | 3.50% | 14.70% | 2.5% | 0.7% | $75.7M |
| 50%, binding | $274M | 37 | 24.60% | 3.50% | 14.70% | 2.4% | 0.7% | $75.7M |

The same floor on every market:

| Floor | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 | Fees mean | Bad debt mean |
|---|---|---|---|---|---|---|---|---|---|---|
| $100k | $3.7M | 37 | 0.35% | 0.35% | 0 | 90.9% | 33.1% | $57.3M | $50.5M | $491k |
| $300k | $11.1M | 37 | 0.75% | 0.75% | 0 | 82.6% | 28.8% | $60.7M | $53.9M | $522k |
| $1M | $37M | 37 | 0.85% | 0.85% | 0 | 64.1% | 22.9% | $65.4M | $58.4M | $558k |
| $3M | $111M | 37 | 1.30% | 1.25% | 0.05% | 38.8% | 16.4% | $70.5M | $63.7M | $605k |
| $10M | $370M | 37 | 5.85% | 1.55% | 1.30% | 13.1% | 8.1% | $75.7M | $70.7M | $632k |

Each probability is the share of the 2,000 paths. The recommendation for a target is the
candidate whose measured share is at or below it that refuses the least; the one-sided 95% upper
bound beside each count is the uncertainty on the measurement, not the selection criterion. With
zero failing paths that bound is 0.15% on 2,000 paths (0.30% on the 1,000-path sensitivity runs),
so a pick measuring exactly at its target sits within sampling noise of it. Refused shares are of
opening notional, directional flow and churn.

## Limits

- **The 100:1 flow is approximated.** BTC-USD's and ETH-USD's bands have no profile in the
  derived papertrade data (their size and leverage profiles are scaled from the oil schedule with
  the same first-tier rate, `flow.with_genesis_profiles`), so every stage's notional, fees and
  exposure on the two markets, and the floors they set, rest on that scaling. Papertrade's traders
  chose their leverage at 1000:1; the `leverage relative to max` row bounds how much of it
  carries over.
- **Ten days of behaviour.** The papertrade data covers 10.6 days of a new venue: two BTC/ETH
  markets at 1000:1, crypto-native users, and a launch spike. Holds, leverage, and long share on
  RWA markets, at lower leverage and with closures, are assumptions carried over from it.
- **Fixed holds.** Each hold class settles at one hold, 1, 4, 16, or 48 hours; papertrade's
  notional-weighted mean holds within the classes are 1.4, 3.9, 13.8, and 54.3 hours.
- **Volume.** The flow level assumes this engine draws papertrade's margin. Fees scale with it, so
  the `flow x0.25` and `flow x4` rows bound the effect.
- **Two years of hourly history and 2,000 paths.** The tails come from 102 weeks (late 2024 to
  late 2026), which include April 2025 and March 2026 but not March 2020; the volatility rows
  stress them. 2,000 paths count a 1% rate to within about half a point, but cannot tell 0.1%
  from 0.01%: no failing path bounds the rate only below 0.15%.
- **Price stand-ins.** DE40 and UK100 use cash indices. Futures series carry roll artefacts that
  the cleaning removes only at holes and one-hour steps; a roll that coincides with a weekend stays
  in that weekend's gap, which overstates gaps for WTI and Brent. Yahoo's hourly bars are a public
  but unofficial source.
- **Engine detail left out.** Size spreads, stress spreads, and toxicity spreads would add house
  income, and leaving them out is conservative. Further gaps: cross-margin between an account's
  markets (each position is its own account); the margin bands set how much notional margin buys,
  but the close-out level stays at half the first tier's rate, below the engine's for orders past
  the first band, so those close out later and leave less balance against a gap; close-outs
  triggered by financing (financing moves what an account can lose but not its trigger); the
  stale-price rule that stops dealing when venues disagree (#15 §1), and its #98 form for a
  market whose FX market has no price.
- **Order sequencing and size.** Within an hour the model nets what fits, against the exposure at
  the hour's start: it assumes the orders that reduce exposure arrive spread among the crowded
  ones. When room is small next to the orders, the engine, checking them one by one, refuses more.
  Orders keep papertrade's sizes; a trader who splits an order under a small cap gets more of it
  filled than the model gives.
- **Close-out fills.** In continuous trading a close-out fills at its trigger. A crash inside an
  hour that the venue index jumps across fills worse in the engine, so crypto bad debt is
  understated.
- **Withdrawals.** The withdrawal and run rates are assumptions; the queue measure depends on
  them, and the house-cash measure does not.

## Stress triggers

The launch stress triggers (`config/production-genesis.toml`, #118) are dev values. The `stress`
stage calibrates them — the fast-move thresholds and window, the divergence-trigger shares, and
the utilisation levels — so each automatic level fires when the house's risk actually rises, not
on noise. `run.py stress` writes `results/stress_moves.csv`, `stress_divergence.csv`,
`stress_utilisation.csv`, and the chosen values with their method in `stress_choice.json`.

The engine's automatic level rises at once when any reading meets a level's trigger and steps down
only after a five-minute cooldown (`stress.rs`), and every reading is a share of a market-relative
bound: a fast move in bps of the index mid, venue divergence in bps of `max_divergence_bps`, and
house exposure in bps of the market's cap at house NAV.

- **Fast move** (thresholds and window, from the market statistics). The trigger reads the largest
  distance of the index mid from its trailing window's lowest or highest mid. Each market's
  per-second volatility comes from its hourly bars' Parkinson range; one minute-scale path per
  sampled history hour (400 hours per market), at the engine's one-second cadence, bucketed window,
  and cooldown hysteresis, gives each candidate's share of time at each level and the volatility
  percentile its firings select. Each simulated hour opens with two window-lengths of warm-up,
  discarded from the tables, so every recorded second reads against a full window, and the
  hysteresis is evaluated within each hour — no state crosses the independent paths. The chosen
  thresholds are the most protective pair whose flow-weighted time at level 1 stays under 1% (level
  2 under 0.1%) and every market's level-1 share under 5%. At a 60s window the candidates measure:

  | (t1, t2) bps | time at L1, flow-weighted | worst market at L1 | mean vol pct of L1 firings |
  |---|---|---|---|
  | (50, 150) | 1.51% | 10.1% | 0.975 |
  | (60, 150) | 0.80% | 6.6% | 0.985 |
  | **(75, 200)** | **0.37%** | **3.8%** | 0.991 |
  | (100, 250) | 0.14% | 1.4% | 0.994 |
  | (150, 300), launch | 0.014% | 0.39% | 0.995 |

  The last column is the mix-weighted mean over the markets of each market's mean firing-hour
  volatility percentile, the same aggregation `choose_move` reports.

  The window stays at 60s: 30s and 120s give parallel fire-rate curves (a longer window raises
  every reading alike), and 60s keeps the window's dozen buckets at one-per-6s against feeds that
  publish at least a mid a second. The choice, 75 and 200 bps, fires on the volatile crypto, oil,
  and share markets — 0.1–1.2% of the time on those carrying at least 1% of the flow, up to 3.8%
  on the smallest share listings, the worst market within the 5% bound — and near-silent on FX
  and indices (0.06–0.4% of the time on the index markets, zero on FX; their main protection is
  the divergence trigger and the stale-price halt); the launch 150/300 fires on none of them
  outside crashes. The hysteresis cooldown and the calibrated window
  are read from the genesis the stage calibrates, so a parameter change there cannot silently drift
  the tables away from the engine they model.
- **Divergence** (shares, from the stale-fill cost model). A divergence reading of `d` bps means
  the venue mids sit `d` apart. With the protocol's two fresh sources the index mid is their
  midpoint, and a fill at the index mid loses up to `d/2` bps if the mark snaps to the other venue
  (`d/4` expected, uniform). That bound is exact for the two-source midpoint index; with more fresh
  sources the index is their median and the worst-case loss can exceed `d/2` (up to nearly `d`), so
  the shares would need re-deriving against the true loss model if a market's source count
  changes. A share `s` of a market's bound `B` fires at `d = ceil(s·B/10000)` bps — the engine
  truncates the share. The level-1 choice is the lowest share whose worst-case stale-fill loss at
  fire covers the level's minimum spread on the tightest-bound market (`d/2 ≥ min_spread`): it
  needs a 4-bps reading, and on the tightest 5-bps bounds share 6000 still fires at 3 bps while
  6001 is the lowest firing at 4 — so 6001 is the choice, firing at 16 bps on crypto's 25-bps
  bound. Level 2 would need a 20-bps reading, infeasible where the tightest bound is 5, so 9500 is
  the documented fallback: on bounds below about 21 bps it first fires at the bound itself (which
  still prices — the stale-price rule refuses only readings above it), the last widening before a
  halt, and on crypto's bound it fires at 24 bps. The launch 6000/8000 fires on the tightest
  markets below the stale-fill cost of the level-1 spread; the expected-cost rule
  (`d/4 ≥ min_spread`) is reported in `stress_divergence.csv` and is unreachable at any share
  under 10000 on the 5-bps markets.
- **Utilisation** (levels, from the Monte Carlo). The model records each market's hourly
  utilisation and exposure — the book the hour leaves behind, after its settlements and its opens,
  the positions outstanding until the next hour, the closest hourly analog of the engine's
  after-command re-evaluation — on the same 2,000 paths as the floor study, at three floor
  families: the floors where each market's refusals end (`house_cash_1pct` in the files: $274M,
  the house-cash measure's binding row, which at 100:1 no longer meets 1%), a uniform $10M, and a
  uniform $1M, the floors both measures now choose at 1%. Level 2 stays at 9500: it is the last
  widening before refusals start. Level 1 is the lowest candidate whose flow-weighted fire rate
  stays under 2% on the first two families and under 5% on any market with at least 1% of the
  flow. At 10:1 that was 7000, firing 1.4–1.5% flow-weighted (worst market 2.9%). At 100:1 no
  candidate passes, because BTC-USD and ETH-USD fill their caps far more often. The fire rates,
  flow-weighted with BTC-USD's beside them (at 10:1 the three families fired 1.5%, 1.4% and 35.4%
  at 7000, BTC-USD 2.9%, 2.9% and 55.9%):

  | level bps | refusal-ending floors | uniform $10M | uniform $1M |
  |---|---|---|---|
  | 5000 | 27.6% (46.6%) | 45.0% (71.6%) | 47.2% (68.7%) |
  | 6000 | 17.8% (31.9%) | 37.4% (62.5%) | 39.7% (59.9%) |
  | 7000, genesis | 10.1% (19.0%) | 29.4% (51.7%) | 32.1% (50.1%) |
  | 7500 | 7.2% (13.8%) | 25.3% (45.7%) | 28.1% (44.8%) |
  | 8000 | 4.8% (9.4%) | 21.2% (39.1%) | 24.2% (39.2%) |
  | 8500 | 2.9% (5.8%) | 17.0% (32.1%) | 20.1% (33.3%) |
  | 9000 | 1.3% (2.6%) | 12.7% (24.4%) | 16.0% (27.0%) |
  | 9500, level 2 | 0.26% (0.5%) | 8.1% (16.0%) | 11.7% (20.2%) |

  On the refusal-ending floors alone 9000 would pass (1.3%, BTC-USD 2.6%), 500 bps under level 2,
  but those floors fail the 1% target and a uniform $10M fires at 12.7% there. Under $1M floors a
  market's cap is so small that every level fires constantly — a property of those floors,
  reported rather than calibrated to — and those are now the recommended floors, so at them the
  utilisation trigger is not a rare event. The per-fire exposure at 7000 on BTC-USD at the
  refusal-ending floors is $25.6M, 81% of its $31.6M cap, against a p99 next-mark gap of 180 bps
  (`gap_tables`), a $461k hole. The model nets each hour's orders against the net-exposure cap and
  fills offsetting flow in full (its documented approximation, README above), so after one-sided
  settlement a book can briefly exceed the cap — the engine, checking each order, refuses more.
  The exposure-at-fire rows of `stress_utilisation.csv` inherit that; the level choice rests on
  the fire rates, which the artifact touches only in a handful of hours per market.

`stress_choice.json` records level 1 as null when no candidate passes, with the fire rates at the
highest candidate below level 2. The genesis values are the 10:1 calibration; the 100:1 stage
leaves them in place and proposes none (#500: the owner decides first):

| parameter | genesis (10:1 calibration) | calibrated at 100:1 |
|---|---|---|
| `stress_move_window_ms` | 60000 | 60000 |
| move bps, level 1 / 2 | 75 / 200 | 75 / 200 |
| divergence share bps, level 1 / 2 | 6001 / 9500 | 6001 / 9500 |
| utilisation bps, level 1 / 2 | 7000 / 9500 | none / 9500 |

Provenance: `run.py stress` on the committed `data/derived/`, 2,000 paths in batches of 25, seed
1, three workers, about 16 minutes (`results/manifest_stress.json`, which records the stage's own
settings: the effective scenario, the move-table hours, and the floor families); the move tables
sample 400 history hours per market at the same seed. The same seed and batch sizes reproduce the
run exactly.

## Toxicity parameters

The launch toxicity parameters (`config/production-genesis.toml`, #114) are dev values. The
`toxicity` stage calibrates them — `prior`, `memory`, the tier thresholds and the tier spreads —
from the observed fill sizes and the markets' markout volatility, so the score separates toxic
flow quickly without drifting on large fills. `run.py toxicity` writes
`results/toxicity_markets.csv` (the per-market distributions and drift), `toxicity_choice.json`
(the values and method), and `manifest_toxicity.json`. It runs no Monte Carlo over price paths:
the engine's notional-weighted recurrence is exact, and the one probability bound is checked by
40,000-trial simulations of it (about 5 seconds).

- **Fill sizes** (`size_quantiles`): papertrade's margin-weighted distribution of position
  notionals on each market's margin schedule, inverted at the median, p75, p90, and p99. A
  position opens as one order, so a fill is a position's notional. The clips vary by schedule:
  $100k median (and $3.2M p99) on the crypto and share schedules, $1.3M median ($10M p99) on
  FX, $1M median ($10M p99) on oil.
- **Markout scales** (`sigma_markout_bps`): the median open hour's Parkinson volatility — the
  model's per-hour scale — scaled to the 2s horizon, in bps. A typical hour prices a typical
  fill: 0.13–0.50 bps on FX and indices, 0.70–1.33 on crypto, up to 1.58 on the noisiest shares
  (`results/toxicity_markets.csv` has every market).
- **Memory**: the bounded-memory score is a weighted average of the last `memory` USD of fills,
  so its sampling drift for `n`-sized fills is about `sigma_markout * sqrt(n / memory)` bps —
  the issue's drift analysis: under 1 bp at the observed sizes unless single fills exceed about
  $5M at the launch memory. The chosen memory is the smallest that keeps the drift under 1 bp at
  every market's p99 fill: $11.26M, set by Brent (a $10M p99 clip at a 1.06 bps markout scale).
- **Prior**: half of memory, enough neutral history that a new account's first fills move its
  score only modestly while the toxic climb still lands well inside the candidate fill counts.
  The launch's $10k lets a single $1.3M fill set the score; the calibrated prior caps a first
  fill's weight at 1.33/(5.63+1.33) ≈ 19% on the biggest median clip.
- **Tiers and spreads** (`choose_tiers`): the design toxic level is a sustained 2 bps per-fill
  markout — above the widest size spread, so an account at it scalps the house at any size.
  Tier 1 is the highest whole-bps threshold that (a) a toxic account filling at the $100k median
  clip reaches within the stated count of fills and (b) a benign account filling at a market's
  median clip, at least a horizon (2s) between fills so markouts do not overlap, stays below it
  on each market for 200 fills with probability at least 99.9% (40,000 trials of the exact
  recurrence). The first passing count wins at 75 fills, so a toxic account reaches tier 1
  within 75 median-size fills; tier 2 doubles the threshold. A tier's spread is the sustained
  markout that reaches the tier in the stated fills (the house recoups the marginal toxic
  account's edge), rounded up to the whole bps the engine stores: the calibrated 1.8 bps becomes
  a 2-bps spread, 2.6 becomes 3.

  Fills closer than the horizon share price moves, and the bound does not cover them: the burst
  sensitivity in `results/toxicity_choice.json` reports the crossing probability for a 25-fill
  burst at one- and half-second cadences — about 0.5% and 3.9% on the noisiest median clip
  (Brent) at the chosen tier. The direction is stated, not hidden, and tighter cadences make
  crossings more likely: a market-making account firing $1M clips every half-second will ping
  the tier in about one burst in twenty-five.

The median clip, not the tail, anchors the benign bound deliberately: an account filling at the
p99 clip on every fill is already an outlier — the memory bound prices its noise at exactly the
drift bound, so no threshold within the toxic reach could stand against it. Such accounts are
the score's purpose, not its false positives. Two assumptions lean the same way and are stated
rather than hidden: the trials price markouts at the median hour's volatility, while live fills
cluster in livelier hours (the direction raises crossings); and the per-market bound composes
across markets only by a union bound, so a single account trading many markets at once is
bounded market by market, not jointly. Revisit both as live fill data arrives (below).

The values proposed to `config/production-genesis.toml` (`results/toxicity_choice.json`):

| parameter | launch | calibrated |
|---|---|---|
| `prior` USD | 10,000 | 5,629,518 |
| `memory` USD | 10,000,000 | 11,259,036 |
| tier 1, min score bps / spread bps | 5 / 1 | 1 / 2 |
| tier 2, min score bps / spread bps | 15 / 3 | 2 / 3 |

The launch tiers sit above what a sustainedly-toxic account can reach: at a 2-bps markout the
score asymptotes below 5 bps at any fill size, so the launch configuration detects no toxic
account within any number of fills. The calibrated tiers detect the design level within 75
median-size fills.

Provenance: `run.py toxicity` on the committed `data/derived/`, seed 1, about 5 seconds
(`results/manifest_toxicity.json`). The benign trials are seeded and deterministic at a fixed
trial count.

## Retained NAV

The `retained` stage sizes the staking liquidity floor: how much NAV the house must hold back from
sweeping so that the vault always covers client balances. It reuses the floor study's data and
simulation with a different scenario (`model/retained.py`): the house opens at a candidate floor as
its capital, sweeps everything above it each hour (worst case; the engine's own deductions only
retain more), and trades under the genesis cap fractions of that NAV, with the planned BTC and ETH
margin bands (1% to $2M, 2% to $10M, 5% above, #520). Cloned markets extend the launch set to 50–500
markets: equity templates at correlation 0.7 with their template, each as busy as it, since the
launch set alone cannot say how the answer scales. `run.py retained` writes
`results/retained_nav.csv`, `results/retained_stability.csv`, and `manifest_retained.json`; it is
its own stage, not part of `all`.

Retained NAV is the lowest grid floor (geometric, ratio 1.6, $4M to $4.6B) that refuses at most 1%
of directional notional and keeps house cash below zero in at most 1% of paths, at it and at every
higher floor:

| Markets | Paths | Retained NAV | Cash<0 | Refused, 90 days | Fees mean |
|---|---|---|---|---|---|
| 37 (launch) | 4000 | $107M | 0 | 0.49% | $74.3M |
| 100 | 1000 | $107M | 0 | 0.49% | $78M |
| 500 | 200 | $107M | 0 | 0.43% | $101M |

Stable in the path count (the 37-market answer is $107M on the first 500, 1,000, 2,000 and 4,000
paths) and in nearly every sensitivity (correlation 0.3–1, volatility x2, flow x0.25–x4, one
client direction, sweep epoch, no farming). What moves it:

- **The genesis cap fractions dominate.** With 10% BTC/ETH margin (the genesis bands) instead of
  the planned 1–5%, retained NAV falls to $26.2M at every count to 300 markets ($41.9M at 500) —
  high margin lets a small NAV carry little risk. The previous bands (1% to $1M, 20% above) give
  the same $107M as the planned ones.
- **Flow per clone is the scaling assumption.** The answer is flat in the market count because
  crypto carries about 76% of the flow and the clones are light equity markets. Cloning crypto
  instead (each clone as busy as BTC/ETH) reaches $440M retained at 100 markets and $2.88B at 500;
  mixed clones sit between ($172M at 200).
- **Fixed dollar caps per market are far weaker than NAV fractions.** With dollar caps of $10M a
  market in place of the NAV-share caps, retained NAV falls to $2M at 37 and 100 markets, and cash
  dips below zero in 0.60% of paths at the 37-market retained floor (12 of 2,000; upper bound
  0.97%) — a fixed cap does not grow with the house, so a small floor meets the same flow. $1M caps
  retain $1M and already measure 0.67% at 500 markets.

Reading for the reserve floor: under the planned bands, **$107M** — held as NAV, with sweeps only
of the excess. Under the genesis 10% bands, $26.2M would serve, but the bands themselves are the
decision that moves the number.

Limits:

- **The 100x crypto profile is approximated** from the existing oil margin schedule (sizes and
  notional per margin); `derive/papertrade.py` lists the planned and previous bands among its
  schedules, so a rebuild with the papertrade positions replaces the approximation.
- **Close-out gap bad debt is excluded.** The model fills crypto close-outs at the trigger in
  continuous trading, so these results say nothing about 100x safety; the trade-grid measure
  (crash windows, priced-update gaps, 100x against 50x) is in the 100:1 section.
- **The crypto series is a Binance spot stand-in**; Coinbase/Kraken-only pricing is not re-derived.
- **Genesis starts at zero capital** (#436): a floor under opening NAV is swept by the first
  staker, so the retained floor must be at or above opening NAV. At zero capital nothing is swept
  until fees build to the floor — about 130 days at launch flow for $107M — carrying the early-dip
  risk the floor study shows at zero capital until then.

## Re-running

From a virtualenv outside the repo (Python 3.12 or newer):

```sh
python3 -m venv /tmp/cap-venv && /tmp/cap-venv/bin/pip install -r research/cap-calibration/requirements.txt
cd research/cap-calibration
/tmp/cap-venv/bin/python -I -m pytest tests     # the accounting tests
/tmp/cap-venv/bin/python -I run.py all          # per-market, joint, sensitivity, then results/report.md
```

`all` reruns the floor study from the committed `data/derived/` with the committed results'
settings: `DEFAULTS` in `run.py` sets each stage's paths and paths per batch, and `--workers`
defaults to 4. Stages run alone with `per-market`, `joint`, `sensitivity`, and `report`; the
stress-trigger calibration (above) runs with `stress` and the toxicity calibration (above) with
`toxicity`, and the liquidity-floor sizing (above) with `retained`, none part of `all`, under the same seeding and manifest conventions. Each batch of paths is
seeded from `--seed` (default 1) and its index, and its size shapes its random draws, so the same
seed and batch size reproduce a run exactly, and another batch size gives other, equally valid
paths. `results/manifest_<stage>.json` records the scenario, including both, the hashes of the
derived files it read, and the run time.

To rebuild the data: `fetch-papertrade` (resumes; `--to-block` pins the end), `fetch-markets`,
`derive-papertrade`, `derive-markets`. Yahoo serves hourly bars for the last 730 days only, so a
later rebuild needs a later window in `fetch/markets.py`.

To recalibrate as live data arrives:

- replace papertrade's flow with this engine's own: the public log gives every order, fill, and
  close, from which the same flow profile (margin by hold class and leverage, long share, hedged
  share) follows, and the market mix becomes the engine's own volume;
- move the price window forward and widen it as hourly history allows;
- set the MONKEY rate and half-life once #36 fixes them, and the benchmark rates from #33;
- rerun `all`, and change floors through the operator channel (#47);
- rerun `stress` after any floor, market, or bound change: the utilisation levels' fire rates
  move with the caps, and the move thresholds with each market's volatility.

## Files

- `run.py`: the entry point.
- `catalogue.py`: launch markets from the genesis, and the public series for each.
- `fetch/`: `papertrade.py` and `markets.py` pull the raw data into `data/raw/` (gitignored).
- `derive/`: `papertrade.py` decodes positions; `markets.py` builds the hourly dataset, statistics,
  mix, and benchmark rates, into `data/derived/`.
- `model/`: `accounting.py` (engine rules), `prices.py` (bootstrap), `flow.py` (behaviour),
  `simulate.py` (the hourly simulation), `experiments.py` (runs and measures), `stress.py` (the
  stress-trigger calibration), `toxicity.py` (the toxicity calibration), `retained.py` (the
  liquidity-floor sizing), `report.py`.
- `tests/`: the engine rules, the simulation's accounting, the data derivation, and the
  estimators, on hand cases and small synthetic inputs.
- `results/`: the stage outputs, `report.md`, and run manifests.
