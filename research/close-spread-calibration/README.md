# Close-spread ceilings

The engine caps, for each market, the spread that closing a position can pay, whatever the operator
does to the trading settings. This cap is the market's close-spread ceiling. This study asks how
high each ceiling must be so that the trading server can charge for closing a position what the
market's pricing risk justifies, and nothing more.

The study proposes a ceiling for each of the 34 launch markets, and the launch genesis,
`config/production-genesis.toml`, carries every one of them. In basis points (bps), where one basis
point is one hundredth of one percent, the ceilings are 30 to 35 bps on crypto, 17 bps on FX, 18 to
19 bps on indices, 23 bps on commodities and 23 to 27 bps on shares. The pricing risk the study
measures sets three of them, those of BTC-USD, ETH-USD and SOL-USD. The other 31 sit at their floor,
which is the most the launch settings can charge for closing a position in the market's first spread
band. [results/report.md](results/report.md) gives every market's figures.

## Results

| Markets | Pricing-risk need | Ceiling | Set by | Share of the smallest initial margin |
| --- | ---: | ---: | --- | ---: |
| BTC-USD | 29.4 bps | 30 bps | Need | 30% |
| ETH-USD | 30.9 bps | 35 bps | Need | 35% |
| SOL-USD | 31.9 bps | 35 bps | Need | 3.5% |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD | 6.6 to 6.8 bps | 17 bps | Floor | 34% |
| US500, US30 | 12.7 to 12.8 bps | 18 bps | Floor | 36% |
| US100, JP225 | 13.2 bps | 19 bps | Floor | 38% |
| XAU-USD, XAG-USD, WTI | 13.5 to 16.8 bps | 23 bps | Floor | 23% |
| AMD, AVGO, LLY, NVDA, ORCL, TSLA | 15.8 to 18.3 bps | 27 bps | Floor | 2.7% |
| The other 14 shares | 13.7 to 16.1 bps | 23 bps | Floor | 2.3% |

The ceilings differ by asset class mainly because the launch price rules allow the venues' quotes to
differ by different amounts: 5 bps on FX, 10 bps on indices, commodities and shares, and 25 bps on
crypto. Volatility matters much less. Doubling the volatility input raises 3 of the 34 ceilings, by
at most 5 bps, and the floor absorbs the change in the other 31.

### Validation

The study fixes its method on the first 78 weeks of data and tests it on the last 24. A ceiling
fitted on the first 78 weeks alone covers the need in the last 24 weeks for every market. Over those
validation hours, the need exceeds that ceiling in at most 0.2% of hours, on BTC-USD. The proposed
ceiling is then fitted to the larger of the two samples' tails, so it is in-sample on the validation
tail.

### Cost to the trader

The study's objective keeps each ceiling within 30% of the market's smallest initial margin. The 30%
limit is an assumption of the study, not a rule of the engine. Every proposed ceiling is at most 38%
of that margin. ETH-USD exceeds the limit at 35%, because its need sets its ceiling. FX, at 34%, and
the indices, at 36% to 38%, exceed it because their floors do. A floor is the launch settings' own
charge for closing a first-band position at the top stress level. The engine refuses a ceiling below
that charge, so only lower launch spreads or stress minimums could bring those markets within the
limit.

### Price gaps

A price jump over a gap, a market closure or a futures roll is not a spread. Such a jump moves
margin and close-out risk, and `results/ceiling_effects.csv` shows that a spread on closing cannot
repair it. In a 5% gap on EUR-USD with no house capital, an uncapped close charge of 105 bps cuts
the house's shortfall by a fifth to a third, and costs the trader about six times as much. The
engine's stress levels already widen spreads during fast moves. A ceiling that moved with volatility
would be one the operator could adjust, so the study does not propose one.

### How the engine applies a ceiling

The engine applies the representation that [the proposal](ceiling-representation-proposal.md)
recommends:

- The engine keeps the ceilings in a registry of their own, apart from each market's listing, so
  relisting a market can neither replace nor raise its ceiling.
- When closing a position would cost more than the ceiling, the engine charges the ceiling instead,
  rounded down to the exact bound. The engine applies this clamp before it checks that the spread
  stays below the price, so closing a position always has a price.
- The engine refuses a trade that reverses a position when its blended spread, over the part that
  closes and the part that opens, exceeds the ceiling. Closing the position on its own stays
  possible.
- The engine refuses a ceiling below the market's launch close charge. After launch, a ceiling can
  only be lowered, by a signed adoption logged at least seven days before it takes effect.

The study priced a grid of reversals, with 6 position sizes and 4 opening fractions for each market
and stress level. The engine would refuse none of them at the calm level, 0.7% at the elevated level
and 34.8% at the top level. By asset class at the top level, it would refuse 21% on commodities, 7%
on crypto, 54% on shares, none on FX and 4% on indices. Closing the same positions outright exceeds
the ceiling, and is charged the ceiling instead, in none of the cases at the calm and elevated
levels and in 31.4% at the top level. The grid is not a distribution of real trades. If every
ceiling sat at its market's floor, the refused shares would be 0.0%, 0.7% and 37.0%.

A single ceiling for every market would not work. It would have to be at least 35 bps, which would
put 13 of the 34 markets above the 30% limit, and cost up to 70% of a market's margin. At the
strictest value, 17 bps, 30 markets would need more than they could charge.

The rounding of the clamp is small. At an index price of 0.65 units, one raw unit is 0.000015 bp. A
seeded search of 20,000 positions closed outright found at most 1 raw unit above the exact bound. At
the strictest ceiling, 17 bps, rounding down to the exact bound makes closing free only below an
index of 589 raw units. `rounding` in `results/ceilings.json` gives the detail.

## Method

### The need

The engine charges closing a position a stress minimum and the account's toxicity add-on, and the
size spread on the part that closes. A close-out does not pay the toxicity add-on. The stress
minimum and the add-on exist to cover one risk: that the reference price is stale, or off the true
price, at the moment a client closes. The launch price rules bound that risk for each market. A
venue's quote may be up to `max_age_ms` old, and the quotes from which the engine takes the median
may differ by up to `max_divergence_bps`. The study therefore sets:

```
need    = (max_divergence_bps + 1) + hourly tail move * sqrt(max_age_ms / 1 hour)
floor   = the market's launch close charge in tenths of a bp, rounded up to a whole bp
ceiling = need rounded up to a multiple of 5 bps, never below the floor
```

The divergence limit counts whole basis points. `compute_index` truncates the spread between the
venues' quotes before comparing it with `max_divergence_bps`, so a market limited to 10 bps admits
quotes 10.9 bps apart. The need therefore adds one basis point to the limit.

The tail move is the upper end of a weekly block-bootstrap interval, with 200 resamples, around the
calibration sample's hourly 99th percentile of the move within each bar, from the bar's open to its
close. The move from the previous bar's close to the bar's open carries session gaps, so the study
leaves it out and reports it separately. The study scales the hourly move to the one-second quote
age by the square root of time, which no hourly data can test. One second is 1/60 of an hour on that
scale, so the tail move adds little.

### The floor

The floor is the launch close charge, below which the engine refuses a ceiling. The charge is the
widest stress minimum (10 bps) and the top toxicity add-on (5 bps) in the launch genesis, plus the
market's first spread band at the widest size multiplier (4 times), in tenths of a basis point.
EUR-USD's first band of 0.4 bps adds 1.6 bps, for a charge of 16.6 bps and a floor of 17 bps. A
first band of 2 bps adds 8 bps, for a floor of 23 bps.

The engine compares `ceiling_bps * 10` with the charge in tenths of a basis point, so the floor is
the charge rounded up to a whole basis point. A ceiling at its floor is at least the exact charge of
closing a first-band position at the launch settings. When such a position is closed, the clamp
removes at most what the engine's upward rounding adds. Closing a larger position pays a higher
band's rate and can exceed the ceiling, which then clamps it.

### The split and its uncertainty

The study splits the data in time before any selection: the first 78 weeks, from 28 October 2024 to
27 April 2026, for calibration, and the last 24 weeks, to 12 October 2026, for validation. The
weekly bootstrap describes the sensitivity of the estimate to sampling, assuming each week is drawn
from the same distribution. It does not cover error in the proxy data, missing feeds or tails the
sample has not seen.

### The pricing model and its agreement with the engine

The study prices closing and reversing positions with two models in `execution.py`:

- `adjustment` models the pricing of engine revision `41a9ef4a52cb26ac592c5a0e372f610d8d1ba067`,
  which holds through protocol version V51. A pure reduction of a position pays no size spread. It
  pays the stress minimum plus the account's toxicity add-on, and a close-out does not pay the
  add-on. A reversal also pays the size spread on the part that opens, spread over the whole fill,
  with the stress multiplier on that part only. Bands are in whole basis points. The model leaves
  out the ceilings themselves. The rounding evidence uses this model.
- `adjustment_v52` models the pricing of protocol version V52, which the reversal grid uses. Bands
  are in tenths of a basis point. The part that closes a position pays through the same bands as the
  part that grows one, over the notional it closes, from the position's size down to what is left.
  Each band's charge rounds on its own. The costs of the closing and opening parts are summed,
  multiplied once by the stress level's size multiplier, and divided over the fill. The charge for
  closing a position outright is clamped to its ceiling before the check that the spread stays under
  the price, so closing a position always has a price. A reversal whose blended charge exceeds the
  ceiling is refused, even when that charge reaches the price.

Two fixture files compare the models with the engine's own fills, through `Engine::apply`:

- `fixtures/engine-v17.json` holds 13 vectors that compare `adjustment` with fills at protocol
  versions V17 and V21. They include reversals across bounded size-spread bands in US dollars and
  euros, a reduce-only clamp, quotes in euros and yen, an index of one raw unit, and a close-out gap
  that creates bad debt.
- `fixtures/engine-v52.json` holds 26 vectors that compare `adjustment_v52` with fills at protocol
  version V52, through the order path with account screening. A listed market's bands and stress
  settings change only after notice, so each vector sets its settings, listing and ceiling before
  V52 activates. The vectors cover partial closing of positions in every band at the calm, elevated
  and top stress levels, reversals within and over the ceiling, including one whose blended charge
  reaches the price, a reduce-only clamp, and positions closed outright whose unclamped charge
  reaches the price. They also cover close-outs with and without bad debt, quotes in euros and yen
  with bands below one basis point, bands listed in whole basis points before V52 activates, and
  adding to a position, which no ceiling bounds. The last vector closes at an index of so few raw
  units that the clamp rounds the charge down to zero.

The Python tests and `crates/core/tests/close_spread_calibration.rs` both read these files, which
are research fixtures kept apart from the protocol's reference logs. Charges round up at nine
decimal places, and a sell's adjustment stops one raw unit below its index. The integer
`Fill.spread_bps` truncates, so checking it alone can miss an excess of a fraction of a basis point.
A ceiling must therefore bound the raw adjustment.

### The gap sweep

`sweep.py` closes two opposite positions, already open in separate accounts, after one price jump
that no quote shows. It sweeps candidate close charges from 0 to 100 bps
(`results/conditional_gap_sweep.csv.gz`). It charges each gain the genesis's base profit fee.
`results/ceiling_effects.csv` gives, at each market's ceiling and with no cap, the trader's cost in
basis points, in dollars and as a share of margin, and the house's bad debt and cash shortfall in a
5% gap.

## Data and sources

- **Prices:** the hourly grid of the [cap calibration](../cap-calibration/README.md), from
  28 October 2024 to 12 October 2026 UTC, 102 whole weeks. The grid is a transformed proxy. It sets
  selected gaps between bars and presumed contract-roll gaps to zero, and folds the overnight moves
  of cash indices. The grid also holds DE40, UK100 and Brent, which the launch genesis does not
  list, and the study reads only the launch markets.
- **Crypto:** Binance's USDT spot markets, not a reference price from several venues in US dollars.
- **FX, indices, commodities and shares:** Yahoo Finance. Indices and commodities are priced from
  front-month futures, such as `ES=F` for US500, which here lack contract identifiers and a record
  of rolls and corporate actions.
- **The launch settings:** `config/production-genesis.toml`, read through a pinned inventory of its
  parameters and each market's margins, spreads and price rules. The inventory leaves out calendars,
  contract rolls and financing. `run.py` hashes its inputs, and refuses to run when the inventory or
  the hourly grid differs from its pinned digest. A change to a rule the study reads is therefore a
  visible change here.

Raw pulls are not committed. [The source audit](source-audit.md) lists the providers, dates, time
zones, checksums and licensing of each source, and what is missing.

`observations.py MANIFEST.json BODY.bin` audits one retained response without network access. Its
manifest records the exact HTTPS URL, the UTC start and end of the retrieval, the status, flags for
complete and truncated bodies, the byte count, the SHA-256 hash, the origin and an explicit provider
format. Trade timestamps from Coinbase and book timestamps from Kraken never fill a client's arrival
time. Data from Dukascopy also requires an explicit verified price scale, a UTC hour anchor and
decompression limits. An incomplete input produces no observations and exits with status 2. The
script never fills forward, suppresses a gap or salvages part of a batch. The original raw captures
and acquisition scripts are not committed.

## Limits

- The data has no simultaneous quotes from independent venues and no quote ages, so the study does
  not measure how stale or divergent the reference price is. The genesis limits stand in for them.
  That is the main missing evidence, and the reason no market's confidence is above moderate.
- Scaling hourly moves to one second by the square root of time is an assumption that hourly data
  cannot test.
- The sweep conditions on two existing positions, each margined on its own, and one price jump. It
  does not simulate order admission or margin shared across positions, and its counts of scenarios
  are not probabilities.
- The sweep charges every gain the genesis's base profit fee of 10%. The engine charges more on a
  position held for less than 15 minutes, and the sweep's positions are held across the gap for
  longer than that. A shorter hold pays more fee and leaves the house a smaller shortfall than the
  sweep shows. At volume tiers 2 and 3 the engine takes 10% and 20% off the base fee on a position
  held for 15 minutes or more, which leaves the house a slightly larger shortfall. The fee curve and
  the markout floors are in the pinned inventory, but no ceiling reads them.
- The study gives every market's first band the width it has at volume tier 2. Tiers 0 and 1 have a
  narrower first band, so more of their positions reach a higher band when they are closed. Tier
  3 has a wider first band, so fewer of its positions do. The count in `reversal_summary` of
  positions whose closing charge exceeds the ceiling is therefore tier 2's. No ceiling or floor
  depends on the band's width, because the floor uses only the first band's rate.
- The sweep margins each position at its market's normal margin. The engine requires at least a
  market's weekend margin over its weekend gap, so a gap across a weekend leaves the house less bad
  debt than the sweep shows.
- The house-loss trigger raises a market to stress level 2 earlier, and changes no level's spread,
  so the floor's widest stress minimum is unchanged. Each volume tier's margin floor raises the
  initial requirement for opening a position, and changes no spread on closing and no close-out
  level, so the sweep's positions read as before. Both are in the pinned inventory, and pinning them
  moved no result.
- No historical calibration guarantees future outcomes. A market with too thin a sample, or no
  bootstrap interval, would be blocked and would take no value from another market. No market is
  blocked in this sample.
- A change to a price rule, a margin band or the hourly grid makes the results stale until the study
  is re-run.

## Re-running

Use Python 3.12 or newer and the exact dependencies in `requirements.txt`, in a virtual environment
outside the repository. From the repository root, the whole run takes about ten seconds and needs no
network:

```sh
python -I research/close-spread-calibration/run.py        # inventory, hourly statistics, gap sweep
python -I research/close-spread-calibration/calibrate.py  # ceilings, reversals, rounding, effects
python -I research/close-spread-calibration/report.py     # report.md and figures
python -I -m unittest discover -s research/close-spread-calibration/tests
cargo test -p engine-core --test close_spread_calibration --locked
```

The simulation seed is fixed at 147, the outputs are deterministic, and a second run changes no
committed file. `run.py` pins the parameters and each market's margins, spreads and price rules in
`config/production-genesis.toml` (`INVENTORY_SHA256`) and refuses to run when they differ. A later
genesis change to those fields needs a re-run and a new pin; `calibrate.py` and `report.py` read the
committed statistics and reproduce the committed results.
