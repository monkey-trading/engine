//! `[api.proxy_protocol]`: the pass-through proxy whose PROXY protocol v2 header names the client.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use anyhow::{bail, ensure};
use ipnet::{IpNet, Ipv4Net};
use serde::Deserialize;

use crate::config::ApiSection;

/// A production build refuses a trusted range with a shorter prefix: a proxy's ranges are smaller,
/// and a wider one trusts addresses its clients can use.
const MIN_PRODUCTION_PREFIX_V4: u8 = 8;
const MIN_PRODUCTION_PREFIX_V6: u8 = 32;

/// Behind a layer-4 proxy that never decrypts, the node learns each client's address from the PROXY
/// protocol v2 header the proxy sends before the TLS handshake. Absent, no connection may carry one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyProtocolSection {
    /// The proxy's address ranges. A connection from one must begin with a header, which names the
    /// client; a header from anywhere else is refused. The header is unauthenticated, so list only
    /// addresses that reach the node solely through the operator's own proxy: not a range clients
    /// connect from, nor a shared provider's egress range that its other customers can point at
    /// the node, since any address in the range can claim to be any client.
    pub trusted: Vec<IpNet>,
}

impl ProxyProtocolSection {
    /// Whether `ip`, an IPv4-mapped IPv6 address counted as its IPv4 address, is in a trusted range.
    pub fn trusts(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.trusted.iter().any(|range| range.contains(&ip))
    }

    /// Checks the ranges can be matched and trust a proxy, not every address.
    /// Error cases:
    /// - `trusted` is empty, has a range of every address, or has an IPv4-mapped IPv6 range, which
    ///   no peer matches since peers are matched by their IPv4 form
    fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.trusted.is_empty(), "api.proxy_protocol.trusted must list at least one range");
        ensure!(
            self.trusted.iter().all(|range| range.prefix_len() > 0),
            "api.proxy_protocol.trusted must not list every address: any client could claim any address"
        );
        ensure!(
            self.trusted.iter().all(|range| !matches!(range, IpNet::V6(v6) if v6.network().to_ipv4_mapped().is_some())),
            "api.proxy_protocol.trusted must list an IPv4-mapped IPv6 range as its IPv4 range"
        );
        Ok(())
    }

    /// Checks no trusted range is the node's own host or wider than a proxy's.
    /// Error cases:
    /// - a range holds a loopback address or is wider than a proxy provider's ranges
    fn refuse_unsafe(&self) -> anyhow::Result<()> {
        let loopback_v4 = Ipv4Net::new(Ipv4Addr::LOCALHOST, 8).expect("a /8 is a valid prefix");
        for range in &self.trusted {
            let (min, loopback) = match range {
                IpNet::V4(v4) => (
                    MIN_PRODUCTION_PREFIX_V4,
                    loopback_v4.contains(&v4.network()) || v4.contains(&loopback_v4.network()),
                ),
                IpNet::V6(v6) => (MIN_PRODUCTION_PREFIX_V6, v6.contains(&Ipv6Addr::LOCALHOST)),
            };
            if loopback {
                bail!("a production build trusts no loopback range: api.proxy_protocol.trusted lists {range}");
            }
            ensure!(
                range.prefix_len() >= min,
                "a production build trusts no range wider than /{min}: api.proxy_protocol.trusted lists {range}"
            );
        }
        Ok(())
    }
}

impl ApiSection {
    /// Checks the PROXY protocol section, when present.
    /// Error cases:
    /// - see `ProxyProtocolSection::validate`
    pub(super) fn validate_proxy_protocol(&self) -> anyhow::Result<()> {
        self.proxy_protocol.as_ref().map_or(Ok(()), ProxyProtocolSection::validate)
    }

    /// Refuses, in a production build, a trusted proxy range that is not the proxy's alone.
    /// Error cases:
    /// - `production` is set and a trusted range holds a loopback address or is wider than
    ///   `MIN_PRODUCTION_PREFIX_V4` / `MIN_PRODUCTION_PREFIX_V6`
    pub(super) fn refuse_unsafe_proxy(&self, production: bool) -> anyhow::Result<()> {
        match &self.proxy_protocol {
            Some(proxy) if production => proxy.refuse_unsafe(),
            _ => Ok(()),
        }
    }
}
