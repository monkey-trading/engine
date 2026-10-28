//! `api.allowed_origins`: the browser origins that may call the REST API and open the stream.

use std::collections::BTreeSet;

use anyhow::{bail, ensure};

use crate::config::ApiSection;

impl ApiSection {
    /// Checks every allowed origin is one a browser can send: `http` or `https`, a lowercase host,
    /// and a port only when it is not the scheme's default, as browsers serialize an origin.
    /// Error cases:
    /// - an origin is `*`, has a path, query, fragment, or userinfo, or is not `scheme://host[:port]`
    /// - an origin is spelled so that a browser would never send it
    /// - an origin is listed twice
    pub(super) fn validate_origins(&self) -> anyhow::Result<()> {
        let mut seen = BTreeSet::new();
        for origin in &self.allowed_origins {
            check_origin(origin).map_err(|e| e.context(format!("api.allowed_origins entry {origin:?}")))?;
            ensure!(seen.insert(origin), "api.allowed_origins lists {origin:?} twice");
        }
        Ok(())
    }

    /// Refuses an origin that is not `https` when `production` is set.
    /// Error cases:
    /// - `production` is set and an allowed origin uses `http`
    pub(super) fn refuse_insecure_origins(&self, production: bool) -> anyhow::Result<()> {
        if let Some(origin) = self.allowed_origins.iter().find(|o| production && !o.starts_with("https://")) {
            bail!("a production build allows only https origins: api.allowed_origins lists {origin:?}");
        }
        Ok(())
    }
}

fn check_origin(origin: &str) -> anyhow::Result<()> {
    let Some((scheme, authority)) = origin.split_once("://") else {
        bail!("is not scheme://host[:port]");
    };
    ensure!(matches!(scheme, "http" | "https"), "scheme must be http or https");
    ensure!(
        !authority.is_empty() && !authority.contains(['/', '?', '#', '@', '*']),
        "must be a bare host with an optional port, with no wildcard, path, query, or userinfo"
    );
    ensure!(
        origin == origin.to_ascii_lowercase() && origin.is_ascii() && !origin.contains(char::is_whitespace),
        "must be lowercase ASCII, as a browser sends it"
    );
    let (host, port) = match authority.rsplit_once(':') {
        // The colons of an IPv6 literal sit inside brackets.
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (authority, None),
    };
    ensure!(!host.is_empty(), "has no host");
    if let Some(port) = port {
        let port: u16 = port.parse().map_err(|_| anyhow::anyhow!("port is not a number up to 65535"))?;
        let default = if scheme == "https" { 443 } else { 80 };
        ensure!(port != 0 && port != default, "a browser omits the port when it is {default}, and never sends 0");
    }
    Ok(())
}
