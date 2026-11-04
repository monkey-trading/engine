"""Records the top of book and the trades of BTC, ETH and SOL against USD on Coinbase and Kraken.

The engine's crypto index is the midpoint of the two venues' book mids (`crates/core/src/price.rs`,
two fresh sources). Public trade histories go back years but carry no book, so this recording
measures, over the same window, how the two-venue book-mid index moves against the Coinbase trade
series the historical sample is built from (`derive/calibrate.py`).

Usage: python -I fetch/record_books.py <out dir> <seconds>

Writes one gzip file per venue and UTC hour, `<venue>-<YYYYmmddHH>.txt.gz`, closed at the hour, so
every finished hour is readable while the recording runs. Lines:

    B <symbol> <receive ns, wall clock> <venue ms> <best bid> <best ask>   top of book changed
    T <symbol> <receive ns, wall clock> <venue ms> <price> <size> <side>   a trade
    R <receive ns>                                                         connection (re)opened

Messages are untrusted: anything that does not parse as the documented shape is skipped and
counted, and a book that crosses (bid at or above ask) is not written.
"""

import asyncio
import gzip
import json
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

import websockets

SYMBOLS = {"BTC": ("BTC-USD", "BTC/USD"), "ETH": ("ETH-USD", "ETH/USD"), "SOL": ("SOL-USD", "SOL/USD")}
COINBASE_URL = "wss://advanced-trade-ws.coinbase.com"
KRAKEN_URL = "wss://ws.kraken.com/v2"
KRAKEN_DEPTH = 10
MAX_LEVELS = 200_000


class HourlyWriter:
    """Appends lines to one gzip file per UTC hour."""

    def __init__(self, out: Path, venue: str):
        self.out, self.venue = out, venue
        self.hour, self.fh = None, None
        self.skipped = 0

    def write(self, line: str):
        hour = datetime.now(timezone.utc).strftime("%Y%m%d%H")
        if hour != self.hour:
            if self.fh:
                self.fh.close()
            self.hour = hour
            self.fh = gzip.open(self.out / f"{self.venue}-{hour}.txt.gz", "at")
        self.fh.write(line)

    def close(self):
        if self.fh:
            self.fh.close()


def iso_ms(text: str) -> int:
    """Milliseconds since the epoch of an RFC 3339 UTC stamp with up to nanosecond digits."""
    head, _, frac = text.rstrip("Z").partition(".")
    base = datetime.strptime(head, "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    return int(base.timestamp()) * 1000 + int((frac + "000")[:3])


class Book:
    def __init__(self):
        self.bids, self.asks = {}, {}

    def top(self):
        if not self.bids or not self.asks:
            return None
        bid, ask = max(self.bids), min(self.asks)
        return (bid, ask) if bid < ask else None


async def coinbase(out: Path, stop: float):
    writer = HourlyWriter(out, "coinbase")
    product = {v[0]: k for k, v in SYMBOLS.items()}
    while time.time() < stop:
        books = {s: Book() for s in SYMBOLS}
        last_top = {}
        try:
            async with websockets.connect(COINBASE_URL, max_size=2**26, ping_interval=10) as ws:
                for channel in ("level2", "market_trades"):
                    await ws.send(json.dumps({"type": "subscribe", "product_ids": list(product), "channel": channel}))
                writer.write(f"R {time.time_ns()}\n")
                while time.time() < stop:
                    raw = await asyncio.wait_for(ws.recv(), timeout=30)
                    now = time.time_ns()
                    try:
                        msg = json.loads(raw)
                        channel = msg.get("channel")
                        for event in msg.get("events", []):
                            if channel == "l2_data":
                                sym = product[event["product_id"]]
                                book = books[sym]
                                if event["type"] == "snapshot":
                                    book.bids, book.asks = {}, {}
                                stamp = None
                                for u in event["updates"]:
                                    side = book.bids if u["side"] == "bid" else book.asks
                                    price, qty = float(u["price_level"]), float(u["new_quantity"])
                                    if qty > 0:
                                        side[price] = qty
                                    else:
                                        side.pop(price, None)
                                    stamp = u["event_time"]
                                if len(book.bids) > MAX_LEVELS or len(book.asks) > MAX_LEVELS:
                                    raise ValueError("book too deep")
                                top = book.top()
                                if top and top != last_top.get(sym) and stamp:
                                    last_top[sym] = top
                                    writer.write(f"B {sym} {now} {iso_ms(stamp)} {top[0]!r} {top[1]!r}\n")
                            elif channel == "market_trades":
                                for t in event.get("trades", []):
                                    sym = product[t["product_id"]]
                                    price, size = float(t["price"]), float(t["size"])
                                    writer.write(f"T {sym} {now} {iso_ms(t['time'])} {price!r} {size!r} {t['side'][0]}\n")
                    except (KeyError, ValueError, TypeError):
                        writer.skipped += 1
        except (OSError, asyncio.TimeoutError, websockets.WebSocketException):
            await asyncio.sleep(2)
    writer.close()
    return writer.skipped


async def kraken(out: Path, stop: float):
    writer = HourlyWriter(out, "kraken")
    symbol = {v[1]: k for k, v in SYMBOLS.items()}
    while time.time() < stop:
        books = {s: Book() for s in SYMBOLS}
        last_top = {}
        try:
            async with websockets.connect(KRAKEN_URL, max_size=2**24, ping_interval=10) as ws:
                await ws.send(json.dumps({"method": "subscribe", "params": {"channel": "book", "symbol": list(symbol), "depth": KRAKEN_DEPTH}}))
                await ws.send(json.dumps({"method": "subscribe", "params": {"channel": "trade", "symbol": list(symbol)}}))
                writer.write(f"R {time.time_ns()}\n")
                while time.time() < stop:
                    raw = await asyncio.wait_for(ws.recv(), timeout=30)
                    now = time.time_ns()
                    try:
                        msg = json.loads(raw)
                        channel = msg.get("channel")
                        if channel == "book":
                            for d in msg["data"]:
                                sym = symbol[d["symbol"]]
                                book = books[sym]
                                if msg["type"] == "snapshot":
                                    book.bids, book.asks = {}, {}
                                for side, levels in ((book.bids, d.get("bids", [])), (book.asks, d.get("asks", []))):
                                    for lv in levels:
                                        price, qty = float(lv["price"]), float(lv["qty"])
                                        if qty > 0:
                                            side[price] = qty
                                        else:
                                            side.pop(price, None)
                                # The subscription keeps the top ten a side: deeper levels drop out.
                                for side, best in ((book.bids, max), (book.asks, min)):
                                    while len(side) > KRAKEN_DEPTH:
                                        side.pop(min(side) if best is max else max(side))
                                top = book.top()
                                stamp = d.get("timestamp")
                                if top and top != last_top.get(sym) and stamp:
                                    last_top[sym] = top
                                    writer.write(f"B {sym} {now} {iso_ms(stamp)} {top[0]!r} {top[1]!r}\n")
                        elif channel == "trade":
                            for t in msg["data"]:
                                sym = symbol[t["symbol"]]
                                price, size = float(t["price"]), float(t["qty"])
                                writer.write(f"T {sym} {now} {iso_ms(t['timestamp'])} {price!r} {size!r} {t['side'][0]}\n")
                    except (KeyError, ValueError, TypeError):
                        writer.skipped += 1
        except (OSError, asyncio.TimeoutError, websockets.WebSocketException):
            await asyncio.sleep(2)
    writer.close()
    return writer.skipped


async def main(out: Path, seconds: float):
    out.mkdir(parents=True, exist_ok=True)
    stop = time.time() + seconds
    skipped = await asyncio.gather(coinbase(out, stop), kraken(out, stop))
    print(json.dumps({"coinbase_skipped": skipped[0], "kraken_skipped": skipped[1]}))


if __name__ == "__main__":
    asyncio.run(main(Path(sys.argv[1]), float(sys.argv[2])))
