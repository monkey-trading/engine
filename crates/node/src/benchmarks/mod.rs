//! Fetch dated observations over publisher-authenticated HTTPS for the external operator worker.

mod parse;
mod schedule;
pub use schedule::BenchmarkSchedule;

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, ensure};
use chrono::NaiveDate;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate, BenchmarkState, MAX_BENCHMARK_RATES};
use reqwest::{Client, Url};
use rustls::{ClientConfig, RootCertStore};
use tracing::{Instrument, info, info_span, warn};

use crate::config::{BenchmarkSection, BenchmarkSource, Publisher};

/// Bounds downloaded metadata as well as observations; the host controls the network.
const MAX_PUBLISHER_BYTES: usize = 8 * 1024 * 1024;

/// A client with compiled-in roots, deadlines, no proxies, and no redirect to another publisher.
/// Error cases:
/// - TLS or HTTP client setup fails
pub(crate) fn client() -> anyhow::Result<Client> {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls = ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Client::builder()
        .user_agent(concat!("engine-benchmark-ingestion/", env!("CARGO_PKG_VERSION")))
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .context("building benchmark HTTP client")
}

/// Fetches each required publisher, including all observations needed for missed rollovers.
/// Failed publishers are recorded as checked: their prior effective observation still applies, as it
/// does for a rate the protocol's cap refuses, which stays out of the snapshot so the other rates
/// and a rollover waiting for a snapshot are not held back by it.
/// Error cases:
/// - a required benchmark has no configured publisher, the snapshot is too large
pub(crate) async fn snapshot(
    cfg: &BenchmarkSection,
    client: &Client,
    state: &BenchmarkState,
    as_of: NaiveDate,
) -> anyhow::Result<BenchmarkBatch> {
    let checked: BTreeSet<String> = cfg.sources.iter().map(|s| s.publisher.benchmark().into()).collect();
    ensure!(state.required.is_subset(&checked), "a listed benchmark has no configured publisher");
    let from = state.next_rollover.map(|ts| state.date_at(ts)).unwrap_or(as_of).min(as_of);
    let mut rates = Vec::new();
    for source in &cfg.sources {
        let span = info_span!("benchmark_fetch", id = source.publisher.benchmark(), %as_of);
        let observations = async {
            match fetch(source, client, cfg.timeout, from, as_of).await {
                Ok(observations) => {
                    info!(
                        alert_resolved = "benchmark_fetch_failed",
                        benchmark = source.publisher.benchmark(),
                        "publisher history fetched"
                    );
                    within_cap(source, state, observations)
                }
                Err(err) => {
                    warn!(
                        alert = "benchmark_fetch_failed",
                        benchmark = source.publisher.benchmark(),
                        "publisher fetch failed; retaining previously sequenced history: {err:#}"
                    );
                    Vec::new()
                }
            }
        }
        .instrument(span)
        .await;
        rates.extend(observations);
    }
    ensure!(rates.len() <= MAX_BENCHMARK_RATES, "publisher snapshot exceeds the protocol batch bound");
    info!(observations = rates.len(), %as_of, "benchmark publisher snapshot fetched");
    Ok(BenchmarkBatch { as_of, checked, rates })
}

/// The observations the protocol's cap admits for one publisher. A refused rate is not signed: the
/// Warning names it, and `benchmark_rate_stale` escalates if the benchmark stays behind.
fn within_cap(
    source: &BenchmarkSource,
    state: &BenchmarkState,
    observations: Vec<BenchmarkRate>,
) -> Vec<BenchmarkRate> {
    let benchmark = source.publisher.benchmark();
    let (admitted, refused) = state.split_by_cap(&observations);
    if refused.is_empty() {
        info!(alert_resolved = "benchmark_rate_refused", benchmark, "publisher history is within the cap");
    } else {
        let refusals = refused.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
        warn!(
            alert = "benchmark_rate_refused",
            benchmark, "not signing what the cap refuses, and the last applied rate stays in force: {refusals}"
        );
    }
    admitted
}

/// Downloads and parses one publisher's history, bounded by both bytes and time.
/// Error cases:
/// - invalid URL, HTTP failure/timeout, excessive body, or invalid publisher CSV
async fn fetch(
    source: &BenchmarkSource,
    client: &Client,
    timeout: std::time::Duration,
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<Vec<engine_core::benchmark::BenchmarkRate>> {
    let url = publisher_url(source, to)?;
    let exchange = async {
        let mut response = client.get(url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_PUBLISHER_BYTES,
                "publisher response exceeds byte bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        parse::parse(source.publisher, &bytes, from, to)
    };
    tokio::time::timeout(timeout, exchange).await.context("publisher request timed out")?
}

/// Adds publisher-specific date bounds to a configured official endpoint.
/// Error cases:
/// - the endpoint is not a URL
fn publisher_url(source: &BenchmarkSource, to: NaiveDate) -> anyhow::Result<Url> {
    // Fetch the full supported history so even a long-stale predecessor can be selected.
    let from = NaiveDate::from_ymd_opt(1970, 1, 1).expect("the earliest protocol date exists");
    let mut url = Url::parse(&source.endpoint)?;
    match source.publisher {
        Publisher::Sofr => {
            url.query_pairs_mut().append_pair("startDate", &from.to_string()).append_pair("endDate", &to.to_string());
        }
        Publisher::Estr => {
            url.query_pairs_mut()
                .append_pair("startPeriod", &from.to_string())
                .append_pair("endPeriod", &to.to_string())
                .append_pair("format", "csvdata");
        }
        Publisher::Sonia => {
            url.query_pairs_mut()
                .append_pair("Datefrom", &from.format("%d/%b/%Y").to_string())
                .append_pair("Dateto", &to.format("%d/%b/%Y").to_string());
        }
        Publisher::Aonia | Publisher::Tona => {}
    }
    Ok(url)
}

#[cfg(test)]
mod tests;
