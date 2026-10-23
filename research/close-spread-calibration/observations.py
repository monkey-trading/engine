"""Validate retained response bytes; decode observations without estimating risk or arrival time."""

import argparse
import csv
import hashlib
import io
import json
import lzma
import math
import re
import struct
from datetime import datetime, timedelta, timezone
from decimal import Decimal, InvalidOperation
from pathlib import Path

UTC = timezone.utc
EPOCH = datetime(1970, 1, 1, tzinfo=UTC)
SECOND = 1_000_000_000
MAX_BODY_BYTES = 1_000_000  # The research task's response-body budget.


def require(condition, message):
    if not condition:
        raise ValueError(message)


def number(value, *, positive=False):
    require(not isinstance(value, bool) and isinstance(value, (str, int, Decimal)), "invalid decimal type")
    result = Decimal(value)
    require(result.is_finite() and (result > 0 if positive else result >= 0), "invalid decimal value")
    parts = result.as_tuple()
    require(len(parts.digits) <= 128 and abs(parts.exponent) <= 100, "decimal exceeds parser resource budget")
    return result


def integer(value):
    require(type(value) is int or isinstance(value, str) and re.fullmatch(r"[0-9]+", value), "invalid integer")
    result = int(value)
    require(result >= 0, "negative integer")
    return result


def utc_text(ns):
    seconds, fraction = divmod(ns, SECOND)
    return (EPOCH + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%S") + f".{fraction:09d}Z"


def rfc3339_ns(value):
    require(isinstance(value, str), "timestamp must be text")
    match = re.fullmatch(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.(\d{1,9}))?(Z|[+-]\d\d:\d\d)", value)
    require(match is not None, "timestamp requires an explicit RFC3339 timezone, at most nine fractional digits")
    base, fraction, offset = match.groups()
    require(offset != "-00:00", "unknown timezone offset")
    stamp = datetime.fromisoformat(base + ("+00:00" if offset == "Z" else offset)).astimezone(UTC)
    delta = stamp - EPOCH
    return (delta.days * 86400 + delta.seconds) * SECOND + int((fraction or "").ljust(9, "0"))


def strict_json(body):
    def pairs(items):
        out = {}
        for key, value in items:
            require(key not in out, "duplicate JSON key")
            out[key] = value
        return out

    def invalid_constant(value):
        raise ValueError("non-finite JSON number: " + value)

    return json.loads(body, parse_float=Decimal, parse_constant=invalid_constant, object_pairs_hook=pairs)


def binance(body, metadata):
    formats = {"binance-rest-ms": 1_000_000, "binance-archive-csv-ms": 1_000_000,
               "binance-archive-csv-us": 1_000}
    fmt = metadata["format"]
    multiplier = formats[fmt]
    rows = strict_json(body) if fmt == "binance-rest-ms" else list(csv.reader(io.StringIO(body.decode("utf-8"))))
    require(isinstance(rows, list) and rows, "missing Binance bars")
    require(metadata["symbol"] in {"BTCUSDT", "ETHUSDT", "SOLUSDT"}, "unsupported Binance symbol")
    out, previous = [], None
    for row in rows:
        require(isinstance(row, list) and len(row) == 12, "Binance bar requires twelve fields")
        opened, closed = integer(row[0]) * multiplier, integer(row[6]) * multiplier
        require(opened <= closed and (previous is None or opened > previous), "overlapping/unordered bar bounds")
        start, end = utc_text(opened), utc_text(closed)
        if fmt != "binance-rest-ms":
            require((fmt.endswith("-us")) == (start >= "2025-01-01"), "archive unit contradicts 2025 unit transition")
        o, h, low, c = [number(row[i], positive=True) for i in (1, 2, 3, 4)]
        require(low <= min(o, c) <= max(o, c) <= h, "inconsistent OHLC")
        volumes = [str(number(row[i])) for i in (5, 7, 9, 10)]
        out.append(dict(symbol=metadata["symbol"], bar_open_utc=start, bar_close_utc=end,
                        open=str(o), high=str(h), low=str(low), close=str(c),
                        base_volume=volumes[0], quote_volume=volumes[1], trade_count=integer(row[8]),
                        taker_buy_base_volume=volumes[2], taker_buy_quote_volume=volumes[3],
                        time_role="bar_bounds", client_arrival_utc=None))
        previous = closed
    return out


def coinbase(body, metadata):
    rows = strict_json(body)
    require(isinstance(rows, list) and rows, "missing Coinbase trades")
    require(metadata["symbol"] in {"BTC-USD", "ETH-USD", "SOL-USD"}, "unsupported Coinbase symbol")
    out, ids = [], set()
    for row in rows:
        require(isinstance(row, dict), "trade must be an object")
        trade_id = integer(row["trade_id"])
        require(trade_id not in ids, "duplicate trade ID")
        require(row["side"] in {"buy", "sell"}, "invalid maker side")
        ns = rfc3339_ns(row["time"])
        out.append(dict(symbol=metadata["symbol"], trade_id=trade_id, maker_side=row["side"],
                        price=str(number(row["price"], positive=True)), size=str(number(row["size"], positive=True)),
                        provider_time_raw=row["time"], provider_time_ns=ns, provider_time_utc=utc_text(ns),
                        time_role="trade_time", client_arrival_utc=None))
        ids.add(trade_id)
    return out


def kraken(body, metadata):
    data = strict_json(body)
    require(isinstance(data, dict) and data["error"] == [], "Kraken provider error")
    require(set(data["result"]) == {metadata["pair_key"]}, "Kraken pair key mismatch")
    book = data["result"][metadata["pair_key"]]
    out, prices = [], {}
    for side in ("bids", "asks"):
        rows = book[side]
        require(isinstance(rows, list) and rows, "missing book side")
        levels = []
        for row in rows:
            require(isinstance(row, list) and len(row) == 3, "book level requires price, quantity, Unix seconds")
            price, quantity = number(row[0], positive=True), number(row[1], positive=True)
            numerator, denominator = number(row[2]).as_integer_ratio()
            require(numerator * SECOND % denominator == 0, "book timestamp exceeds nanosecond precision")
            ns = numerator * SECOND // denominator
            levels.append(price)
            out.append(dict(pair_key=metadata["pair_key"], side=side, price=str(price), quantity=str(quantity),
                            provider_time_ns=int(ns), provider_time_utc=utc_text(int(ns)),
                            time_role="provider_level_time_semantics_unverified", client_arrival_utc=None))
        require(len(set(levels)) == len(levels), "duplicate book price level")
        prices[side] = levels
    require(max(prices["bids"]) <= min(prices["asks"]), "crossed book")
    return out


def dukascopy(body, metadata, limits):
    require(metadata["symbol"] in {"EURUSD", "GBPUSD", "USDJPY", "AUDUSD"}, "unsupported FX symbol")
    scale = metadata["price_scale"]
    require(scale["symbol"] == metadata["symbol"] and scale["verified"] is True
            and isinstance(scale["source"], str) and scale["source"].strip(), "missing verified pair-specific scale source")
    divisor = integer(scale["points_per_price_unit"])
    require(re.fullmatch(r"10*", str(divisor)), "price scale must be a positive power of ten")
    price_exponent = f"e{1 - len(str(divisor))}"
    anchor = rfc3339_ns(metadata["hour_anchor_utc"])
    require(anchor % (3600 * SECOND) == 0, "legacy anchor must be an exact UTC hour")
    require(limits is not None, "explicit decompression resource limits required")
    maximum = integer(limits["max_output_bytes"])
    memory = integer(limits["lzma_memlimit_bytes"])
    require(maximum > 0 and memory > 0, "invalid decompression limits")
    decoder = lzma.LZMADecompressor(format=lzma.FORMAT_ALONE, memlimit=memory)
    raw = decoder.decompress(body, max_length=maximum + 1)
    require(len(raw) <= maximum, "decompression output limit exceeded")
    require(decoder.eof and not decoder.unused_data, "truncated or trailing LZMA data")
    require(raw and len(raw) % 20 == 0, "tick payload requires complete twenty-byte records")
    out, previous = [], -1
    for ms, ask, bid, ask_volume, bid_volume in struct.iter_unpack(">IIIff", raw):
        require(previous <= ms < 3_600_000, "tick outside legacy hour or out of order")
        require(0 < bid <= ask, "invalid tick prices")
        require(all(math.isfinite(v) and v >= 0 for v in (ask_volume, bid_volume)), "invalid tick volume")
        ns = anchor + ms * 1_000_000
        out.append(dict(symbol=metadata["symbol"], offset_ms=ms, provider_time_ns=ns,
                        provider_time_utc=utc_text(ns), ask=str(Decimal(str(ask) + price_exponent)),
                        bid=str(Decimal(str(bid) + price_exponent)), ask_raw=ask, bid_raw=bid,
                        ask_volume=ask_volume, bid_volume=bid_volume, volume_unit="unverified",
                        time_role="legacy_hour_tick_time", client_arrival_utc=None))
        previous = ms
    return out


def audit(body, metadata, *, limits=None):
    """Return decoded or incomplete atomically; decoded means payload validity, not dataset coverage."""
    result = dict(status="incomplete", observations=[], manifest=metadata, computed_sha256=None,
                  client_arrival_utc=None, scope="payload_validation_only")
    try:
        require(body is not None, "missing response body")
        require(isinstance(body, bytes), "response must be original bytes")
        result["computed_sha256"] = hashlib.sha256(body).hexdigest()
        require(isinstance(metadata, dict), "missing manifest")
        require(metadata["http_status"] == 200, "HTTP response is not 200")
        require(metadata["response_complete"] is True, "incomplete response capture")
        require(metadata.get("truncated", False) is False, "truncated response capture")
        require(metadata.get("transport_error") is None, "transport error")
        require(type(metadata["byte_count"]) is int and metadata["byte_count"] == len(body), "response length mismatch")
        require(metadata["sha256"] == result["computed_sha256"], "response SHA256 mismatch")
        require(isinstance(metadata["url"], str) and metadata["url"].startswith("https://"), "missing HTTPS source URL")
        started = rfc3339_ns(metadata["retrieval_started_utc"])
        finished = rfc3339_ns(metadata["retrieval_finished_utc"])
        require(started <= finished, "retrieval timestamps reversed")
        require(metadata["data_origin"] in {"synthetic", "provider"}, "explicit data origin required")
        fmt = metadata["format"]
        if fmt in {"binance-rest-ms", "binance-archive-csv-ms", "binance-archive-csv-us"}:
            rows = binance(body, metadata)
        elif fmt == "coinbase-rest-trades":
            rows = coinbase(body, metadata)
        elif fmt == "kraken-rest-depth-seconds":
            rows = kraken(body, metadata)
        elif fmt == "dukascopy-legacy-hour-lzma-alone":
            rows = dukascopy(body, metadata, limits)
        else:
            raise ValueError("unsupported explicit format: " + str(fmt))
        result.update(status="decoded", observations=rows)
    except (ValueError, TypeError, KeyError, AttributeError, OverflowError, InvalidOperation, lzma.LZMAError) as error:
        result["reason"] = str(error)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("body", type=Path)
    parser.add_argument("--max-output-bytes", type=int)
    parser.add_argument("--lzma-memlimit-bytes", type=int)
    args = parser.parse_args()
    try:
        with args.manifest.open("rb") as stream:
            manifest_body = stream.read(MAX_BODY_BYTES + 1)
        require(len(manifest_body) <= MAX_BODY_BYTES, "manifest exceeds task budget")
        metadata = strict_json(manifest_body)
        try:
            with args.body.open("rb") as stream:
                body = stream.read(MAX_BODY_BYTES + 1)
            require(len(body) <= MAX_BODY_BYTES, "body exceeds task budget")
        except FileNotFoundError:
            body = None
        limits = dict(max_output_bytes=args.max_output_bytes, lzma_memlimit_bytes=args.lzma_memlimit_bytes)
        result = audit(body, metadata, limits=limits)
    except (OSError, ValueError) as error:
        result = dict(status="incomplete", reason=str(error), observations=[])
    print(json.dumps(result, default=str, allow_nan=False))
    return 0 if result["status"] == "decoded" else 2


if __name__ == "__main__":
    raise SystemExit(main())
