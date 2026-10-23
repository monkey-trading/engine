"""Synthetic fixtures only: no provider captures, network, or statistical estimates."""

import copy
import importlib.util
import hashlib
import json
import lzma
import struct
import subprocess
import sys
import tempfile
import unittest
from decimal import localcontext
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("audit", ROOT / "observations.py")
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)
LIMITS = {"max_output_bytes": 1024, "lzma_memlimit_bytes": 16 * 1024 * 1024}
BAR = [1735941540000, "100.00", "102.00", "99.00", "101.00", "2.50",
       1735941540999, "250.00", 5, "1.25", "125.00", "0"]
TRADE = {"trade_id": 7, "price": "100.01", "size": "0.001", "side": "sell",
         "time": "2025-01-03T21:59:00.123456789Z"}
BOOK = {"error": [], "result": {"XXBTZUSD": {
    "bids": [["100.00", "2.5", 1735941540]],
    "asks": [["100.01", "1.0", 1735941541]]}}}


def encoded(value):
    return json.dumps(value).encode()


def manifest(body, fmt="binance-rest-ms", **extra):
    return dict(url="https://synthetic.invalid/fixture", data_origin="synthetic",
                retrieval_started_utc="2026-10-23T22:00:00Z",
                retrieval_finished_utc="2026-10-23T22:00:01Z", http_status=200,
                response_complete=True, truncated=False, byte_count=len(body),
                sha256=hashlib.sha256(body).hexdigest(), format=fmt, **extra)


def fx_metadata(body, symbol="EURUSD"):
    return manifest(body, "dukascopy-legacy-hour-lzma-alone", symbol=symbol,
                    hour_anchor_utc="2025-01-03T21:00:00Z",
                    price_scale={"symbol": symbol, "points_per_price_unit": 100000,
                                 "verified": True, "source": "synthetic: explicitly assigned fixture scale"})


def ticks(records):
    return lzma.compress(b"".join(struct.pack(">IIIff", *row) for row in records), format=lzma.FORMAT_ALONE)


class AuditTests(unittest.TestCase):
    def test_decimal_exponents_and_digits_are_bounded_before_integer_ratio_expansion(self):
        for value in ("1e101", "1e-101", "9" * 129):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "resource budget"):
                audit.number(value)

    def incomplete(self, body, metadata, **kwargs):
        before = copy.deepcopy(metadata)
        result = audit.audit(body, metadata, **kwargs)
        self.assertEqual(result["status"], "incomplete", result)
        self.assertEqual(result["observations"], [])
        self.assertTrue(result["reason"])
        self.assertEqual(metadata, before)
        return result

    def test_binance_explicit_rest_ms_archive_us_and_pre2025_archive_ms(self):
        body = encoded([BAR])
        rest = audit.audit(body, manifest(body, symbol="BTCUSDT"))
        self.assertEqual(rest["status"], "decoded")
        self.assertEqual(rest["observations"][0]["bar_open_utc"], "2025-01-03T21:59:00.000000000Z")
        self.assertEqual(rest["observations"][0]["bar_close_utc"], "2025-01-03T21:59:00.999000000Z")
        self.assertEqual(rest["observations"][0]["quote_volume"], "250.00")
        self.assertEqual(rest["observations"][0]["time_role"], "bar_bounds")
        self.assertIsNone(rest["observations"][0]["client_arrival_utc"])
        row = BAR.copy()
        row[0] *= 1000
        row[6] = BAR[6] * 1000
        csv_body = (",".join(map(str, row)) + "\n").encode()
        archive = audit.audit(csv_body, manifest(csv_body, "binance-archive-csv-us", symbol="BTCUSDT"))
        self.assertEqual(archive["observations"], rest["observations"])
        row[6] += 999
        precise = (",".join(map(str, row)) + "\n").encode()
        result = audit.audit(precise, manifest(precise, "binance-archive-csv-us", symbol="BTCUSDT"))
        self.assertEqual(result["observations"][0]["bar_close_utc"], "2025-01-03T21:59:00.999999000Z")
        old = BAR.copy()
        old[0], old[6] = 1704290340000, 1704290340999
        old_body = (",".join(map(str, old)) + "\n").encode()
        result = audit.audit(old_body, manifest(old_body, "binance-archive-csv-ms", symbol="ETHUSDT"))
        self.assertEqual(result["observations"][0]["bar_open_utc"], "2024-01-03T13:59:00.000000000Z")
        self.incomplete(old_body, manifest(old_body, "binance-archive-csv-us", symbol="ETHUSDT"))
        new_ms = (",".join(map(str, BAR)) + "\n").encode()
        self.incomplete(new_ms, manifest(new_ms, "binance-archive-csv-ms", symbol="BTCUSDT"))

    def test_binance_rejects_schema_price_bounds_and_partial_batches(self):
        malformed = [[], [BAR[:-1]], [BAR, BAR], {"code": -1}, [None]]
        for index, value in [(0, True), (1, "NaN"), (2, "99"), (3, "102"),
                             (5, "-1"), (6, BAR[0] - 1), (8, 0.5)]:
            bad = BAR.copy()
            bad[0] += 1000
            bad[6] += 1000
            bad[index] = value
            malformed.append([BAR, bad])
        for rows in malformed:
            with self.subTest(rows=rows):
                body = encoded(rows)
                self.incomplete(body, manifest(body, symbol="SOLUSDT"))
        body = encoded([BAR])
        self.incomplete(body, manifest(body, "binance-auto", symbol="BTCUSDT"))

    def test_coinbase_preserves_nanoseconds_and_trade_time_not_arrival(self):
        body = encoded([TRADE])
        result = audit.audit(body, manifest(body, "coinbase-rest-trades", symbol="BTC-USD"))
        row = result["observations"][0]
        self.assertEqual(row["provider_time_ns"], 1735941540123456789)
        self.assertEqual(row["provider_time_raw"], TRADE["time"])
        self.assertEqual(row["provider_time_utc"], TRADE["time"])
        self.assertEqual(row["time_role"], "trade_time")
        self.assertEqual(row["maker_side"], "sell")
        self.assertIsNone(row["client_arrival_utc"])
        self.assertEqual(audit.rfc3339_ns("2025-01-03T22:59:00.123456789+01:00"), row["provider_time_ns"])

    def test_coinbase_rejects_ambiguous_times_bad_prices_and_duplicates(self):
        for key, value in [("time", "2025-01-03T21:59:00"), ("time", "2025-01-03T21:59:00-00:00"),
                           ("time", "2025-01-03T21:59:00.1234567890Z"), ("time", "2025-02-30T00:00:00Z"),
                           ("price", "Infinity"), ("size", "0"), ("side", "bid"), ("trade_id", True)]:
            with self.subTest(key=key, value=value):
                bad = dict(TRADE, trade_id=8)
                bad[key] = value
                body = encoded([TRADE, bad])
                self.incomplete(body, manifest(body, "coinbase-rest-trades", symbol="ETH-USD"))
        for body in [encoded([TRADE, TRADE]), b'[{"price":1,"price":2}]', b'[NaN]', b'[]', b'{']:
            self.incomplete(body, manifest(body, "coinbase-rest-trades", symbol="SOL-USD"))

    def test_kraken_preserves_level_timestamp_without_client_arrival(self):
        body = encoded(BOOK)
        body = body.replace(b"1735941540", b"1735941540.123456789")
        result = audit.audit(body, manifest(body, "kraken-rest-depth-seconds", pair_key="XXBTZUSD"))
        bid, ask = result["observations"]
        self.assertEqual(bid["provider_time_ns"], 1735941540123456789)
        self.assertEqual(bid["provider_time_utc"], "2025-01-03T21:59:00.123456789Z")
        self.assertEqual(ask["provider_time_ns"], 1735941541000000000)
        self.assertEqual(bid["quantity"], "2.5")
        self.assertEqual(bid["time_role"], "provider_level_time_semantics_unverified")
        self.assertIsNone(bid["client_arrival_utc"])
        with localcontext() as context:
            context.prec = 5
            precise = audit.audit(body, manifest(body, "kraken-rest-depth-seconds", pair_key="XXBTZUSD"))
        self.assertEqual(precise["observations"], result["observations"])

    def test_kraken_rejects_provider_errors_crossed_books_and_bad_levels(self):
        cases = []
        bad = copy.deepcopy(BOOK)
        bad["error"] = ["EGeneral:unavailable"]
        cases.append(bad)
        for rows in [[], [["99", "1", 1735941541]], [["101", "-1", 1735941541]],
                     [["101", "1"]], [["101", "1", "NaN"]],
                     [["101", "1", "1735941541.0000000001"]],
                     [["101", "1", "1735941541.12345678900000000000000001"]],
                     [["101", "1", 1735941541], ["101", "2", 1735941542]]]:
            bad = copy.deepcopy(BOOK)
            bad["result"]["XXBTZUSD"]["asks"] = rows
            cases.append(bad)
        for book in cases:
            body = encoded(book)
            self.incomplete(body, manifest(body, "kraken-rest-depth-seconds", pair_key="XXBTZUSD"))
        body = encoded(BOOK)
        self.incomplete(body, manifest(body, "kraken-rest-depth-seconds", pair_key="USDTZUSD"))

    def test_duka_legacy_hour_big_endian_scale_and_last_millisecond(self):
        body = ticks([(0, 110020, 110010, 1.5, 2.5), (1001, 110030, 110020, 1, 2),
                      (3599999, 110040, 110030, 0, 0)])
        result = audit.audit(body, fx_metadata(body), limits=LIMITS)
        first, middle, last = result["observations"]
        self.assertEqual(first["provider_time_utc"], "2025-01-03T21:00:00.000000000Z")
        self.assertEqual(middle["provider_time_utc"], "2025-01-03T21:00:01.001000000Z")
        self.assertEqual(last["provider_time_utc"], "2025-01-03T21:59:59.999000000Z")
        self.assertEqual((first["ask"], first["bid"]), ("1.10020", "1.10010"))
        self.assertEqual((first["ask_raw"], first["bid_raw"]), (110020, 110010))
        self.assertEqual(first["ask_volume"], 1.5)
        self.assertEqual(first["volume_unit"], "unverified")
        self.assertIsNone(first["client_arrival_utc"])
        jpy = ticks([(0, 150010, 150000, 1, 1)])
        metadata = fx_metadata(jpy, "USDJPY")
        metadata["price_scale"]["points_per_price_unit"] = 1000
        result = audit.audit(jpy, metadata, limits=LIMITS)
        self.assertEqual(result["observations"][0]["ask"], "150.010")
        with localcontext() as context:
            context.prec = 5
            precise = audit.audit(jpy, metadata, limits=LIMITS)
        self.assertEqual(precise["observations"], result["observations"])

    def test_duka_requires_pair_scale_evidence_and_rejects_daily_format(self):
        body = ticks([(0, 150010, 150000, 1, 1)])
        for key, value in [("verified", False), ("source", ""), ("symbol", "EURUSD"),
                           ("points_per_price_unit", 0), ("points_per_price_unit", 3)]:
            metadata = fx_metadata(body, "USDJPY")
            metadata["price_scale"][key] = value
            self.incomplete(body, metadata, limits=LIMITS)
        metadata = fx_metadata(body, "USDJPY")
        del metadata["price_scale"]
        self.incomplete(body, metadata, limits=LIMITS)
        metadata = fx_metadata(body)
        metadata["format"] = "dukascopy-daily"
        self.incomplete(body, metadata, limits=LIMITS)
        metadata = fx_metadata(body)
        metadata["hour_anchor_utc"] = "2025-01-03T21:00:01Z"
        self.incomplete(body, metadata, limits=LIMITS)

    def test_duka_rejects_truncation_trailing_data_bad_ticks_and_resource_limits(self):
        good = ticks([(0, 110020, 110010, 1, 1)])
        cases = [good[:-1], good + b"trailing", lzma.compress(b"x", format=lzma.FORMAT_ALONE),
                 lzma.compress(b"", format=lzma.FORMAT_ALONE),
                 lzma.compress(b"x" * 20, format=lzma.FORMAT_XZ)]
        for records in [[(3600000, 2, 1, 1, 1)], [(0, 0, 0, 1, 1)], [(0, 1, 2, 1, 1)],
                        [(1, 2, 1, 1, 1), (0, 2, 1, 1, 1)],
                        [(0, 2, 1, float("nan"), 1)], [(0, 2, 1, 1, float("inf"))],
                        [(0, 2, 1, -1, 1)]]:
            cases.append(ticks(records))
        for body in cases:
            self.incomplete(body, fx_metadata(body), limits=LIMITS)
        self.incomplete(good, fx_metadata(good))
        for changes in [{"max_output_bytes": 19}, {"lzma_memlimit_bytes": 1}]:
            self.incomplete(good, fx_metadata(good), limits=dict(LIMITS, **changes))

    def test_capture_integrity_and_missing_inputs_never_impute(self):
        body = encoded([BAR])
        metadata = manifest(body, symbol="BTCUSDT")
        self.incomplete(None, metadata)
        self.incomplete(body, None)
        self.incomplete(body.decode(), metadata)
        for key, value in [("http_status", 404), ("response_complete", False), ("truncated", True),
                           ("transport_error", "timeout"), ("byte_count", len(body) + 1),
                           ("sha256", "0" * 64), ("url", "http://synthetic.invalid"),
                           ("data_origin", "unknown"), ("retrieval_finished_utc", "2020-01-01T00:00:00Z")]:
            self.incomplete(body, dict(metadata, **{key: value}))
        for key in metadata:
            missing = metadata.copy()
            del missing[key]
            if key != "truncated":
                self.incomplete(body, missing)
        empty = b""
        self.incomplete(empty, manifest(empty, symbol="BTCUSDT"))
        self.assertEqual(hashlib.sha256(body).hexdigest(), metadata["sha256"])

    def test_cli_reads_retained_bytes_and_reports_missing_body(self):
        body = encoded([BAR])
        with tempfile.TemporaryDirectory(dir=ROOT) as directory:
            directory = Path(directory)
            meta_path, body_path = directory / "manifest.json", directory / "body.bin"
            meta_path.write_text(json.dumps(manifest(body, symbol="BTCUSDT")))
            body_path.write_bytes(body)
            command = [sys.executable, "-I", "-B", str(ROOT / "observations.py"), str(meta_path), str(body_path)]
            result = subprocess.run(command, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout)["status"], "decoded")
            self.assertEqual(body_path.read_bytes(), body)
            body_path.unlink()
            result = subprocess.run(command, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertEqual(json.loads(result.stdout)["observations"], [])
            self.assertEqual(json.loads(result.stdout)["status"], "incomplete")


if __name__ == "__main__":
    unittest.main()
