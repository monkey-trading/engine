"""Generate the pinned market inventory and per-market hourly statistics."""

import hashlib
import csv
import gzip
import io
import json
import runpy
from datetime import datetime, timedelta
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[1]
PIN = "41a9ef4a52cb26ac592c5a0e372f610d8d1ba067"
# The genesis file also carries calendars and contract rolls this study does not read, so the pin
# is the canonical JSON of what it does read: the protocol parameters and the market inventory.
INVENTORY_SHA256 = "72ffcc75459ccd23ea1459d87d905746b59a0b3d55ede0fe9a1ba55865dc0e55"
BARS_SHA256 = "d742bab6c751e97338bbb82d7801f22aec3a5bed7d01904b7dcdd9ed076ec522"
SOURCE_HASHES = {
    "catalogue.py": "8426ed40b977bc18c2b80fa240b3d2c22c56b2c871363017e9b84af159cb6396",
    "derive/markets.py": "32629c08a2adcb27109f4dddd529828ccc81421fd583245e673d9108d1edd428",
    "fetch/markets.py": "11d775b4a2fb9bbb226abf79c6d8f3484b60a035e31fc0058aedcf0d41fba855",
}
CALIBRATION_WEEKS = 78
BOOTSTRAP_SEED = 147
BOOTSTRAP_SAMPLES = 200


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify(path, expected):
    actual = digest(path)
    if actual != expected:
        raise ValueError(f"input changed: {path}: expected {expected}, got {actual}")
    return actual


def inventory_digest(genesis_path):
    """Hash the parameters and market fields that calibration reads, so comments and calendars can move."""
    catalogue = runpy.run_path(str(REPO / "research/cap-calibration/catalogue.py"))
    used = [{key: value for key, value in market.items() if key not in ("schedule", "contracts", "financing")}
            for market in catalogue["load_genesis"](genesis_path)["markets"]]
    params = catalogue["load_genesis"](genesis_path)["params"]
    text = json.dumps({"params": params, "markets": used}, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(text.encode()).hexdigest()


def summarize(rows, unit):
    """Measure inherited hourly returns without interpolating closed hours."""
    active = rows[:, 0] != np.iinfo(np.int32).min
    values = rows[active].astype(np.float64) / unit
    if not len(values):
        return {"active_hours": 0, "abs_hourly_p99_bps": None, "abs_gap_max_bps": None}
    returns = np.expm1(values[:, 3]) * 10_000
    gaps = np.abs(np.expm1(values[:, 0]) * 10_000)
    return {
        "active_hours": len(values),
        "abs_hourly_p99_bps": float(np.quantile(np.abs(returns), 0.99, method="higher")),
        "abs_hourly_max_bps": float(np.max(np.abs(returns))),
        "abs_gap_max_bps": float(gaps.max()),
        "hourly_p99_week_block_bootstrap": uncertainty(rows, unit),
        "return_definition": "in-bar move: close against the bar's own open; the open-against-previous-close gap is excluded and reported separately",
        "warning": "transformed hourly proxy; not execution-scale or uncensored gap evidence",
    }


def uncertainty(rows, unit):
    """Resample complete calendar weeks; this is an exploratory interval, not a risk guarantee."""
    if len(rows) < 168 or len(rows) % 168:
        return None
    weeks = rows.reshape(-1, 168, 4)
    generator = np.random.default_rng(BOOTSTRAP_SEED)
    estimates = []
    for _ in range(BOOTSTRAP_SAMPLES):
        sampled = weeks[generator.integers(0, len(weeks), size=len(weeks))].reshape(-1, 4)
        active = sampled[sampled[:, 0] != np.iinfo(np.int32).min].astype(np.float64)
        if len(active):
            returns = np.abs(np.expm1(active[:, 3] / unit)) * 10_000
            estimates.append(float(np.quantile(returns, 0.99, method="higher")))
    if not estimates:
        return None
    return {
        "p025_bps": float(np.quantile(estimates, 0.025, method="lower")),
        "p975_bps": float(np.quantile(estimates, 0.975, method="higher")),
        "replicates": len(estimates),
        "seed": BOOTSTRAP_SEED,
        "replicate_sha256": hashlib.sha256(json.dumps(estimates, separators=(",", ":")).encode()).hexdigest(),
        "limitation": "week-block stationarity assumed; not unseen-tail, proxy or missing-feed uncertainty",
    }


def build():
    genesis = REPO / "config/production-genesis.toml"
    bars_path = REPO / "research/cap-calibration/data/derived/hourly_bars.npz"
    actual = inventory_digest(genesis)
    if actual != INVENTORY_SHA256:
        raise ValueError(f"genesis inventory changed: expected {INVENTORY_SHA256}, got {actual}")
    hashes = {"genesis_inventory": actual, "genesis_file_informational": digest(genesis),
              "bars": verify(bars_path, BARS_SHA256)}
    for relative, expected in SOURCE_HASHES.items():
        hashes[relative] = verify(REPO / "research/cap-calibration" / relative, expected)
    catalogue = runpy.run_path(str(REPO / "research/cap-calibration/catalogue.py"))
    markets = catalogue["launch_markets"](genesis)
    cutoff = CALIBRATION_WEEKS * 168
    with np.load(bars_path, allow_pickle=False) as data:
        symbols = data["symbols"].tolist()
        bars = data["bars"]
        unit = float(data["unit"])
        if symbols != [m["symbol"] for m in markets] or bars.shape != (102 * 168, 37, 4):
            raise ValueError("market inventory or hourly grid does not match pinned source")
        if not np.isfinite(unit) or unit <= 0:
            raise ValueError("invalid log-return unit")
        results = []
        for index, market in enumerate(markets):
            source, ticker, proxy_note = market.pop("series")
            results.append({
                "market": market,
                "source": source,
                "ticker": ticker,
                "instrument_note": proxy_note,
                "resolution": "1h transformed log returns, units 1e-6; grid UTC",
                "calibration": summarize(bars[:cutoff, index], unit),
                "validation": summarize(bars[cutoff:, index], unit),
            })
        return {
            "engine_revision": PIN,
            "input_sha256": hashes,
            "week0_utc": str(data["week0"]),
            "validation_start_utc": (datetime.fromisoformat(str(data["week0"]))
                                     + timedelta(weeks=CALIBRATION_WEEKS)).isoformat(),
            "end_exclusive_utc": (datetime.fromisoformat(str(data["week0"]))
                                  + timedelta(weeks=102)).isoformat(),
            "calibration_weeks": CALIBRATION_WEEKS,
            "validation_weeks": 102 - CALIBRATION_WEEKS,
            "split_rule": "chronological; grid established before any parameter selection",
            "protocol_params": catalogue["load_genesis"](genesis)["params"],
            "markets": results,
        }


if __name__ == "__main__":
    output = ROOT / "results/market_statistics.json"
    result = build()
    output.parent.mkdir(exist_ok=True)
    output.write_text(json.dumps(result, indent=2, allow_nan=False) + "\n")
    sweep = runpy.run_path(str(ROOT / "sweep.py"))
    with (ROOT / "results/conditional_gap_sweep.csv.gz").open("wb") as raw_output, \
            gzip.GzipFile(filename="", mode="wb", fileobj=raw_output, mtime=0) as compressed, \
            io.TextIOWrapper(compressed, encoding="utf-8", newline="") as handle:
        writer = None
        count = 0
        for market in result["markets"]:
            rows = sweep["frontier"](market, result["protocol_params"]["profit_fee_bps"],
                                     result["protocol_params"]["closeout_level_bps"])
            if writer is None:
                writer = csv.DictWriter(handle, fieldnames=list(rows[0]), lineterminator="\n")
                writer.writeheader()
            writer.writerows(rows)
            count += len(rows)
    print(f"{len(result['markets'])} markets audited; statistics written: {output}")
    print(f"{count} conditional gap/cost scenarios; no scenario frequency is a probability")
