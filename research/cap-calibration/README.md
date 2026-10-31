# Cap calibration

## Summary

Two measures of insolvency set very different floors, and the owner's choice between them decides
the launch floors. Over 90 days from a house with no capital, on 2,000 simulated paths, each cell
giving the measured share of paths with the one-sided 95% upper bound beside it:

| Target | NAV never below zero | House cash never below zero (the vault covers client balances) |
|---|---|---|
| 1% | $37M in total: $1M on every market (1.00%; 20 paths, bound <1.45%). Refuses 47% of launch-week opening notional, 20% over 90 days. | $240M in total: $10M on most markets, $17.8M on US500 and US100, $5.62M on ETH-USD and SOL-USD, $3.16M on every share (0.80%; 16 paths, bound <1.21%). Refuses 0.3% in the launch week, 0.1% over 90 days. |
| 0.1% | $0 in total: nothing opens. The best candidate with any floor, $100k on every market, already measures 0.20% (bound <0.46%). | $0 in total: nothing opens, at the same 0.20%. |
| 0.01% | $0 in total: nothing opens. | $0 in total: nothing opens. |

The 1% row for NAV is the uniform family: $1M on every market. The risk-level method below sets
each market's floor from the market run alone, where a small market's own fees cannot absorb its
gap losses; alone, no market stays at or below 0.3% at any floor, so at those levels the family
puts $0 on every market and nothing opens. Its 1% choice is $49.4k on two markets, refusing 97% of
launch-week notional; with every market at $1M, pooled crypto fees carry the small markets, the
same target measures 1.00%, and it refuses 47%. For 0.1% and 0.01%, every candidate with any floor
measures above the target, so the recommendation is $0. The house-cash 1% row is the binding
point: each market at the floor where its refusals end.

- **Early dips are common, shallow, and short.** With $10M on every market, NAV
  falls below zero at some hour in 18.75% of paths (375 of 2,000), at a median of 0.2 days after
  launch, before fees have built any capital. It falls below -$100k in 3.4% of paths and never
  below -$1M. House cash goes below zero in 0.85% (17 paths), no withdrawal ever waits in the #49
  queue, and NAV ends the 90 days at a median of $26.2M.
- **Order size, not exposure, is what small floors refuse.** An order larger than the cap is
  refused whatever flows around it. At 200:1, orders up to $1M carry only 45% of papertrade's
  margin, so a floor under a few million refuses most launch flow — 82% of launch-week notional at
  $100k a market, 47% at $1M — while $10M a market, papertrade's position cap, refuses 2%.
- **Along the risk-level allocations, the dip rises quickly with the floors.** From 0.55% at $49.4k
  in total (two markets with floors), it passes 1.80% at $7.78M and 7.35% at $98.1M, reaching
  19.7% at $240M, where every market sits at its refusal-ending floor. At 0.3% and below the
  family sets $0 on every market, since alone no market stays at or below the level.
- **Pooling carries the small markets.** Each market alone, at the floor where its refusals end,
  dips below zero in 39.5% (XAU-USD) to 73.0% (DE40) of paths, because its own fees are small next
  to its gap losses. Together at $10M a market the house earns a mean of $25.3M of fees over the 90
  days, and the crypto markets fund the others' risk.
- **The total floor does not bind.** At the chosen per-market floors, a total floor at 25% to 100%
  of their sum leaves the measured risk unchanged and moves the refused share by less than a
  hundredth of a point — at $1M a market ($37M sum) from $9.25M to $37M, and at the house-cash
  choice ($240M sum) from $60M to $240M.

The assumptions that move these numbers most, each at the house-cash 1% choice ($240M of floors),
on 1,000 paths (the baseline gives NAV below zero 18.0% and house cash below zero 1.2%):

| Assumption | NAV<0 ever | Cash<0 | NAV<-$100k | Bad debt mean |
|---|---|---|---|---|
| Baseline | 18.0% | 1.2% | 3.3% | $633k |
| No farming (no hedged flow) | 79.1% | 29.3% | 44.5% | $173k |
| One client direction for all markets | 27.6% | 4.4% | 7.9% | $633k |
| Farmers hold 10% of their pairs through every closure | 24.3% | 8.6% | 10.5% | $8M |
| Volatility x2 | 21.3% | 1.2% | 9.4% | $2M |
| Markets bootstrapped independently | 7.6% | 0.30% | 0.60% | $631k |
| Farming share x2 | 2.4% | 0 | 0 | $1.09M |
| Flow x0.25 / x4 | 18.5% / 4.7% | 1.2% / 0.60% | 0.20% / 1.20% | $158k / $2.52M |

- **Farming is what makes the house solvent early.** Hedged flow pays about two thirds of the fees.
  Without it, NAV dips below zero in 79% of paths and house cash in 29%.
- **Correlated client direction is among the largest market risks.** When clients lean the same way in
  every market, the house's net exposure adds up across markets instead of netting.
- **Pairs held through closures are a risk floors barely touch.** A pair adds no net exposure, so a
  cap limits it only through the size of its orders, while a gap past a leg's balance is bad debt:
  $8M over 90 days in this stress. Margin over closures, not floors, controls it.

Decisions for the owner:

1. **The measure and the target.** NAV below zero at any hour is strict: with no capital, every
   floor that lets the house trade risks an early dip. House cash below zero is what clients feel:
   the vault holding less than their balances. A depth tolerance, such as NAV never below -$100k,
   sits between them: $10M on every market gives 3.4% on 2,000 paths, the NAV measure's 1%
   recommendation keeps it at zero, and the house-cash measure's reaches it in 3.6%.
2. **Whether to seed the house.** The early dip happens before fees accrue. Seed capital, not
   floors, removes it.
3. **Leverage on gap-prone markets.** Bad debt from gaps past 100:1 and 200:1 margin (oil, silver,
   indices) and 10:1 earnings gaps on shares is untouched by floors.
4. **#111's last acceptance criterion.** It puts the chosen values into the production genesis.
   This study recommends values but changes no config.

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

`config/production-genesis.toml` lists 37 markets. `fetch/markets.py` pulls, for each:

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
buys $10M of a 10:1 market in one tier, but $5.5M of BTC-USD (10% to $1M, then 20%) and $3.3M of a
share (10% to $500k, 20% to $2M, then 50%).

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
| 0.01% to 0.3% | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 | $0 | $0 |
| 1% | $49.4k | 2 | 0.55% | 3.55% | 0 | 97.2% | 42.3% | $16.8M | $15.8M | $338k |
| 3% | $740k | 2 | 0.80% | 0.95% | 0 | 82.3% | 31.3% | $19.8M | $18.7M | $409k |
| 10% | $3.35M | 12 | 0.75% | 0.80% | 0 | 62.6% | 25.1% | $21.4M | $20.2M | $450k |
| 20% | $7.78M | 15 | 1.80% | 0.55% | 0 | 47.6% | 20.8% | $22.4M | $21.2M | $477k |
| 30% | $13.5M | 17 | 2.55% | 0.50% | 0 | 35.9% | 17.4% | $23.0M | $21.9M | $500k |
| 50% | $98.1M | 29 | 7.35% | 0.55% | 0.05% | 7.6% | 3.8% | $25.7M | $24.8M | $626k |
| binding | $240M | 37 | 19.7% | 0.80% | 3.60% | 0.3% | 0.1% | $26.2M | $25.3M | $632k |

Floors set for house cash below zero at some hour:

| Level | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 |
|---|---|---|---|---|---|---|---|---|
| 0 | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 0.01% to 3% | $0 | 0 | 0 | 0 | 0 | 100% | 100% | $0 |
| 10% | $6.31M | 12 | 11.75% | 1.45% | 0 | 57.2% | 22.2% | $22.2M |
| 20% | $63.2M | 20 | 14.1% | 0.65% | 0.95% | 6.5% | 2.9% | $25.9M |
| 30% | $176M | 26 | 19.75% | 0.80% | 3.70% | 0.9% | 0.3% | $26.2M |
| 50%, binding | $240M | 37 | 19.7% | 0.80% | 3.60% | 0.3% | 0.1% | $26.2M |

The same floor on every market:

| Floor | Total floor | Markets with a floor | NAV<0 ever | Cash<0 | NAV<-$100k | Refused, week 1 | Refused, 90 days | NAV day 90 p50 | Fees mean | Bad debt mean |
|---|---|---|---|---|---|---|---|---|---|---|
| $100k | $3.7M | 37 | 0.20% | 0.20% | 0 | 82.0% | 31.5% | $19.7M | $18.6M | $416k |
| $300k | $11.1M | 37 | 0.25% | 0.25% | 0 | 69.0% | 26.8% | $21M | $19.9M | $448k |
| $1M | $37M | 37 | 1.00% | 0.40% | 0 | 47.0% | 20.1% | $22.8M | $21.7M | $495k |
| $3M | $111M | 37 | 9.00% | 0.75% | 0.05% | 23.8% | 12.1% | $24.7M | $23.6M | $572k |
| $10M | $370M | 37 | 18.75% | 0.85% | 3.40% | 1.9% | 1.0% | $26.2M | $25.3M | $631k |

Each probability is the share of the 2,000 paths. The recommendation for a target is the
candidate whose measured share is at or below it that refuses the least; the one-sided 95% upper
bound beside each count is the uncertainty on the measurement, not the selection criterion. With
zero failing paths that bound is 0.15% on 2,000 paths (0.30% on the 1,000-path sensitivity runs),
so a pick measuring exactly at its target sits within sampling noise of it. Refused shares are of
opening notional, directional flow and churn.

## Limits

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
  percentile its firings select. The chosen thresholds are the most protective pair whose
  flow-weighted time at level 1 stays under 1% (level 2 under 0.1%) and every market's level-1
  share under 5%. At a 60s window the candidates measure:

  | (t1, t2) bps | time at L1, flow-weighted | worst market at L1 | median vol pct of L1 firings |
  |---|---|---|---|
  | (50, 150) | 1.77% | 8.3% | 0.86 |
  | (60, 150) | 1.00% | 5.1% | 0.85 |
  | **(75, 200)** | **0.43%** | **2.7%** | 0.87 |
  | (100, 250) | 0.12% | 1.2% | 0.93 |
  | (150, 300), launch | 0.009% | 0.3% | 0.97 |

  The window stays at 60s: 30s and 120s give parallel fire-rate curves (a longer window raises
  every reading alike), and 60s keeps the window's dozen buckets at one-per-6s against feeds that
  publish at least a mid a second. The choice, 75 and 200 bps, fires on the volatile crypto, oil,
  and share markets (roughly 0.06–1.1% of their time at level 1) and is silent on FX and indices
  (their protection is the divergence trigger and the stale-price halt); the launch 150/300 fires
  on none of them outside crashes.
- **Divergence** (shares, from the stale-fill cost model). A divergence reading of `d` bps means
  the venue mids sit `d` apart with the index mid between them; a fill at the index mid loses up
  to `d/2` bps if the mark snaps to the other venue (`d/4` expected, uniform). A share `s` of a
  market's bound `B` fires at `d = s·B/10000`. The chosen share is the lowest whose worst-case
  stale-fill loss at fire covers the level's minimum spread on the tightest-bound market
  (`d/2 ≥ min_spread`): level 1 needs `d ≥ 4` bps, level 2 `d ≥ 20`. That gives 8000 (level 1) and,
  capped at the highest share still short of the bound itself — at 10000 the market has already
  halted — 9500 (level 2). The launch 6000/8000 fires on the tightest markets below the
  stale-fill cost of the level-1 spread; the expected-cost rule (`d/4 ≥ min_spread`) is reported
  in `stress_divergence.csv` and is unreachable at any share under 10000 on the 5-bps markets.
- **Utilisation** (levels, from the Monte Carlo). The model records each market's hourly
  utilisation and exposure on the same 2,000 paths as the floor study, at three floor families:
  the house-cash 1% choice ($240M of floors, the study's recommendation), a uniform $10M, and a
  uniform $1M. Level 2 stays at 9500: it is the last widening before refusals start. Level 1 is
  the lowest candidate whose flow-weighted fire rate stays under 2% on the launch-relevant
  families and under 5% on any market with at least 1% of the flow — 6000, firing 1.8–2.0%
  flow-weighted (worst market 3.7%). The fire rates:

  | level bps | house-cash 1% | uniform $10M | uniform $1M |
  |---|---|---|---|
  | 5000 | 7.7% | 7.2% | 50.3% |
  | **6000 (L1)** | **2.0%** | **1.8%** | 39.4% |
  | 8000, launch L1 | 0.11% | 0.10% | 16.7% |
  | **9500 (L2)** | **0.00%** | **0.00%** | 2.4% |

  Under $1M floors a market's cap is so small that every level fires constantly — a property of
  those floors, reported rather than calibrated to; at the recommended floors the utilisation
  trigger is a rare event, as intended. The per-fire exposure at 6000 is the house near its cap
  ($5.7M mean on BTC-USD), against a p99 next-mark gap of 180 bps on BTC-USD (`gap_tables`). The
  model nets each hour's orders against the net-exposure cap and fills offsetting flow in full
  (its documented approximation, README above), so after one-sided settlement a book can briefly
  exceed the cap — the engine, checking each order, refuses more. The exposure-at-fire rows of
  `stress_utilisation.csv` inherit that; the level choice rests on the fire rates, which the
  artifact touches only in a handful of hours per market.

The values proposed to `config/production-genesis.toml` (`stress_choice.json`):

| parameter | launch | calibrated |
|---|---|---|
| `stress_move_window_ms` | 60000 | 60000 |
| move bps, level 1 / 2 | 150 / 300 | 75 / 200 |
| divergence share bps, level 1 / 2 | 6000 / 8000 | 8000 / 9500 |
| utilisation bps, level 1 / 2 | 8000 / 9500 | 6000 / 9500 |

Provenance: `run.py stress` on the committed `data/derived/`, 2,000 paths in batches of 25, seed
1, four workers, about 13 minutes (`results/manifest_stress.json`); the move tables sample 400
history hours per market at the same seed. The same seed and batch sizes reproduce the run
exactly.

## Re-running

From a virtualenv outside the repo (Python 3.12 or newer):

```sh
python3 -m venv /tmp/cap-venv && /tmp/cap-venv/bin/pip install -r research/cap-calibration/requirements.txt
cd research/cap-calibration
/tmp/cap-venv/bin/python -I -m pytest tests     # the accounting tests
/tmp/cap-venv/bin/python -I run.py all          # per-market, joint, sensitivity, then results/report.md
```

`all` reruns everything from the committed `data/derived/` with the committed results' settings:
`DEFAULTS` in `run.py` sets each stage's paths and paths per batch, and `--workers` defaults to 4.
Stages run alone with `per-market`, `joint`, `sensitivity`, and `report`; the stress-trigger
calibration (below) runs with `stress`, under the same seeding and manifest conventions. Each
batch of paths is
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
  stress-trigger calibration), `report.py`.
- `tests/`: the engine rules, the simulation's accounting, the data derivation, and the
  estimators, on hand cases and small synthetic inputs.
- `results/`: the stage outputs, `report.md`, and run manifests.
