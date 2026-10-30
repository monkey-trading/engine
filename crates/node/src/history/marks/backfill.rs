//! Provider backfill: seeding the marks cache from a venue's public candles endpoint before the
//! log has depth of its own. Only the Coinbase public REST candles are implemented — no key, one
//! adapter per venue's format — and the config pins the endpoint to the venue's own domain over
//! TLS. Seeded buckets carry `seeded_from_provider` provenance, and a log mark re-labels its
//! bucket, so a client can weigh seeded history against what the public log proves.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::config::{BackfillSection, FeedsSection, Venue};
use crate::history::marks::Resolution;
use crate::history::marks::run::{BATCH, Live};
use crate::history::marks::store::{Seed, Store};
use anyhow::{Context, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use futures_util::StreamExt;
use reqwest::{Client, redirect};
use serde_json::Value;

/// The most candles the endpoint returns for one request.
const MAX_CANDLES: Millis = 300;
/// How long one candles request waits.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most bytes one candles response may carry: 300 rows of six numbers never approach it.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
/// The pace between page requests, under the venue's public rate limit.
const PAGE_PACE: Duration = Duration::from_millis(200);
/// The most pages one resolution's backfill fetches, so a misconfigured depth cannot loop
/// unbounded; the default depths need at most a few hundred.
const MAX_PAGES: usize = 1_000;

/// A market's backfill source: the venue and the venue's own name for the instrument.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub venue: Venue,
    pub symbol: String,
}

/// One provider candle: the bucket start and its OHLC.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub ts: Millis,
    pub open: Fixed,
    pub high: Fixed,
    pub low: Fixed,
    pub close: Fixed,
}

/// The markets a config's feeds can backfill, one source per market. Only Coinbase connections
/// have a candles adapter; other venues' markets serve log-derived history until an adapter lands.
pub fn sources(feeds: &FeedsSection) -> BTreeMap<MarketId, Source> {
    let Some(venues) = &feeds.venues else { return BTreeMap::new() };
    venues
        .connections
        .iter()
        .filter(|conn| conn.venue == Venue::Coinbase)
        .flat_map(|conn| {
            conn.symbols.iter().map(move |s| (s.market, Source { venue: conn.venue, symbol: s.symbol.clone() }))
        })
        .collect()
}

/// The public candles client for one venue endpoint.
pub struct Candles {
    http: Client,
    base_url: String,
}

impl Candles {
    /// Builds the bounded client without contacting the venue.
    /// Error cases:
    /// - the HTTP client cannot be built
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| reqwest::Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .context("loading candles TLS roots")?;
        let http = Client::builder()
            .tls_certs_only(roots)
            .no_proxy()
            .redirect(redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the candles client")?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_owned() })
    }

    /// The candles whose buckets start in [`start`, `end`], newest first, at `granularity`
    /// seconds. The endpoint answers at most the `MAX_CANDLES` most recent; wider windows page
    /// (see `windows`).
    /// Error cases:
    /// - the request times out or fails, the status is not success, the body exceeds the byte
    ///   bound, or a row does not parse
    pub async fn fetch(
        &self,
        product: &str,
        granularity: u32,
        start: Millis,
        end: Millis,
    ) -> anyhow::Result<Vec<Candle>> {
        // A symbol never carries path or query characters: it is one path segment of the request.
        ensure!(
            product.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
            "the product {product:?} is not one safe path segment"
        );
        let span = format!("{product} {granularity}s {start}..{end}");
        let url = format!(
            "{}/products/{product}/candles?granularity={granularity}&start={}&end={}",
            self.base_url,
            rfc3339(start),
            rfc3339(end),
        );
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("fetching candles for {span}"))?
            .error_for_status()
            .with_context(|| format!("fetching candles for {span}"))?;
        // Read incrementally under the byte bound, so a hostile or broken endpoint cannot grow
        // the response without limit.
        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.with_context(|| format!("reading candles for {span}"))?;
            ensure!(
                body.len().saturating_add(chunk.len()) as u64 <= MAX_RESPONSE_BYTES,
                "the candles response for {span} exceeds {MAX_RESPONSE_BYTES} bytes"
            );
            body.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&body).with_context(|| format!("the candles for {span} are not UTF-8"))?;
        parse_candles(text).with_context(|| format!("parsing candles for {span}"))
    }
}

/// The request windows, newest first, that cover the `depth` before `now` at `granularity`
/// seconds a bucket width: each window asks for the `MAX_CANDLES` buckets ending at its `end`, and
/// the next window ends one bucket before the previous window's oldest.
pub fn windows(now: Millis, depth_ms: Millis, granularity_ms: Millis) -> Vec<(Millis, Millis)> {
    let oldest = now.saturating_sub(depth_ms);
    let step = MAX_CANDLES * granularity_ms;
    let mut windows = Vec::new();
    let mut end = now;
    // One past the page bound, so a depth over it is visible to the caller's refusal rather than
    // truncated; a depth near Millis' range cannot loop past it or underflow.
    while end >= oldest && windows.len() <= MAX_PAGES {
        let start = (end - (MAX_CANDLES - 1) * granularity_ms).max(oldest);
        windows.push((start, end));
        end = end.saturating_sub(step);
    }
    windows
}

/// Seeds every backfillable resolution of `market` from its provider, paging back from `now` —
/// the engine's admitted time, never the host clock — and pacing requests. Between pages it
/// drains live samples into `buffered`, flushing at `BATCH`, so a long fetch cannot overflow the
/// queue and lose marks. Returns how many buckets were added. A fetch failure stops the seeding
/// with nothing recorded as complete: a later run inserts the missing rows over the ones already
/// there.
/// Error cases:
/// - a page fetch fails, or the store rejects a seed or a flush
pub async fn seed_market(
    http: &Candles,
    source: &Source,
    market: MarketId,
    cfg: &BackfillSection,
    now: Millis,
    store: &mut Store,
    live: &mut Live,
) -> anyhow::Result<usize> {
    ensure!(source.venue == Venue::Coinbase, "no candles adapter for {}", source.venue.name());
    let mut seeded = 0;
    for (resolution, depth) in
        [(Resolution::M1, cfg.depth_1m), (Resolution::H1, cfg.depth_1h), (Resolution::D1, cfg.depth_1d)]
    {
        let depth_ms = Millis::try_from(depth.as_millis()).context("backfill depth")?;
        let pages = windows(now, depth_ms, resolution.ms());
        ensure!(
            pages.len() <= MAX_PAGES,
            "the {resolution:?} depth asks for {} pages, over the {MAX_PAGES}-page bound",
            pages.len()
        );
        let mut rows = Vec::new();
        for (start, end) in pages {
            for candle in http.fetch(&source.symbol, (resolution.ms() / 1_000) as u32, start, end).await? {
                // A conforming endpoint answers within the window; a bucket past `now` is the
                // endpoint's noise, not history, and is never seeded.
                if candle.ts > now {
                    continue;
                }
                rows.push(Seed {
                    market,
                    resolution,
                    ts: resolution.bucket(candle.ts),
                    open: candle.open,
                    high: candle.high,
                    low: candle.low,
                    close: candle.close,
                });
            }
            tokio::time::sleep(PAGE_PACE).await;
            // Keep draining live marks while fetching, so the queue cannot fill and drop samples.
            while let Ok(sample) = live.samples.try_recv() {
                live.buffered.push(sample);
            }
            if live.buffered.len() >= BATCH {
                store.observe(&live.buffered)?;
                live.buffered.clear();
            }
        }
        seeded += store.seed(&rows, now)?;
    }
    Ok(seeded)
}

/// Turns the venue's JSON — `[time, low, high, open, close, volume]` rows, newest first, prices
/// as numbers or strings — into candles. A malformed row is refused rather than guessed at.
/// Error cases:
/// - the text is not a JSON array of six-element rows, an element is neither a number nor a
///   string, a price does not parse or is outside the valid input range, or a time is negative
pub fn parse_candles(text: &str) -> anyhow::Result<Vec<Candle>> {
    let rows: Vec<Value> = serde_json::from_str(text).context("the candles are not a JSON array")?;
    rows.iter().map(candle).collect()
}

/// One parsed row of `parse_candles`.
/// Error cases:
/// - as for `parse_candles`
fn candle(row: &Value) -> anyhow::Result<Candle> {
    let columns = row.as_array().context("a candle is not an array")?;
    ensure!(columns.len() == 6, "a candle has {} columns, not 6", columns.len());
    let seconds = columns[0].as_i64().context("a candle time is not an integer")?;
    ensure!(seconds >= 0, "a candle time is negative");
    let price = |column: usize| -> anyhow::Result<Fixed> {
        let owned;
        let text: &str = match &columns[column] {
            Value::String(text) => text,
            Value::Number(number) => {
                owned = number.to_string();
                &owned
            }
            other => bail!("a candle price is a {}", other),
        };
        let price: Fixed = text.parse().with_context(|| format!("a candle price {text:?} does not parse"))?;
        ensure!(price.is_valid_input(), "a candle price {price} is out of range");
        Ok(price)
    };
    Ok(Candle {
        ts: seconds.checked_mul(1_000).context("a candle time overflows milliseconds")?,
        open: price(3)?,
        high: price(2)?,
        low: price(1)?,
        close: price(4)?,
    })
}

/// `ts` as the RFC 3339 UTC second the candles endpoint takes.
fn rfc3339(ts: Millis) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts)
        .expect("millisecond timestamps build")
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests;
