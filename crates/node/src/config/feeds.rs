//! The `[feeds]` section: the mock feed, or the venues and market-data vendors the node reads
//! quotes from, with the checks that keep a feed from failing silently or being forged by the host.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use engine_core::Fixed;
use engine_core::types::MarketId;
use serde::Deserialize;
use tokio_tungstenite::tungstenite::http::Uri;

/// Massive's real-time WebSocket endpoints, one per asset class, by path.
const MASSIVE_PATHS: [&str; 3] = ["/forex", "/futures", "/stocks"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedProvider {
    /// A random walk quoted by three fake venues. Local development only.
    Mock,
    /// Live venue WebSockets from `[feeds.venues]`, one supervised task per connection.
    Venues,
}

/// A venue or market-data vendor whose live feed the node reads. Its name is the quote source in
/// the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Bitstamp,
    Coinbase,
    Kraken,
    /// Massive (formerly Polygon.io): FX, futures, and US stock quotes.
    Massive,
    /// TraderMade: FX quotes.
    Tradermade,
    /// Databento: futures and US stock quotes from its live gateway.
    Databento,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedsSection {
    pub provider: FeedProvider,
    /// The mock feed's tick.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Starting mids for the mock feed, by market id.
    #[serde(default)]
    pub mock_mids: Vec<MockMid>,
    /// Required when `provider = "venues"`; validated whenever present.
    pub venues: Option<VenuesSection>,
}

/// Live venue feeds.
///
/// This file comes from host storage, so the host chooses the venues and the symbol each market
/// reads; the attestation binds the file's SHA-256, so a verifier checks that choice against a
/// published copy of the config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenuesSection {
    /// Deadline to connect and subscribe, and the longest a connection, or one of its symbols, may
    /// go without a fresh quote while its market is open before the connection is replaced and the
    /// gap alerted.
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// The first reconnect delay; it doubles after each attempt that sequences no fresh quote.
    #[serde(with = "humantime_serde")]
    pub reconnect_min: Duration,
    #[serde(with = "humantime_serde")]
    pub reconnect_max: Duration,
    /// A quote older than this, or than its market's `max_age_ms` in the engine, once sequenced
    /// does not count as its venue quoting: it ends no outage or silence.
    #[serde(with = "humantime_serde")]
    pub stale_after: Duration,
    /// The most often one symbol's latest quote is sequenced: a book update sequences its
    /// symbol's newest reached quote at once, and updates closer together than this coalesce to
    /// the newest, bounding the sequencer load venue messages can demand.
    #[serde(with = "humantime_serde")]
    pub min_quote_spacing: Duration,
    pub connections: Vec<VenueConnection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConnection {
    pub venue: Venue,
    /// The venue's endpoint: `wss://` on the venue's own domain, and for Massive one of its
    /// `/forex`, `/futures`, or `/stocks` feeds. The host controls the network, so plaintext would
    /// let it forge prices, and TLS alone proves only the server's name, not that it is the venue
    /// the quotes are labelled with. Databento's live gateway speaks only plain TCP, so its
    /// endpoint is `tcp://<gateway>.lsg.databento.com:13000`.
    ///
    /// TODO: the host can rewrite a Databento feed in flight. The divergence bound limits what it
    /// gains: a market priced from Databento and one TLS source moves at most half its
    /// `max_divergence_bps`, and loses its price beyond that. On a futures market it can move both
    /// contracts' medians apart before a roll, so the roll's gap, and with it the index from then
    /// on, shifts by up to the full `max_divergence_bps` and keeps that shift. Databento offers no
    /// TLS gateway.
    pub url: String,
    /// The environment variable holding the vendor's API key, which is a credential and stays out
    /// of config files: required for Massive, TraderMade, and Databento, and refused for a venue
    /// whose feed is public. The node refuses to start while a named variable is unset.
    pub api_key_env: Option<String>,
    /// The Databento dataset the gateway serves, e.g. `GLBX.MDP3`; required for Databento only.
    pub dataset: Option<String>,
    pub symbols: Vec<VenueSymbol>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueSymbol {
    pub market: MarketId,
    /// The venue's own name for the instrument, e.g. `BTC-USD`, or `ESZ6` for a futures contract.
    pub symbol: String,
    /// For a market priced from futures, the contract this symbol quotes, as the market's listing
    /// names it.
    pub contract: Option<String>,
}

/// A vendor API key. `Debug` hides it; read it with `expose` where it is used.
#[derive(Clone)]
pub struct ApiKey(String);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockMid {
    pub market: u32,
    pub mid: Fixed,
    /// Stops quoting this market this long after boot, so its price goes stale.
    #[serde(default, with = "humantime_serde")]
    pub stop_after: Option<Duration>,
}

impl fmt::Debug for ApiKey {
    /// Never prints the key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl Venue {
    /// The source name this venue's quotes carry.
    pub fn name(self) -> &'static str {
        match self {
            Venue::Bitstamp => "bitstamp",
            Venue::Coinbase => "coinbase",
            Venue::Kraken => "kraken",
            Venue::Massive => "massive",
            Venue::Tradermade => "tradermade",
            Venue::Databento => "databento",
        }
    }

    /// The domain this venue's endpoints are served under.
    fn domain(self) -> &'static str {
        match self {
            Venue::Bitstamp => "bitstamp.net",
            Venue::Coinbase => "coinbase.com",
            Venue::Kraken => "kraken.com",
            Venue::Massive => "massive.com",
            Venue::Tradermade => "tradermade.com",
            Venue::Databento => "databento.com",
        }
    }

    /// Whether `host` is the venue's domain or one of its subdomains.
    fn serves(self, host: &str) -> bool {
        host.strip_suffix(self.domain()).is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
    }

    /// Whether the venue's feed needs an API key.
    pub(super) fn needs_key(self) -> bool {
        matches!(self, Venue::Massive | Venue::Tradermade | Venue::Databento)
    }
}

impl ApiKey {
    /// Reads the key from the environment variable `var`, looked up through `env`.
    /// Error cases:
    /// - `var` is unset or empty
    pub fn from_env(var: &str, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let key = env(var).filter(|key| !key.is_empty());
        Ok(ApiKey(key.ok_or_else(|| anyhow!("environment variable {var} is not set"))?))
    }

    /// The key, for the one place that authenticates with it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl VenueConnection {
    /// The connection's API key, from the environment variable its config names, if any.
    /// Error cases:
    /// - the named variable is unset or empty
    pub fn api_key(&self, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Option<ApiKey>> {
        let read =
            |var: &String| ApiKey::from_env(var, &env).with_context(|| format!("{} api_key_env", self.venue.name()));
        self.api_key_env.as_ref().map(read).transpose()
    }
}

impl VenuesSection {
    /// Checks the values that would otherwise fail silently: a venue quoting one instrument under
    /// one source name twice, a symbol that could mean two instruments, a feed missing its
    /// credentials, or one the host could forge.
    /// Error cases:
    /// - `timeout`, `reconnect_min`, or `stale_after` is zero, or `reconnect_min` exceeds
    ///   `reconnect_max` (`min_quote_spacing`'s bounds are the config check's)
    /// - no connections, or a connection with no symbols
    /// - a URL that does not parse as `wss://` with a host on the venue's own domain (for Massive,
    ///   with one of its feed paths), or for Databento as `tcp://` on its domain with a port
    /// - an API key missing for a vendor or given for a public venue, or a dataset missing for
    ///   Databento or given for any other venue
    /// - a symbol listed twice on one connection, or a market and contract, or a URL, listed twice
    ///   for one venue
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.timeout.is_zero() && !self.reconnect_min.is_zero() && !self.stale_after.is_zero(),
            "timeout, reconnect_min, and stale_after must be non-zero"
        );
        ensure!(self.reconnect_min <= self.reconnect_max, "reconnect_min exceeds reconnect_max");
        ensure!(!self.connections.is_empty(), "no connections");
        let mut instruments = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        for conn in &self.connections {
            let name = conn.venue.name();
            conn.validate_endpoint()?;
            // A venue's alerts name the connection by its URL, so no two of its connections share one.
            ensure!(endpoints.insert((conn.venue, conn.url.as_str())), "{name} lists url {} twice", conn.url);
            ensure!(
                conn.api_key_env.is_some() == conn.venue.needs_key(),
                "{name} api_key_env must be set exactly for a vendor"
            );
            ensure!(
                conn.dataset.is_some() == (conn.venue == Venue::Databento),
                "{name} dataset must be set exactly for databento"
            );
            ensure!(!conn.symbols.is_empty(), "{name} has no symbols");
            let mut symbols = BTreeSet::new();
            for s in &conn.symbols {
                ensure!(symbols.insert(s.symbol.as_str()), "{name} lists symbol {} twice", s.symbol);
                let contract = s.contract.as_deref().map(|c| format!(" contract {c}")).unwrap_or_default();
                ensure!(
                    instruments.insert((conn.venue, s.market, s.contract.as_deref())),
                    "{name} lists market {}{contract} twice",
                    s.market
                );
            }
        }
        Ok(())
    }
}

impl VenueConnection {
    /// Checks the endpoint is the venue's own, over TLS except for Databento's plain TCP gateway.
    /// Error cases:
    /// - as for `VenuesSection::validate`
    fn validate_endpoint(&self) -> anyhow::Result<()> {
        let name = self.venue.name();
        let uri: Uri = self.url.parse().with_context(|| format!("{name} url"))?;
        let scheme = if self.venue == Venue::Databento { "tcp" } else { "wss" };
        ensure!(uri.scheme_str() == Some(scheme), "{name} url must be {scheme}://");
        let domain = self.venue.domain();
        ensure!(uri.host().is_some_and(|host| self.venue.serves(host)), "{name} url must be on {domain}");
        if self.venue == Venue::Databento {
            ensure!(uri.port_u16().is_some(), "{name} url must name the gateway port");
        }
        if self.venue == Venue::Massive {
            ensure!(MASSIVE_PATHS.contains(&uri.path()), "{name} url must be one of its {MASSIVE_PATHS:?} feeds");
        }
        Ok(())
    }
}
