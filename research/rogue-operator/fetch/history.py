"""Fetches the historical sample: hourly candles for the year, then every trade in the sampled hours.

Usage:
    python -I fetch/history.py candles <raw dir>
    python -I fetch/history.py trades <raw dir> <sample.json> [market or ""] [venue] [reverse]

`candles` pulls Coinbase Exchange hourly candles (`/products/<p>/candles`, 300 a request) for
BTC-USD, ETH-USD and SOL-USD from `START` to `END` and for the stress day, into
`<raw>/candles/<market>.json`. `derive/sample.py` stratifies the hours by them and writes the
sample. `trades` then pulls, for each sampled hour, every Coinbase Exchange trade
(`/products/<p>/trades`, paged back by trade id) and every Kraken trade (`/0/public/Trades`, paged
forward by time) from `LEAD_S` before the hour to `TAIL_S` after it, into
`<raw>/trades/<venue>/<market>/<hour>.jsonl.gz`, one `[ms, price, size]` a line. A file that
exists is kept, so an interrupted pull resumes.

Responses are untrusted: each trade is checked for a positive finite price and size and a time
inside the requested window, Coinbase pages for strictly decreasing ids, and Kraken pages for
non-decreasing times; a page that fails is retried, and a pull that keeps failing stops with the
error rather than writing a partial hour.
"""

import gzip
import json
import math
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

START = datetime(2025, 11, 1, tzinfo=timezone.utc)
END = datetime(2026, 11, 1, tzinfo=timezone.utc)
# 2025-10-10, the largest crypto liquidation day on record, outside the year: a stress case.
STRESS_DAY = datetime(2025, 10, 10, tzinfo=timezone.utc)
MARKETS = {"BTC-USD": "XBTUSD", "ETH-USD": "ETHUSD", "SOL-USD": "SOLUSD"}
COINBASE = "https://api.exchange.coinbase.com"
KRAKEN = "https://api.kraken.com/0/public/Trades"
LEAD_S = 180
TAIL_S = 120
AGENT = "monkeytrading-research/1.0"


def get(url: str, pause: float, tries: int = 8):
    for attempt in range(tries):
        time.sleep(pause)
        try:
            req = urllib.request.Request(url, headers={"User-Agent": AGENT, "Accept": "application/json"})
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.loads(resp.read())
        except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, ConnectionError) as err:
            wait = min(60, 2 ** attempt)
            print(f"retry {attempt + 1} in {wait}s: {url} ({err})", file=sys.stderr)
            time.sleep(wait)
    raise RuntimeError(f"giving up on {url}")


def finite_positive(x) -> float:
    v = float(x)
    if not math.isfinite(v) or v <= 0:
        raise ValueError(f"not a positive finite number: {x!r}")
    return v


def iso_ms(text: str) -> int:
    head, _, frac = text.rstrip("Z").partition(".")
    base = datetime.strptime(head, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    return int(base.timestamp()) * 1000 + int((frac + "000")[:3])


def candles(raw: Path):
    out = raw / "candles"
    out.mkdir(parents=True, exist_ok=True)
    for market in MARKETS:
        rows = {}
        for lo, hi in ((STRESS_DAY, STRESS_DAY + timedelta(days=1)), (START, END)):
            t = lo
            while t < hi:
                u = min(hi, t + timedelta(hours=300))
                q = urllib.parse.urlencode({"granularity": 3600, "start": t.isoformat(), "end": u.isoformat()})
                for c in get(f"{COINBASE}/products/{market}/candles?{q}", 0.4):
                    ts, low, high, open_, close, volume = c
                    if lo.timestamp() <= ts < hi.timestamp():
                        rows[int(ts)] = [int(ts), finite_positive(open_), finite_positive(high), finite_positive(low), finite_positive(close), float(volume)]
                t = u
        bad = [r for r in rows.values() if not r[3] <= min(r[1], r[4]) <= max(r[1], r[4]) <= r[2]]
        if bad:
            raise ValueError(f"{market}: {len(bad)} candles with low/high outside open/close")
        (out / f"{market}.json").write_text(json.dumps(sorted(rows.values())))
        print(f"{market}: {len(rows)} hourly candles")


def coinbase_trade(market: str, after: int | None, limit: int):
    q = {"limit": limit}
    if after is not None:
        q["after"] = after
    page = get(f"{COINBASE}/products/{market}/trades?{urllib.parse.urlencode(q)}", 0.26)
    return [(int(t["trade_id"]), iso_ms(t["time"]), finite_positive(t["price"]), finite_positive(t["size"])) for t in page]


# (ms, trade id) pairs seen so far per market, which bracket the next search.
KNOWN: dict[str, list[tuple[int, int]]] = {}


def coinbase_id_at(market: str, target_ms: int) -> int:
    """A trade id at or just after `target_ms`, by interpolation on id against time between the
    closest trades seen so far, bisecting every other step so a one-sided interpolation cannot
    stall."""
    known = KNOWN.setdefault(market, [])
    if not known:
        for tid, ms, _, _ in (coinbase_trade(market, None, 1)[0], coinbase_trade(market, 2, 1)[0]):
            known.append((ms, tid))
    below = [k for k in known if k[0] < target_ms]
    above = [k for k in known if k[0] >= target_ms]
    lo_ms, lo_id = max(below)
    hi_ms, hi_id = min(above)
    for step in range(80):
        if hi_id - lo_id <= 1000:
            return hi_id
        if step % 2:
            guess = (lo_id + hi_id) // 2
        else:
            guess = lo_id + int((target_ms - lo_ms) / max(1, hi_ms - lo_ms) * (hi_id - lo_id))
        guess = min(hi_id - 1, max(lo_id + 1, guess))
        page = coinbase_trade(market, guess + 1, 1)
        if not page:
            lo_id = guess
            continue
        tid, ms = page[0][0], page[0][1]
        known.append((ms, tid))
        if ms < target_ms:
            lo_id, lo_ms = tid, ms
        else:
            hi_id, hi_ms = tid, ms
    raise RuntimeError(f"{market}: no trade id found near {target_ms}")


def coinbase_hour(market: str, t0_ms: int, t1_ms: int):
    after = coinbase_id_at(market, t1_ms) + 1
    rows, last_id = [], None
    while True:
        page = coinbase_trade(market, after, 1000)
        if not page:
            break
        for tid, ms, price, size in page:
            if last_id is not None and tid >= last_id:
                raise ValueError(f"{market}: trade ids not decreasing at {tid}")
            last_id = tid
            if t0_ms <= ms < t1_ms:
                rows.append((ms, price, size))
        KNOWN.setdefault(market, []).append((page[-1][1], page[-1][0]))
        if page[-1][1] < t0_ms:
            break
        after = page[-1][0]
    rows.reverse()
    return rows


def kraken_hour(pair: str, t0_ms: int, t1_ms: int):
    since = t0_ms * 1_000_000
    rows = []
    while True:
        body = get(f"{KRAKEN}?{urllib.parse.urlencode({'pair': pair, 'since': since, 'count': 1000})}", 1.1)
        if body.get("error"):
            raise RuntimeError(f"kraken {pair}: {body['error']}")
        result = body["result"]
        key = next(k for k in result if k != "last")
        page = result[key]
        last = int(result["last"])
        prev = rows[-1][0] if rows else 0
        for price, size, ts, *_ in page:
            ms = int(round(float(ts) * 1000))
            if ms < prev:
                raise ValueError(f"kraken {pair}: times decrease at {ms}")
            prev = ms
            if t0_ms <= ms < t1_ms:
                rows.append((ms, finite_positive(price), finite_positive(size)))
        if not page or last <= since or prev >= t1_ms:
            break
        since = last
    return rows


def write_rows(path: Path, rows):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(f".{os.getpid()}.tmp")
    with gzip.open(tmp, "wt") as fh:
        for r in rows:
            fh.write(json.dumps(list(r)) + "\n")
    tmp.rename(path)


def trades(raw: Path, sample_path: Path, only: str | None = None, venue_only: str | None = None, order: str | None = None):
    sample = json.loads(sample_path.read_text())
    hours = sample["hours"][::-1] if order == "reverse" else sample["hours"]
    for entry in hours:
        market, hour = entry["market"], int(entry["hour"])
        if only and market != only:
            continue
        t0_ms, t1_ms = (hour - LEAD_S) * 1000, (hour + 3600 + TAIL_S) * 1000
        name = datetime.fromtimestamp(hour, timezone.utc).strftime("%Y%m%dT%H")
        for venue, pull in (("coinbase", lambda: coinbase_hour(market, t0_ms, t1_ms)), ("kraken", lambda: kraken_hour(MARKETS[market], t0_ms, t1_ms))):
            path = raw / "trades" / venue / market / f"{name}.jsonl.gz"
            if path.exists() or (venue_only and venue != venue_only):
                continue
            rows = pull()
            write_rows(path, rows)
            print(f"{venue} {market} {name}: {len(rows)} trades", flush=True)


if __name__ == "__main__":
    command, raw = sys.argv[1], Path(sys.argv[2])
    if command == "candles":
        candles(raw)
    elif command == "trades":
        trades(raw, Path(sys.argv[3]), *(a or None for a in sys.argv[4:7]))
    else:
        raise SystemExit(__doc__)
