//! The `[feeds]` section: the mock feed, or the venues and market-data vendors the node reads
//! quotes from, with the checks that keep a feed from failing silently or being forged by the host.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use engine_core::Fixed;
use engine_core::market::MarketSpec;
use engine_core::types::MarketId;
use serde::Deserialize;
use tokio_tungstenite::tungstenite::http::Uri;

use crate::config::{SecretName, SecretValue};
use crate::feeds::check_cme_symbol;

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
    /// CME Group's real-time WebSocket API: its own exchanges' futures.
    Cme,
    Coinbase,
    Kraken,
    /// Massive (formerly Polygon.io): FX, futures, and US stock quotes.
    Massive,
    /// TraderMade: FX quotes.
    Tradermade,
}

#[derive(Clone, Debug, Deserialize)]
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
#[derive(Clone, Debug, Deserialize)]
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
    /// The rate a symbol's quotes may be sequenced at: one per spacing, after a burst of a few. A
    /// book update that moves its mid sequences its symbol's newest quote at once while that budget
    /// lasts, whatever the quote's stamp; past it, updates coalesce to the newest and wait for the
    /// budget to refill, bounding the sequencer load venue messages can demand. A book update that
    /// leaves the mid where it was waits until the quote the engine holds is half of `stale_after`
    /// or the market's `max_age_ms` old, whichever is shorter, and then sequences.
    #[serde(with = "humantime_serde")]
    pub min_quote_spacing: Duration,
    /// How often a connection pings its venue. The time since the newest answered ping was sent
    /// bounds how far behind its stream may be.
    #[serde(with = "humantime_serde")]
    pub ping_interval: Duration,
    /// How far behind a connection's stream may be before what it reads counts as no quote. A
    /// quote's age when it reaches the engine is at most this under a hostile host, so a price's is
    /// at most about this plus its market's `max_age_ms`. Past `ping_interval` by more than the round
    /// trips and stalls an honest network shows, or the market loses its price without cause.
    #[serde(with = "humantime_serde")]
    pub max_delay: Duration,
    pub connections: Vec<VenueConnection>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueConnection {
    pub venue: Venue,
    /// The venue's endpoint: `wss://` on the venue's own domain, and for Massive one of its
    /// `/forex`, `/futures`, or `/stocks` feeds. Only a source reached over TLS that ends in the
    /// enclave may set or move a price: the host controls the network, so plaintext would let it
    /// forge prices, and TLS alone proves only the server's name, not that it is the venue the
    /// quotes are labelled with.
    pub url: String,
    /// The environment variable holding the vendor's API key, which is a credential and stays out
    /// of config files: required for Massive and TraderMade unless `api_key_secret`
    /// names the key, and refused for a venue whose feed is public. The node refuses to start while
    /// a named variable is unset. A production build refuses this field: the guest has no
    /// environment to set it in.
    pub api_key_env: Option<String>,
    /// The delivered secret holding the vendor's API key, in place of `api_key_env`. Until it is
    /// delivered this connection stays off, under `secret_missing`.
    pub api_key_secret: Option<SecretName>,
    /// The OAuth login whose bearer token the WebSocket handshake carries: required for CME and
    /// refused for every other venue.
    pub oauth: Option<OAuthSection>,
    pub symbols: Vec<VenueSymbol>,
}

/// An OAuth 2.0 client-credentials login (RFC 6749, section 4.4) at the venue's token endpoint. The
/// client ID and its secret are delivered secrets: a production build reads no credential from the
/// environment, and until both are delivered the connection stays off, under `secret_missing`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthSection {
    /// The token endpoint: `https://` on the venue's own domain.
    pub token_url: String,
    /// The delivered secret holding the client ID, CME's API ID.
    pub client_id_secret: SecretName,
    /// The delivered secret holding the client secret, the API ID's password.
    pub client_secret_secret: SecretName,
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

/// A vendor credential: an API key, an OAuth client ID or secret, or a bearer token. `Debug` hides
/// it; read it with `expose` where it is used.
#[derive(Clone)]
pub struct ApiKey(String);

#[derive(Clone, Debug, Deserialize)]
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
            Venue::Cme => "cme",
            Venue::Coinbase => "coinbase",
            Venue::Kraken => "kraken",
            Venue::Massive => "massive",
            Venue::Tradermade => "tradermade",
        }
    }

    /// The domain this venue's endpoints are served under.
    fn domain(self) -> &'static str {
        match self {
            Venue::Bitstamp => "bitstamp.net",
            Venue::Cme => "cmegroup.com",
            Venue::Coinbase => "coinbase.com",
            Venue::Kraken => "kraken.com",
            Venue::Massive => "massive.com",
            Venue::Tradermade => "tradermade.com",
        }
    }

    /// Whether `host` is the venue's domain or one of its subdomains.
    fn serves(self, host: &str) -> bool {
        host.strip_suffix(self.domain()).is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('.'))
    }

    /// Whether the venue's feed needs an API key.
    pub(super) fn needs_key(self) -> bool {
        matches!(self, Venue::Massive | Venue::Tradermade)
    }

    /// Whether the venue's WebSocket handshake needs an OAuth bearer token.
    fn needs_oauth(self) -> bool {
        self == Venue::Cme
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

    /// The key a delivered secret holds.
    pub(crate) fn delivered(value: &SecretValue) -> Self {
        ApiKey(value.expose().to_owned())
    }

    /// A credential the node obtained or derived, such as a bearer token.
    pub(crate) fn obtained(value: String) -> Self {
        ApiKey(value)
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
    /// - `timeout`, `reconnect_min`, `stale_after`, or `ping_interval` is zero, `reconnect_min`
    ///   exceeds `reconnect_max`, or `ping_interval` is not under `max_delay`
    ///   (`min_quote_spacing`'s bounds are the config check's)
    /// - no connections, or a connection with no symbols
    /// - a URL that does not parse as `wss://` with a host on the venue's own domain (for Massive,
    ///   with one of its feed paths)
    /// - an API key missing for a vendor or given for a public venue
    /// - an OAuth login missing for CME or given for another venue, or its token URL not `https://`
    ///   on the venue's own domain
    /// - a CME symbol that does not name, as `<product>:<YYYYMM>`, the contract it quotes
    /// - a symbol listed twice on one connection, or a market and contract, or a URL, listed twice
    ///   for one venue
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.timeout.is_zero() && !self.reconnect_min.is_zero() && !self.stale_after.is_zero(),
            "timeout, reconnect_min, and stale_after must be non-zero"
        );
        ensure!(self.reconnect_min <= self.reconnect_max, "reconnect_min exceeds reconnect_max");
        // A frame between pings is judged by the last answer, so this would refuse most of them.
        ensure!(
            !self.ping_interval.is_zero() && self.ping_interval < self.max_delay,
            "ping_interval must be non-zero and shorter than max_delay"
        );
        ensure!(!self.connections.is_empty(), "no connections");
        let mut instruments = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        for conn in &self.connections {
            let name = conn.venue.name();
            ensure!(conn.oauth.is_some() == conn.venue.needs_oauth(), "{name} oauth must be set exactly for cme");
            conn.validate_endpoint()?;
            // A venue's alerts name the connection by its URL, so no two of its connections share one.
            ensure!(endpoints.insert((conn.venue, conn.url.as_str())), "{name} lists url {} twice", conn.url);
            ensure!(
                (conn.api_key_env.is_some() || conn.api_key_secret.is_some()) == conn.venue.needs_key(),
                "{name} api_key_env must be set exactly for a vendor, unless api_key_secret names its key"
            );
            ensure!(
                conn.api_key_env.is_none() || conn.api_key_secret.is_none(),
                "{name} names both api_key_env and api_key_secret"
            );
            ensure!(!conn.symbols.is_empty(), "{name} has no symbols");
            let mut symbols = BTreeSet::new();
            for s in &conn.symbols {
                ensure!(symbols.insert(s.symbol.as_str()), "{name} lists symbol {} twice", s.symbol);
                if conn.venue == Venue::Cme {
                    check_cme_symbol(&s.symbol, s.contract.as_deref())
                        .map_err(|reason| anyhow!("{name} symbol {}: {reason}", s.symbol))?;
                }
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
    /// Checks the endpoint, and any token endpoint, is the venue's own, over TLS.
    /// Error cases:
    /// - as for `VenuesSection::validate`
    fn validate_endpoint(&self) -> anyhow::Result<()> {
        let name = self.venue.name();
        let uri: Uri = self.url.parse().with_context(|| format!("{name} url"))?;
        ensure!(uri.scheme_str() == Some("wss"), "{name} url must be wss://");
        let domain = self.venue.domain();
        ensure!(uri.host().is_some_and(|host| self.venue.serves(host)), "{name} url must be on {domain}");
        if self.venue == Venue::Massive {
            ensure!(MASSIVE_PATHS.contains(&uri.path()), "{name} url must be one of its {MASSIVE_PATHS:?} feeds");
        }
        if let Some(oauth) = &self.oauth {
            let token: Uri = oauth.token_url.parse().with_context(|| format!("{name} oauth.token_url"))?;
            ensure!(token.scheme_str() == Some("https"), "{name} oauth.token_url must be https://");
            let on_domain = token.host().is_some_and(|host| self.venue.serves(host));
            ensure!(on_domain, "{name} oauth.token_url must be on {domain}");
        }
        Ok(())
    }
}

/// Each instrument `market`'s listing names, with the venues among `connections` that quote it:
/// every one of its contracts, rolled off or not, for a market priced from futures, and the market
/// itself (`None`) for one that is not. An instrument no venue quotes maps to none, and a symbol for
/// an instrument the listing does not name can never price the market, so it counts for none.
pub(super) fn venues_quoting<'a>(
    connections: &[VenueConnection],
    market: &'a MarketSpec,
) -> BTreeMap<Option<&'a str>, BTreeSet<Venue>> {
    let instruments: Vec<Option<&str>> = match market.contracts.as_slice() {
        [] => vec![None],
        contracts => contracts.iter().map(|listed| Some(listed.contract.as_str())).collect(),
    };
    instruments
        .into_iter()
        .map(|instrument| {
            let quotes = |conn: &&VenueConnection| {
                conn.symbols.iter().any(|s| s.market == market.id && s.contract.as_deref() == instrument)
            };
            (instrument, connections.iter().filter(quotes).map(|conn| conn.venue).collect())
        })
        .collect()
}
