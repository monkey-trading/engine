# Close-spread calibration

Calibrates a close-spread ceiling for each of the 37 launch markets in `config/production-genesis.toml`
(#147), and recommends how #344 should represent them. Nothing here is adopted: the proposed values,
the objective behind them and the five representation choices in
[the proposal](ceiling-representation-proposal.md) wait for the owner. The per-market results are in
[results/report.md](results/report.md).

## Reproduce

Use Python 3.12 or newer and the exact dependencies in `requirements.txt`, in a virtual environment
outside the repository. From the repository root (about ten seconds in all, no network):

```sh
python -I research/close-spread-calibration/run.py        # inventory, hourly statistics, gap sweep
python -I research/close-spread-calibration/calibrate.py  # ceilings, reversals, rounding, effects
python -I research/close-spread-calibration/report.py     # report.md and figures
python -I -m unittest discover -s research/close-spread-calibration/tests
cargo test -p engine-core --test close_spread_calibration --locked
```

`run.py` hashes its inputs and refuses to run if the genesis inventory (its parameters and each market's
margins, spreads and price rules; not its calendars, contract rolls or financing) or the hourly grid differs from the
pinned digest, so a change to a rule this study reads is a visible change here. Simulation seeds are fixed
(147), the outputs are deterministic, and a second run changes no committed file.

| Output | Content |
|---|---|
| `results/ceilings.csv`, `ceilings.json` | one row per launch market: inputs, need, proposed ceiling, validation, flags, status; the registry-against-global comparison, reversal and rounding summaries |
| `results/market_statistics.json` | per-market hourly tails with weekly block-bootstrap intervals, input hashes, split |
| `results/reversal_blended_spread.csv` | blended spread of 2,664 reversal cells against each ceiling |
| `results/ceiling_effects.csv` | trader exit cost in bps, money and share of margin at the ceiling and under an uncapped operator change, with the house outcome in a 5% gap |
| `results/conditional_gap_sweep.csv.gz` | the wider candidate sweep (0 to 100 bps) |
| `results/report.md`, `*.png` | the report and figures |

## What a ceiling is

A ceiling is a fixed limit on operator power, set in genesis. It is not a forecast of price moves and not
the charged spread. The engine charges a close a stress minimum plus the account's toxicity add-on; a pure
close pays no size spread, and a liquidation waives toxicity. Those charges exist to cover one risk, the
index being stale or off the true price at the moment a client closes, and the genesis price rules bound
it per market: a quote may be `max_age_ms` old and the sources it medians may differ by
`max_divergence_bps`. So

```
need    = (max_divergence_bps + 1) + hourly tail move * sqrt(max_age_ms / 1 hour)
ceiling = need rounded up to a multiple of 5 bps, never below the launch close charge (13 bps)
```

The divergence limit counts whole bps: `compute_index` truncates the sources' spread before
comparing it with `max_divergence_bps`, so a market limited to 10 admits sources 10.9 bps apart
and the need counts the exclusive bound.

The tail move is the upper end of the weekly block-bootstrap interval of the calibration sample's hourly
p99 of the in-bar move (close against the bar's own open); the open against the previous close, which carries
session gaps, is left out and reported separately. DE40 and UK100 fold their overnight move into the bar, which
overstates their tail. Different asset classes differ because their divergence limits differ (FX 5, indices and commodities
10, shares 10, crypto 25 bps), and a tail move over one second is small: one second is 1/60 of an hour's
scale. The 13-bps floor is the stress-minimum 10 plus the top toxicity tier 3 in `production-genesis.toml`;
a ceiling below it would clamp a launch close at the top stress level.

Volatility enters only through that one-second tail, and a doubled volatility input raises 31 of 37 ceilings,
by at most 10 bps (the floor absorbs the four calm FX markets). A price jump over a gap, a closure or a contract roll is not a spread: it moves margin and
close-out risk. `results/ceiling_effects.csv` shows a close spread cannot repair it: in a 5% gap on EUR-USD
with no house capital, an uncapped 103-bp close charge cuts the house shortfall by a fifth to a third and
costs the trader eight times as much. A volatility-responsive charged spread is a different mechanism (the
existing stress levels already respond to moves); a ceiling that adjusted itself with volatility would be
operator-adjustable by construction and needs its own decision.

The objective, in the owner's terms: let the operator charge every close the market's reference risk can
justify, and nothing more; keep that within 30% of the market's smallest initial margin (an assumption, not
a decision). Its parameters are the floor, the step, the burden limit and the tail statistic.

## Split and uncertainty

The chronological split is the first 78 weeks (calibration) and the last 24 (validation), fixed before any
selection. The method is fixed on the calibration sample and tested on the validation sample: the
calibration-only ceiling covers the validation need for every market, and over validation hours the need
exceeds it in at most 0.9% of hours (MSFT; 0.9% or less for the proposed values). The proposed
value then refits on the larger of the two tails, which is in-sample on the validation tail. The weekly bootstrap (200 resamples) describes sampling
sensitivity under weekly stationarity; it does not cover proxy error, missing feeds or unseen tails.

## Engine model boundary

At engine revision `41a9ef4a52cb26ac592c5a0e372f610d8d1ba067`, a pure reduction has no size spread. It pays the
stress minimum plus the account's toxicity add-on; a liquidation waives the add-on. A reversal also pays
size spread on its opening slice, divided over the whole fill quantity, with the stress multiplier on that
slice only. `Engine::price` has no version gate other than the V10 upward rounding.

Thirteen vectors compare `execution.py` with actual `Engine::apply` fills at V17 and at V21 (the latest standalone harness without access screening; later versions
need access evidence on every order, which does not touch pricing), including reversals across bounded size-spread bands (USD and EUR), a reduce-only
clamp, EUR and JPY quotes, a one-raw-unit index and a liquidation gap that creates bad debt. They are research
fixtures, separate from the protocol reference logs, and the fixture file is read by both the Python tests and
`crates/core/tests/close_spread_calibration.rs`. Charges round upward at nine decimal places; a sell's adjustment
stops one raw unit below its index. The integer `Fill.spread_bps` truncates, so checking it alone can miss a
fractional-basis-point excess: a ceiling must bound the raw adjustment.

## Evidence and limitations

- The inherited hourly grid covers 2024-10-28 to 2026-10-12 UTC (102 whole weeks), a transformed proxy:
  it zeroes selected inter-bar and presumed roll gaps and folds cash-index overnight moves. DE40 and UK100
  are cash-index proxies for longer-session futures and carry low confidence.
- Crypto uses Binance USDT spot, not a multi-venue USD reference. The data has no simultaneous independent
  feeds or quote ages, so reference staleness and divergence are not measured: the genesis limits stand in
  for them. That is the main missing evidence, and why no market has more than moderate confidence.
- Yahoo front-month futures lack contract identifiers and roll or corporate-action audit trails here; raw
  pulls are not committed and public availability is not a redistribution right.
  [The source audit](source-audit.md) lists providers, dates, time zones, checksums and licensing.
- The sweep conditions on two existing isolated positions and one price jump; it is not an admission or
  cross-margin simulation and scenario counts are not probabilities. Positions of other venues' holders are
  scenario inputs, not launch behaviour.
- No historical calibration guarantees future outcomes. A market with too thin a sample, or no bootstrap
  interval, is blocked and takes no value from another market; none is blocked in this sample.

## Retained observation audits

`observations.py MANIFEST.json BODY.bin` audits one retained response without network access. Its manifest
records the exact HTTPS URL, UTC retrieval start and end, status, complete and truncated flags, byte count,
SHA-256, origin and explicit provider format. Coinbase trade and Kraken level timestamps never populate client
arrival times. Dukascopy additionally requires an explicit verified price scale, UTC hour anchor and
decompression limits. Incomplete inputs produce no observations and exit 2: no forward fill, gap suppression or
partial-batch salvage occurs. Original raw captures and acquisition scripts remain in the ignored evidence area
pending permitted-use decisions.

## Development fixtures and promotion

Development and Anvil configurations should carry deliberately unequal, clearly labelled test ceilings (in
`config/test-keys.toml`'s spirit: a fixture list, never reused as production values), so a scenario can show
a refused or clamped charge, a refused reversal and a refused listing without a calibrated number. Production
promotion needs the owner's adoption of the objective and each value, and re-running this pipeline on any
change to a price rule, margin tier or the hourly grid. No protocol version, reference log or production
config changes here.
