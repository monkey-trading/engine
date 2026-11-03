//! The `[api]` section: limits on the public API and the cadence of the stats scans behind it.

use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use serde::Deserialize;

use super::api_defaults::{default_heartbeat_interval, default_quote_interval, default_quote_max_markets};
use super::stats::{default_stats_interval, default_stats_light_interval};
use super::{ApiTlsSection, ProxyProtocolSection};

/// Limits on the public API. A client is its socket peer IP, or the address a trusted proxy's PROXY
/// protocol header names, with an IPv6 client counted by its /64; forwarding headers are ignored
/// because the host could forge them.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiSection {
    /// Open connections, in total and from one client; one over either is closed unread. Keep the
    /// total below the process's open-file limit, or accepts fail before the cap is reached.
    pub max_connections: NonZeroUsize,
    pub max_connections_per_ip: NonZeroUsize,
    /// A connection that has not finished its TLS handshake by then is closed.
    #[serde(with = "humantime_serde")]
    pub handshake_timeout: Duration,
    /// A client gets this long to send a request's headers, and again its body; a connection idle
    /// this long between requests is closed.
    #[serde(with = "humantime_serde")]
    pub read_timeout: Duration,
    /// Largest request body, which gets 413 when larger, and largest message a stream subscriber
    /// may send, which ends its stream when larger.
    pub max_body_bytes: NonZeroUsize,
    /// Requests from one client, and requests that act for one account; excess requests get 429.
    /// A never-credited account's free requests are also counted at these rates: `per_ip` is the
    /// budget all clients share, and `per_account` each client's share of it.
    pub per_ip: RateLimit,
    pub per_account: RateLimit,
    /// New connections from one client; one over it is closed before its TLS handshake. Set it
    /// above `per_ip`, so a client that opens a connection per request gets 429 first.
    pub per_ip_connections: RateLimit,
    /// Open WebSocket stream subscriptions; one more gets 503.
    pub max_subscribers: NonZeroUsize,
    /// A client that takes longer than this to take a response, or a stream subscriber one
    /// message, is disconnected.
    #[serde(with = "humantime_serde")]
    pub send_timeout: Duration,
    /// A signed stream connection gets a signed heartbeat at least this often, so its client can
    /// tell a quiet log from a held-back one. At least one second.
    #[serde(default = "default_heartbeat_interval", with = "humantime_serde")]
    pub heartbeat_interval: Duration,
    /// A signed stream connection that asked for an account's quotes gets them again when one
    /// changes: as soon as the log head moves if its last read was this long ago, and never more often
    /// than this. At least 100 ms.
    #[serde(default = "default_quote_interval", with = "humantime_serde")]
    pub quote_interval: Duration,
    /// Most markets one quote subscription may name.
    #[serde(default = "default_quote_max_markets")]
    pub quote_max_markets: NonZeroUsize,
    /// The least sequenced time between two scans of every account for the house's position
    /// figures (`GET /v1/house`, `GET /v1/house/positions`) and the long and short series. At
    /// least one second; 60 seconds by default.
    #[serde(default = "default_stats_interval", with = "humantime_serde")]
    pub stats_interval: Duration,
    /// The least sequenced time between two scans that total only each market's long and short, the
    /// open interest and the open positions, which the House page shows live. Every `stats_interval`
    /// the scan is a full one instead; a value above it leaves only the full scans. At least one
    /// second; five seconds by default.
    #[serde(default = "default_stats_light_interval", with = "humantime_serde")]
    pub stats_light_interval: Duration,
    /// Browser origins (`scheme://host[:port]`) that may read the REST API and open the stream. Empty
    /// by default, which sends no CORS headers and refuses a stream upgrade that carries any `Origin`
    /// (a client that sends none still connects). The
    /// operator's `/v1/admin` route is never opened to a browser. A production build takes only
    /// `https` origins.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// A public CA certificate for the API's domain; without it the API serves only its
    /// self-signed certificate, which clients trust by pinning the attested key.
    pub tls: Option<ApiTlsSection>,
    /// A pass-through proxy in front of the node, whose PROXY protocol header names each client.
    /// Absent, the client is the socket peer.
    pub proxy_protocol: Option<ProxyProtocolSection>,
}

/// A token bucket: `burst` requests at once, refilled at `per_second`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub per_second: NonZeroU32,
    pub burst: NonZeroU32,
}
