# Benchmark rate cap

## Summary

From `V46` a signed benchmark rate moves at most 300 bps per day between its date and the last applied
rate. This study replays each launch benchmark's whole published daily history through that rule and
lists every rate it would have refused: **185, all in AONIA, all from 1976 to 10 June 1988**. No rate
of SOFR, €STR, SONIA or TONA is refused, and no rate of AONIA since 10 June 1988.

The closest any benchmark has come since then is SOFR on 17 September 2019, the repo market spike:
282 bps of the 300 in one day, 94% of the cap, and it reversed 270 bps the next day.

| Benchmark | Observations | From | To | Refused | Closest move after 10 June 1988 |
| --- | ---: | --- | --- | ---: | --- |
| SOFR | 2,144 | 2018-04-02 | 2026-10-30 | 0 | 2019-09-16 2.43% to 2019-09-17 5.25%: 282 bps of 300 |
| €STR | 1,815 | 2019-10-01 | 2026-10-30 | 0 | 2022-09-13 -0.083% to 2022-09-14 0.662%: 74.5 bps of 300 |
| SONIA | 7,537 | 1997-01-02 | 2026-10-29 | 0 | 1998-12-23 7.109% to 1998-12-24 4.9611%: 214.79 bps of 300 |
| AONIA | 12,813 | 1976-04-07 | 2026-10-30 | 185 | 1988-06-15 12.875% to 1988-06-16 14.75%: 187.5 bps of 300 |
| TONA | 7,064 | 1998-01-05 | 2026-10-29 | 0 | 1998-03-30 0.39% to 1998-03-31 0.7%: 31 bps of 300 |

`results/refused.csv` lists each refused rate with the applied rate it is measured from, the gap, the
move and the limit, and `results/summary.md` holds these figures as the check writes them, with the
count of published moves described below. AONIA's
refusals by year: 1976 7, 1977 18, 1978 13, 1979 12, 1980 29, 1981 11, 1982 19, 1983 59, 1984 8,
1985 6, 1986 1, 1987 1, 1988 1. The largest published move among them is 66 percentage points in a
day (85% to 19%, 9 March 1983), and June 1985 holds two of about 15. All predate the Reserve Bank's
cash rate target, whose series in the same table starts on 2 August 1990, and the worker's endpoint for
AONIA, which starts in 2011, holds none of them.

Counting consecutive published moves past the cap instead, as if the last published rate had always
applied, gives 192 for AONIA and still none elsewhere. The replay count is smaller because a refused
rate does not move the rate the next is measured from.

## Method

The rule is the engine's (`engine_core::benchmark::split_by_cap`): rates in date order, each measured
from the last applied, with a limit of 300 bps times the calendar days between their dates. A refused
rate changes nothing, so the next is measured from the same applied rate over a wider gap. The first
observation of each history is a benchmark's first rate, bound only by the ±100% range, which every
published rate is inside.

`crates/core/tests/benchmark_cap_history.rs` replays `fixtures/` through the engine's own function and
requires exactly the rates `results/refused.csv` lists, so the study's arithmetic and the engine's must
agree, and a change to the cap shows as moves to report.

## Sources

Each is the publisher's own series, and the worker's configured endpoint
(`config/benchmark-publishers.toml`) unless noted. All five were reached on 2 November 2026.

- SOFR: Federal Reserve Bank of New York, `markets.newyorkfed.org/api/rates/secured/sofr/search.csv`.
  Published from 2 April 2018.
- €STR: European Central Bank, `data-api.ecb.europa.eu/service/data/EST/B.EU000A2X2A25.WT`. Published
  from 1 October 2019.
- SONIA: Bank of England, series `IUDSOIA` in its statistical database. From 2 January 1997.
- AONIA: Reserve Bank of Australia, series `FIRMMCRID`: table F1 (`f1-data.csv`, the worker's endpoint,
  from 4 January 2011), and before it the daily history workbook (`xls-hist/f01dhist.xls`, 7 April 1976
  to 31 December 2010), which the worker does not read.
- TONA: Bank of Japan, series `FM01'STRDCLUCON` (`fm01_d_1_en.csv`). From 5 January 1998.

Not checked, since none is this series as the worker reads it: SOFR's indicative values before its
launch, the ECB's pre-€STR estimates, and overnight call rates before 1998.

## Re-running

From a virtualenv outside the repo with `requirements.txt`, from this directory's parent:

```bash
python -I research/benchmark-cap/fetch.py    # data/raw/, gitignored; names any source it cannot reach
python -I research/benchmark-cap/derive.py   # fixtures/
python -I research/benchmark-cap/check.py    # results/
cargo test -p engine-core --test benchmark_cap_history --locked
```

A new fetch adds later observations, so `fixtures/`, `results/` and the test's expectations move
together. Fetched data is handled as data: nothing it holds is executed, and `derive.py` keeps only a
date and a decimal from each row.
