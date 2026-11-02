"""Launch markets, read from the production genesis, and the public price series used for each.

The genesis file is the source of truth for margins, caps, and financing. `SERIES` maps each
launch symbol to the public series that stands in for its venue price, with the reason when the
series differs from the venue.
"""

import tomllib
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
GENESIS = REPO / "config" / "production-genesis.toml"

# symbol -> (source, ticker, note)
SERIES = {
    "BTC-USD": ("binance", "BTCUSDT", "Binance spot, USDT quote"),
    "ETH-USD": ("binance", "ETHUSDT", "Binance spot, USDT quote"),
    "SOL-USD": ("binance", "SOLUSDT", "Binance spot, USDT quote"),
    "EUR-USD": ("yahoo", "EURUSD=X", "spot FX"),
    "GBP-USD": ("yahoo", "GBPUSD=X", "spot FX"),
    "USD-JPY": ("yahoo", "JPY=X", "spot FX"),
    "AUD-USD": ("yahoo", "AUDUSD=X", "spot FX"),
    "US500": ("yahoo", "ES=F", "CME E-mini S&P 500 front month"),
    "US100": ("yahoo", "NQ=F", "CME E-mini Nasdaq-100 front month"),
    "US30": ("yahoo", "YM=F", "CBOT E-mini Dow front month"),
    "DE40": ("yahoo", "^GDAXI", "DAX cash index; the venue's future trades longer hours, so gaps are overstated"),
    "UK100": ("yahoo", "^FTSE", "FTSE 100 cash index; the venue's future trades longer hours, so gaps are overstated"),
    "JP225": ("yahoo", "NIY=F", "CME Nikkei 225 yen future, near the venue's day and night sessions"),
    "XAU-USD": ("yahoo", "GC=F", "COMEX gold front month"),
    "XAG-USD": ("yahoo", "SI=F", "COMEX silver front month"),
    "WTI": ("yahoo", "CL=F", "NYMEX WTI front month"),
    "BRENT": ("yahoo", "BZ=F", "NYMEX Brent last-day future, front month"),
    "AAPL": ("yahoo", "AAPL", "NYSE/Nasdaq regular session"),
    "MSFT": ("yahoo", "MSFT", "regular session"),
    "NVDA": ("yahoo", "NVDA", "regular session"),
    "AMZN": ("yahoo", "AMZN", "regular session"),
    "GOOGL": ("yahoo", "GOOGL", "regular session"),
    "META": ("yahoo", "META", "regular session"),
    "TSLA": ("yahoo", "TSLA", "regular session"),
    "AVGO": ("yahoo", "AVGO", "regular session"),
    "BRK.B": ("yahoo", "BRK-B", "regular session"),
    "JPM": ("yahoo", "JPM", "regular session"),
    "V": ("yahoo", "V", "regular session"),
    "MA": ("yahoo", "MA", "regular session"),
    "LLY": ("yahoo", "LLY", "regular session"),
    "JNJ": ("yahoo", "JNJ", "regular session"),
    "XOM": ("yahoo", "XOM", "regular session"),
    "WMT": ("yahoo", "WMT", "regular session"),
    "COST": ("yahoo", "COST", "regular session"),
    "NFLX": ("yahoo", "NFLX", "regular session"),
    "AMD": ("yahoo", "AMD", "regular session"),
    "ORCL": ("yahoo", "ORCL", "regular session"),
}

# The same market on Hyperliquid (main dex, or a builder-deployed dex), whose volume sets the
# model's market mix. A market with no listing takes its asset class's smallest listed share.
HYPERLIQUID = {
    "BTC-USD": "BTC",
    "ETH-USD": "ETH",
    "SOL-USD": "SOL",
    "EUR-USD": "xyz:EUR",
    "GBP-USD": "xyz:GBP",
    "USD-JPY": "xyz:JPY",
    "US500": "xyz:SP500",
    "US100": "xyz:XYZ100",
    "JP225": "xyz:JP225",
    "XAU-USD": "xyz:GOLD",
    "XAG-USD": "xyz:SILVER",
    "WTI": "xyz:CL",
    "BRENT": "xyz:BRENTOIL",
    "AAPL": "xyz:AAPL",
    "MSFT": "xyz:MSFT",
    "NVDA": "xyz:NVDA",
    "AMZN": "xyz:AMZN",
    "GOOGL": "xyz:GOOGL",
    "META": "xyz:META",
    "TSLA": "xyz:TSLA",
    "AVGO": "xyz:AVGO",
    "LLY": "xyz:LLY",
    "COST": "xyz:COST",
    "NFLX": "xyz:NFLX",
    "AMD": "xyz:AMD",
    "ORCL": "xyz:ORCL",
}

# Official daily series used to cross-check the public ones: symbol -> FRED id.
FRED_CHECKS = {
    "EUR-USD": "DEXUSEU",
    "GBP-USD": "DEXUSUK",
    "USD-JPY": "DEXJPUS",
    "AUD-USD": "DEXUSAL",
    "US500": "SP500",
    "US100": "NASDAQ100",
    "US30": "DJIA",
    "JP225": "NIKKEI225",
    "WTI": "DCOILWTICO",
    "BRENT": "DCOILBRENTEU",
}

# Benchmark rates on FRED; the others are assumptions stated in the README.
FRED_BENCHMARKS = {"SOFR": "SOFR", "SONIA": "IUDSOIA", "ESTR": "ECBESTRVOLWGTTRMDMNRT"}


def load_genesis(path: Path = GENESIS) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)["genesis"]


def launch_markets(path: Path = GENESIS) -> list[dict]:
    """The genesis markets, each with its public series attached."""
    markets = load_genesis(path)["markets"]
    missing = [m["symbol"] for m in markets if m["symbol"] not in SERIES]
    if missing:
        raise ValueError(f"no price series for {missing}")
    return [dict(m, series=SERIES[m["symbol"]]) for m in markets]


# The owner's planned margin for BTC-USD and ETH-USD: this first-tier rate up to the same
# notional as today's first tier, then the rates above it unchanged (SOL-USD keeps the genesis
# bands). Not yet in the production genesis; the retained-NAV study runs both.
PLANNED_CRYPTO_SYMBOLS = ("BTC-USD", "ETH-USD")
PLANNED_CRYPTO_FIRST_TIER_BPS = 100


def planned_crypto_tiers(markets: list[dict]) -> list[dict]:
    """BTC-USD's genesis margin bands with the first tier at the planned rate."""
    tiers = next(m["margin_tiers"] for m in markets if m["symbol"] == PLANNED_CRYPTO_SYMBOLS[0])
    return [dict(tiers[0], bps=PLANNED_CRYPTO_FIRST_TIER_BPS), *tiers[1:]]
