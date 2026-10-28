//! `api.allowed_origins`: the browser origins that may call the REST API and open the stream.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, ensure};
use reqwest::Url;

use crate::config::ApiSection;

impl ApiSection {
    /// Checks every allowed origin is exactly what a browser sends as `Origin` (see `check_origin`),
    /// so none can fail to match silently.
    /// Error cases:
    /// - an origin is `*`, is not an `http` or `https` URL, or is not spelled as a browser sends it
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

/// Whether `origin` is exactly the text a browser sends as `Origin` for an `http` or `https` page.
/// Error cases:
/// - it has a wildcard, or is not a URL
/// - its scheme is not `http` or `https`, or its port is 0
/// - a browser would spell it differently: with a path, query, fragment, or userinfo, an uppercase
///   letter, a default or empty port, or a port with leading zeros
fn check_origin(origin: &str) -> anyhow::Result<()> {
    ensure!(!origin.contains('*'), "has a wildcard");
    let url = Url::parse(origin).map_err(|e| anyhow!("is not a URL: {e}"))?;
    ensure!(matches!(url.scheme(), "http" | "https"), "scheme must be http or https");
    ensure!(url.port() != Some(0), "port 0 is never a page's port");
    let sent = url.origin().ascii_serialization();
    ensure!(sent == origin, "is not spelled as a browser sends it, which is {sent:?}");
    Ok(())
}
