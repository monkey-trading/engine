# Source audit

This audit distinguishes source access from evidence adequate to select a production ceiling.
No raw observations have been changed or redistributed by this study.

## Bounded acquisition results

The retained crypto/FX probe batch received 12,647 body bytes across 18 requests. Nine payloads
decoded: four five-second Binance bar sets, three Coinbase trade snapshots and two Kraken
depth-one books. Seven Dukascopy attempts failed during TLS establishment, TrueFX returned
only whitespace, and FRED returned a date-unbounded CSV prefix that exceeded its cap. These
are incomplete inputs, not zero volatility or closed sessions. The crypto observations are
not contemporaneous comparable quotes, and provider timestamps are not client arrival times.
See [the access manifest](results/source_access_crypto_fx.json) for UTC retrievals, original
body hashes and explicit outcomes. Original response bodies remain in the ignored audit area.

The independent equities/futures acquisition made 48 requests and retained 460,301 body bytes
plus 35,164 header bytes, below its 1 MB budget. It obtained three-day hourly provider samples
for all 20 shares and ten explicitly unqualified index/commodity proxies. A JPX CSV range
contained 19 dated large Nikkei futures settlement records; it was a prefix of a larger file,
and neither a full-file checksum nor execution timestamps were established. Limited split,
dividend and earnings records were also captured. These do not establish complete event history.
The original responses, including failures and prefixes, remain private and unchanged.
[Coverage](results/source_coverage_equities_futures.csv) and
[request metadata](results/source_requests_equities_futures.csv) preserve instrument aliases,
sample dates, timezones, byte counts and retained-body checksums without publishing quotes.

The new equity samples include zero-volume current quote records appended to historical bars;
one historical earnings request also returned an out-of-window current quote. Those records
remain flagged and must not enter historical returns. No gap cleaning or assumed split
adjustment was applied. Yahoo's current exchange-offset metadata does not determine historical
DST; use IANA timezone rules for each observation. None of these small samples establishes
historical execution-horizon reference divergence or genuine dated-contract reopening gaps.

The standalone `observations.py` auditor validates retained manifests and explicit formats,
including Binance's archive timestamp-unit transition. Dukascopy decoding requires a verified
pair-specific scale and a legacy-hour anchor; the missing actual tick data is not fabricated.
Decoded means a payload passed format checks, not that its history is fit for calibration.

| Input | Available | Appropriate use | Missing evidence |
|---|---|---|---|
| Pinned production genesis | Committed; the digest of the parameters and market fields this study reads is checked | Instrument IDs, parameters, margins, calendars, reference age/divergence limits | Actual production reference providers/contracts for each market |
| Exposure-cap study (#111) catalogue and transformation code | Committed, full SHA-256 checked | Trace source tickers and explain changes | Independent audit of presumed roll-gap exclusions |
| #111 hourly NPZ | Committed, full SHA-256 checked | Chronological coarse historical stress context | Unmodified gaps, quotes, source ages, independent simultaneous observations |
| Exposure-cap study (#111) raw pulls | Not committed | Re-fetch subject to access and terms; preserve each response | Original response hashes, retrieval metadata and contract identifiers |
| Binance spot kline REST | Minimal historical BTC request succeeded, HTTP 200 | Spot bar observations; potentially minute-scale supplementary validation | USD/USDT basis, other venues, sub-minute observations, redistribution clearance |
| Yahoo chart REST | Minimal historical AAPL request succeeded, HTTP 200 | Exploratory proxy cross-checks | Documented service guarantees, data licensing, contract rolls and corporate actions |

The minimal access probes on 23 October 2026 requested three BTCUSDT one-minute rows from
`2024-10-27T00:00:00Z` and AAPL daily observations for `2024-10-27` through `2024-10-29`.
They downloaded 506 and 1,304 bytes respectively. Their bodies were inspected only to establish
access, not persisted as calibration data or used in results. An access probe is not a data-quality
or licensing approval. The Yahoo terms page could not be retrieved (HTTP 999).

The inherited fetch code describes raw source windows from 28 October 2024 through
17 October 2026 for hourly bars and from 2016 for daily bars. This pipeline uses only the
committed 102-week grid, ending 12 October 2026. Its split is the first 78 weeks for calibration
and the final 24 weeks for validation; no random shuffle, forward filling, or extrapolation into
closed hours occurs. Hourly tails are marked as transformed proxy statistics in every output.

## Provider documentation

- [Binance market-data endpoint documentation](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/rest-api/market)
  documents `/api/v3/klines`. The inherited endpoint is
  `https://data-api.binance.vision/api/v3/klines`.
- The inherited Yahoo endpoint is `https://query1.finance.yahoo.com/v8/finance/chart/<ticker>`.
  A working public endpoint does not establish a licensed historical-data service.
- [Yahoo terms](https://legal.yahoo.com/us/en/yahoo/terms/otos/index.html) were unavailable at
  audit time. Do not describe redistribution rights as verified.
- [Coinbase market-data terms](https://www.coinbase.com/legal/market_data), checked on
  23 October 2026, restrict external distribution of data and derived research and certain
  benchmark uses without written consent. No such permission is established here.
- [TrueFX terms](https://www.truefx.com/truefx-terms-and-conditions/) describe internal analysis
  and prohibit redistribution of price content. The empty access probe supplies no observations.
- [Binance archive documentation](https://raw.githubusercontent.com/binance/binance-public-data/master/README.md)
  distinguishes the spot archive's microsecond timestamps from 2025 onward. REST and archive
  formats must be selected explicitly; no magnitude-based timestamp guessing is used.

## Data needed before numerical adoption

Crypto needs independent venue observations, source timestamps and USD conversion/basis.
FX needs synchronised independent spot references including weekend/holiday reopenings.
Indices and commodities need named futures contracts, dated roll rules and contract-level
history; cash indices cannot silently replace longer-session futures. Shares need split/dividend
and corporate-action records, regular-session opening observations and earnings/event stress.
All classes need enough execution-horizon observations to measure the reference's age and
divergence. No convenient volatility multiple supplies these missing observations.

Without that evidence the study keeps the genesis limits (`max_age_ms`, `max_divergence_bps`) as the
bound on reference error and labels every proposed ceiling moderate or low confidence; a market whose
statistics are missing or too thin is blocked and takes no value from another. Production adoption needs the
owner's objective and values after the results are reviewed.
