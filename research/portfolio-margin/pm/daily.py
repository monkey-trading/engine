"""Fetch and derive the daily history from 2016 for every launch market.

Fetching reuses the cap study's pulls (`fetch.markets`: Binance, Yahoo Finance, FRED) for the daily
series only; the raw responses stay in the gitignored `data/raw/`. Deriving reuses its
`derive.markets.daily_closes` (FRED's official series where one exists, else the public series,
dividend-adjusted for shares) and writes `data/derived/daily_logclose.csv`: one row per date, one
column per market, the natural log of the close, blank where the market has no close that date.
Each market's daily return is the difference of its log closes between consecutive dates it has;
a pair's is taken over the dates both have.
"""

import csv
from datetime import UTC, datetime

import numpy as np

from pm.common import DERIVED, RAW, launch_symbols


def fred(series_id: str) -> None:
    """One FRED series from 2016, by curl: the cap study's urllib call times out against FRED's
    full-history graph endpoint, which `cosd` shortens. Stored as the cap study stores it."""
    import gzip
    import subprocess
    import time

    path = RAW / "fred" / f"{series_id}.csv.gz"
    if path.exists():
        return
    url = f"https://fred.stlouisfed.org/graph/fredgraph.csv?id={series_id}&cosd=2016-01-01"
    for attempt in range(6):
        out = subprocess.run(["curl", "-s", "-m", "60", "--fail", url], capture_output=True)
        if out.returncode == 0 and out.stdout.startswith((b"observation_date,", b"DATE,")):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(gzip.compress(out.stdout))
            return
        time.sleep(3 * (attempt + 1))
    raise RuntimeError(f"FRED {series_id}: no answer")


# FRED's S&P 500 series now holds one year; the same cash close from the public chart series.
SUBSTITUTE = {"US500": "^GSPC"}


def fetch() -> None:
    from catalogue import FRED_BENCHMARKS, FRED_CHECKS, SERIES  # noqa: F401
    from fetch import markets as f

    symbols = launch_symbols()
    for symbol in symbols:
        source, ticker, _ = SERIES[symbol]
        if source == "binance":
            f.binance(RAW, ticker, "1d", f.DAILY_START)
        else:
            f.yahoo(RAW, ticker, "1d", f.DAILY_START)
        if symbol in FRED_CHECKS:
            fred(FRED_CHECKS[symbol])
        if symbol in SUBSTITUTE:
            f.yahoo(RAW, SUBSTITUTE[symbol], "1d", f.DAILY_START)
        print(f"fetched {symbol}", flush=True)


def derive() -> None:
    from catalogue import SERIES
    from derive.markets import daily_closes

    symbols = launch_symbols()
    series, sources = {}, {}
    for symbol in symbols:
        source, ticker, _ = SERIES[symbol]
        if symbol in SUBSTITUTE:
            name, ts, close = daily_closes(RAW, "-", "yahoo", SUBSTITUTE[symbol])
        else:
            name, ts, close = daily_closes(RAW, symbol, source, ticker)
        days = [datetime.fromtimestamp(t, UTC).date().isoformat() for t in ts]
        series[symbol] = dict(zip(days, np.log(close)))  # a later row of the same date wins
        sources[symbol] = name
    dates = sorted({d for s in series.values() for d in s})
    DERIVED.mkdir(parents=True, exist_ok=True)
    with (DERIVED / "daily_logclose.csv").open("w", newline="") as fh:
        w = csv.writer(fh, lineterminator="\n")
        w.writerow(["date", *symbols])
        for d in dates:
            w.writerow([d, *[f"{series[s][d]:.6f}" if d in series[s] else "" for s in symbols]])
    with (DERIVED / "daily_sources.csv").open("w", newline="") as fh:
        w = csv.writer(fh, lineterminator="\n")
        w.writerow(["symbol", "source", "first", "last", "days"])
        for s in symbols:
            ds = sorted(series[s])
            w.writerow([s, sources[s], ds[0], ds[-1], len(ds)])


def load() -> tuple[list[str], list[str], np.ndarray]:
    """(dates, symbols, log closes) with NaN where a market has no close."""
    with (DERIVED / "daily_logclose.csv").open() as fh:
        rows = list(csv.reader(fh))
    symbols, body = rows[0][1:], rows[1:]
    x = np.array([[float(v) if v else np.nan for v in r[1:]] for r in body])
    return [r[0] for r in body], symbols, x


def returns(x: np.ndarray, cols: list[int]) -> np.ndarray:
    """Returns over the dates every column in `cols` has a close, as a (days, len(cols)) array."""
    sub = x[:, cols]
    sub = sub[np.isfinite(sub).all(axis=1)]
    return np.diff(sub, axis=0)


def returns_dated(dates: list[str], x: np.ndarray, cols: list[int]) -> tuple[list[str], np.ndarray]:
    ok = np.isfinite(x[:, cols]).all(axis=1)
    idx = np.flatnonzero(ok)
    return [dates[i] for i in idx[1:]], np.diff(x[idx][:, cols], axis=0)
