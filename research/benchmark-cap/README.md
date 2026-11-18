# Benchmark rate cap

A position held through the daily rollover pays or receives financing at a benchmark interest rate,
which the operator signs and sends to the engine. The engine refuses a signed rate that moves more
than 300 basis points (bps) a day from the last rate it applied. One basis point is one hundredth of
one percent. This study asks whether that cap would ever have refused a rate that a launch
benchmark's publisher actually published.

Across the whole published daily history of the five launch benchmarks, the cap refuses 185 rates.
All of them are rates of AONIA, the Australian overnight rate, published between 1976 and 10 June
1988. The cap refuses no rate of SOFR, €STR, SONIA or TONA, and no rate of AONIA published after
10 June 1988.

## Results

| Benchmark | Observations | From | To | Refused | Closest move after 10 June 1988 |
| --- | ---: | --- | --- | ---: | --- |
| SOFR | 2,144 | 2018-04-02 | 2026-10-30 | 0 | 2019-09-16 2.43% to 2019-09-17 5.25%: 282 bps of 300 |
| €STR | 1,815 | 2019-10-01 | 2026-10-30 | 0 | 2022-09-13 -0.083% to 2022-09-14 0.662%: 74.5 bps of 300 |
| SONIA | 7,537 | 1997-01-02 | 2026-10-29 | 0 | 1998-12-23 7.109% to 1998-12-24 4.9611%: 214.79 bps of 300 |
| AONIA | 12,813 | 1976-04-07 | 2026-10-30 | 185 | 1988-06-15 12.875% to 1988-06-16 14.75%: 187.5 bps of 300 |
| TONA | 7,064 | 1998-01-05 | 2026-10-29 | 0 | 1998-03-30 0.39% to 1998-03-31 0.7%: 31 bps of 300 |

Since 10 June 1988, the closest any benchmark has come to the cap is SOFR on 17 September 2019,
during the spike in the US repo market. SOFR rose 282 bps that day, which is 94% of the cap, and
fell 270 bps the next day.

The 185 refused AONIA rates fall in these years:

| Year | 1976 | 1977 | 1978 | 1979 | 1980 | 1981 | 1982 | 1983 | 1984 | 1985 | 1986 | 1987 | 1988 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Refused | 7 | 18 | 13 | 12 | 29 | 11 | 19 | 59 | 8 | 6 | 1 | 1 | 1 |

The largest published move among them is 66 percentage points in a day, from 85% to 19% on 9 March
1983. June 1985 holds two moves of about 15 percentage points each. Every refused rate predates the
Reserve Bank of Australia's cash rate target, whose series in the same table starts on 2 August
1990. The endpoint from which the operator's worker reads AONIA starts in 2011, so it holds none of
the refused rates.

A second count measures each published rate from the rate published before it, as if every published
rate had been applied. By that count, 192 AONIA moves exceed the cap, and no move of any other
benchmark does. The replay refuses fewer rates than this count, because a refused rate does not
change the rate from which the next one is measured.

`results/refused.csv` lists each refused rate with the applied rate it is measured from, the number
of days between them, the move and the limit. `results/summary.md` holds the table above as the
check writes it, together with the second count.

## Method

The study applies the engine's rule, `engine_core::benchmark::split_by_cap`. The rule takes each
benchmark's rates in date order and measures each rate from the last rate applied. The limit is
300 bps multiplied by the number of calendar days between the two rates' dates. A refused rate
changes nothing, so the next rate is measured from the same applied rate, over a wider gap. A
benchmark's first observation has no earlier rate, so only the engine's range of ±100% bounds it,
and every published rate lies inside that range.

The engine's test `crates/core/tests/benchmark_cap_history.rs` replays `fixtures/` through the
engine's own function. The test requires the engine to refuse exactly the rates that
`results/refused.csv` lists, so the study's arithmetic and the engine's must agree. If the cap or
the data changes, the test fails and shows the moves whose treatment changed.

## Data and sources

Each series is the publisher's own. Unless noted, it is also the endpoint that the operator's worker
reads, as configured in `config/benchmark-publishers.toml`. All five sources were reached on
2 November 2026.

- **SOFR:** Federal Reserve Bank of New York,
  `markets.newyorkfed.org/api/rates/secured/sofr/search.csv`. Published from 2 April 2018.
- **€STR:** European Central Bank, `data-api.ecb.europa.eu/service/data/EST/B.EU000A2X2A25.WT`.
  Published from 1 October 2019.
- **SONIA:** Bank of England, series `IUDSOIA` in its statistical database. From 2 January 1997.
- **AONIA:** Reserve Bank of Australia, series `FIRMMCRID`. From 4 January 2011 the series comes
  from table F1 (`f1-data.csv`), which is the worker's endpoint. Before that it comes from the daily
  history workbook (`xls-hist/f01dhist.xls`, 7 April 1976 to 31 December 2010), which the worker
  does not read.
- **TONA:** Bank of Japan, series `FM01'STRDCLUCON` (`fm01_d_1_en.csv`). From 5 January 1998.

## Limits

- A history with no refusal does not show that every future move stays within the cap. When a signed
  rate exceeds the cap, the engine refuses it, and the last applied rate keeps financing positions.
  Because the limit grows with the days since that rate, a lasting move is accepted once enough days
  have passed.
- Every refused rate comes from AONIA's daily history workbook, which the worker does not read. The
  study therefore shows the cap against the series, not against the endpoint the worker uses.
- The study does not check SOFR's indicative values before its launch, the European Central Bank's
  estimates before €STR, or the Japanese overnight call rates before 1998. None of them is the
  series that the worker reads.

## Re-running

Run these commands from a virtualenv outside the repository with `requirements.txt` installed, from
the repository root:

```bash
python -I research/benchmark-cap/fetch.py    # data/raw/, gitignored; names any source it cannot reach
python -I research/benchmark-cap/derive.py   # fixtures/
python -I research/benchmark-cap/check.py    # results/
cargo test -p engine-core --test benchmark_cap_history --locked
```

A new fetch adds later observations, so `fixtures/`, `results/` and the test's expectations change
together. The study handles fetched data as data. Nothing in it is executed, and `derive.py` keeps
only a date and a decimal from each row.
