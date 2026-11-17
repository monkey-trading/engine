"""Paths and the cap study's inputs, read in place.

The cap study's modules (`catalogue`, `fetch`, `derive`, `model`) are imported from its directory,
not copied: this study reads its committed hourly bars and derived statistics and reuses its
fetch and daily-close code. Its names are put last on the import path so none shadows this package.
"""

import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
RESEARCH = HERE.parent
CAP = RESEARCH / "cap-calibration"
LAUNCH = RESEARCH / "launch-caps"
REPO = RESEARCH.parent
RAW = HERE / "data" / "raw"
DERIVED = HERE / "data" / "derived"
RESULTS = HERE / "results"
GENESIS = REPO / "config" / "production-genesis.toml"

if str(CAP) not in sys.path:
    sys.path.append(str(CAP))

UNIT = 1e6
WEEK = 168

# D3's candidate groups: one asset class, USD as the quote currency, one schedule. USD-JPY (its USD
# profit and loss is nonlinear in its price) and JP225 stay alone; WTI stays alone.
CANDIDATES = {
    "crypto": ["BTC-USD", "ETH-USD", "SOL-USD"],
    "us_index": ["US500", "US100", "US30"],
    "metals": ["XAU-USD", "XAG-USD"],
    "fx": ["EUR-USD", "GBP-USD", "AUD-USD"],
    "shares": [
        "AAPL", "MSFT", "NVDA", "AMZN", "GOOGL", "META", "TSLA", "AVGO", "BRK.B", "JPM", "V", "MA",
        "LLY", "JNJ", "XOM", "WMT", "COST", "NFLX", "AMD", "ORCL",
    ],
}


def genesis() -> dict:
    with GENESIS.open("rb") as fh:
        return tomllib.load(fh)["genesis"]


def launch_symbols() -> list[str]:
    return [m["symbol"] for m in genesis()["markets"]]

ROGUE = RESEARCH / "rogue-operator" / "data" / "derived"
