//! `engine ct-watch`: a separate process, outside the enclave and holding no key of the node's,
//! that follows Certificate Transparency logs and alerts when a certificate for the API's domain
//! names a key no verified attestation of the node names. It polls the node's `/v1/attestation`,
//! verifies it with the native verifier, and keeps each attested TLS key hash. It reads each log's
//! signed tree head and new entries (RFC 6962), checks every certificate and precertificate, and
//! raises the Critical `unattested_certificate` through the same webhook routing as the node.
//! `ct_log_unreachable` and `attestation_unverified` say the watcher cannot do its job.
//!
//! The logs are untrusted input and are bounded (see `log_client`). The watcher does not check
//! consistency or inclusion proofs, so a log that shows different views to different clients is out
//! of scope. It sees only certificates in the logs it follows: follow every log the CA submits to.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use clap::Args;
use futures_util::FutureExt as _;
use tokio::net::TcpListener;
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{Instant, Interval, MissedTickBehavior, interval, interval_at, timeout};
use tracing::{Instrument, error, info, info_span, warn};

use crate::benchmarks;
use crate::clock::now_ms;
use crate::metrics;
use crate::telemetry::{Telemetry, init_worker_telemetry};

mod attestation;
use self::attestation::{AttestationSource, Poll};
mod config;
pub use self::config::WatchConfig;
use self::config::{LogSection, endpoint_url};
mod leaf;
use self::leaf::{Certificate, parse_leaf, suspect_unparsed};
mod log_client;
use self::log_client::{LogClient, LogKey, MAX_BATCH};
mod state;
use self::state::{State, StateFile};
mod tracker;
use self::tracker::Tracker;
#[cfg(test)]
mod tests;

/// How long the alerts already logged get to be delivered after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
/// How far ahead of this clock a log's tree head may be dated.
const MAX_HEAD_SKEW_MS: u64 = 5 * 60 * 1000;
/// The shortest and longest time one log's read may take.
const MIN_LOG_BUDGET: Duration = Duration::from_secs(45);
const MAX_LOG_BUDGET: Duration = Duration::from_secs(180);

#[derive(Args)]
pub struct CtWatchArgs {
    /// The watcher's config: the domain, the node and its release manifest, and the logs.
    #[arg(short, long)]
    config: PathBuf,
    /// Accept a mock attestation and plain HTTP to loopback, for development against a mock node
    /// and a local stand-in log. Refused in a production build.
    #[arg(long)]
    allow_mock: bool,
}

/// A log to follow: its config and its client.
struct Followed {
    domain: String,
    section: LogSection,
    client: LogClient,
}

/// The watcher's clock, in milliseconds since the epoch; tests give their own.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The tracker and the file it is saved to, and the clock they are read by.
struct Store {
    tracker: Tracker,
    file: StateFile,
    clock: Clock,
}

/// What the watcher polls, and what it has learned.
struct Watcher {
    logs: Vec<Followed>,
    /// How long one log's read may take: its share of a quarter of the grace, between
    /// `MIN_LOG_BUDGET` and `MAX_LOG_BUDGET`. The attestation poll between logs is what keeps it on
    /// time, whatever the logs together take.
    log_budget: Duration,
    /// How long one attestation poll may take: half its interval, so the logs always get the rest.
    attestation_budget: Duration,
    /// When the next attestation poll is due.
    attestation_ticks: Interval,
    source: AttestationSource,
    store: Store,
}

/// Runs the watcher until a shutdown signal, or until a task it depends on stops, then delivers
/// the alerts already logged.
/// Error cases:
/// - `--allow-mock` in a production build, or the config, a webhook variable, the state file, the
///   release manifest or the alert outbox cannot be loaded or opened
/// - the state file cannot be written, or alert routing stops
pub async fn run(args: CtWatchArgs) -> anyhow::Result<()> {
    ensure!(!(args.allow_mock && cfg!(feature = "production")), "--allow-mock is refused in a production build");
    let cfg = WatchConfig::load(&args.config, args.allow_mock)?;
    let Telemetry { metrics, alerts, pages_critical } =
        init_worker_telemetry(&cfg.logging.level, &cfg.alerts.critical, &None, |var| std::env::var(var).ok())?;
    ensure_pages_critical(pages_critical, args.allow_mock)?;
    if args.allow_mock {
        warn!("--allow-mock: a mock attestation is accepted, and plain HTTP to loopback; development only");
    }
    let listen = cfg.metrics.listen;
    let listener = TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    info!(%listen, "metrics up");
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    // Ends an episode a previous run's `ct_watch_stopped` left open in the outbox.
    info!(alert_resolved = "ct_watch_stopped", "the watcher started");
    let outcome = tokio::select! {
        result = watch(&cfg, args.allow_mock) => result,
        stopped = routing.stopped() => Err(stopped),
        served = axum::serve(listener, metrics::router(metrics)) => {
            Err(served.err().map_or_else(|| anyhow::anyhow!("the metrics server stopped"), anyhow::Error::from))
        }
        () = shutdown_signal() => Ok(()),
    };
    if let Err(e) = &outcome {
        error!(alert = "ct_watch_stopped", "the watcher stopped and the domain is no longer watched: {e:#}");
    }
    routing.drain(SHUTDOWN_GRACE).await;
    outcome
}

/// Requires a Critical webhook, since a page is the watcher's only output; a mock run may log alone.
/// Error cases:
/// - there is no `alerts.critical` route and the run is not a mock one
fn ensure_pages_critical(pages_critical: bool, allow_mock: bool) -> anyhow::Result<()> {
    if !pages_critical {
        ensure!(allow_mock, "no alerts.critical webhook: nobody would be paged for an unattested certificate");
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    Ok(())
}

/// Resolves on ctrl-c or SIGTERM.
async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("installing the SIGTERM handler");
    tokio::select! {
        _ = ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutdown signal received");
}

/// Polls the node and the logs forever.
/// Error cases:
/// - the state file is locked, unreadable or cannot be written, or a client or the release manifest
///   cannot be built
async fn watch(cfg: &WatchConfig, allow_mock: bool) -> anyhow::Result<()> {
    let mut watcher = Watcher::new(cfg, allow_mock, Arc::new(now))?;
    // The first pass learns the attested key before it reads any log.
    watcher.poll_attestation().await;
    let mut log_reads = interval(cfg.log_interval);
    log_reads.set_missed_tick_behavior(MissedTickBehavior::Skip);
    info!(domain = %cfg.domain, logs = watcher.logs.len(), "certificate transparency watch started");
    loop {
        // An overdue attestation poll goes first, and `poll_logs` makes one between logs, so a backlog
        // cannot delay it past a grace. It is cut off at `attestation_budget`, so it cannot starve
        // the logs either.
        tokio::select! {
            biased;
            _ = watcher.attestation_ticks.tick() => watcher.poll_attestation().await,
            _ = log_reads.tick() => {
                // A log still behind its tree head is read again at once, not a tick later.
                if watcher.poll_logs().await? {
                    log_reads.reset_immediately();
                }
            }
        }
        watcher.settle()?;
    }
}

impl Watcher {
    /// Opens the state, builds the clients and loads the release manifest.
    /// Error cases:
    /// - the state file is locked, unreadable or cannot be written, or a client or the release
    ///   manifest cannot be built
    fn new(cfg: &WatchConfig, allow_mock: bool, clock: Clock) -> anyhow::Result<Self> {
        let (file, mut state) = StateFile::open(&cfg.state_file)?;
        if state == State::default() {
            state.allow_mock = allow_mock;
            state.domain = cfg.domain.clone();
        }
        ensure!(
            state.domain == cfg.domain,
            "{} was kept for {}, not {}; use a fresh state_file to watch another domain",
            cfg.state_file.display(),
            state.domain,
            cfg.domain
        );
        ensure!(
            state.allow_mock == allow_mock,
            "{} holds keys from {} attestations; use a fresh state_file to change that",
            cfg.state_file.display(),
            if state.allow_mock { "mock" } else { "verified" }
        );
        let http = benchmarks::client()?;
        let logs = cfg
            .logs
            .iter()
            .map(|log| {
                let url = endpoint_url(&log.url, allow_mock).with_context(|| format!("log {}", log.name))?;
                let key = LogKey::parse(&log.public_key).with_context(|| format!("log {}", log.name))?;
                Ok(Followed {
                    domain: cfg.domain.clone(),
                    section: log.clone(),
                    client: LogClient::new(http.clone(), url, key),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let source = AttestationSource::new(&cfg.node, allow_mock)?;
        let log_keys: Vec<(String, String)> =
            logs.iter().map(|log| (log.section.name.clone(), log.client.fingerprint().to_owned())).collect();
        let tracker = Tracker::new(state, &cfg.domain, cfg.grace, cfg.log_stale_after, log_keys, clock());
        let mut attestation_ticks = interval_at(Instant::now() + cfg.attestation_interval, cfg.attestation_interval);
        attestation_ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let shares = u32::try_from(logs.len()).unwrap_or(u32::MAX).max(1);
        let log_budget = (cfg.grace / 4 / shares).clamp(MIN_LOG_BUDGET, MAX_LOG_BUDGET);
        Ok(Self {
            logs,
            log_budget,
            attestation_budget: cfg.attestation_interval / 2,
            attestation_ticks,
            source,
            store: Store { tracker, file, clock },
        })
    }

    /// Fetches and verifies the node's attestation and records the TLS key it names. A node that
    /// does not answer names no key and wrongly names none; one whose attestation does not verify
    /// alerts.
    async fn poll_attestation(&mut self) {
        poll_attestation(&self.source, self.attestation_budget, &mut self.store).await;
    }

    /// Reads every log up to its tree head or as far as its time budget allows, and says whether a
    /// budget ran out with a log still behind. A failed read is logged and retried at the next poll;
    /// `ct_log_unreachable` is raised when reads keep failing.
    /// Error cases:
    /// - the state file cannot be written
    async fn poll_logs(&mut self) -> anyhow::Result<bool> {
        let mut behind = false;
        for followed in &self.logs {
            // Between logs, so a long pass over many logs cannot hold back the attestation poll.
            if self.attestation_ticks.tick().now_or_never().is_some() {
                poll_attestation(&self.source, self.attestation_budget, &mut self.store).await;
            }
            let name = followed.section.name.as_str();
            let read = timeout(self.log_budget, read_log(followed, &mut self.store));
            match read.instrument(info_span!("ct_log", id = name)).await {
                Ok(Ok(())) => {}
                Ok(Err(ReadError::Log(e))) => warn!(id = name, "log read failed: {e:#}"),
                Ok(Err(ReadError::State(e))) => return Err(e),
                // Cut off with the log still behind; a batch is saved whole or not at all.
                Err(_) => {
                    behind = true;
                    warn!(id = name, "log read used its {}s budget", self.log_budget.as_secs());
                }
            }
        }
        Ok(behind)
    }

    /// Decides the findings that are due, checks the logs are being read, and saves the state.
    /// Error cases:
    /// - the state file cannot be written
    fn settle(&mut self) -> anyhow::Result<()> {
        let now = (self.store.clock)();
        self.store.tracker.settle(now);
        self.store.tracker.check_logs(now);
        self.store.tracker.check_attestation(now);
        self.store.persist()
    }
}

/// Why a log read stopped.
enum ReadError {
    /// The log answered badly or not at all.
    Log(anyhow::Error),
    /// The watcher's own state could not be saved.
    State(anyhow::Error),
}

/// Reads `followed` up to its signed tree head, batch by batch. A
/// batch's findings and the new position are saved together, so a failed or cut-off batch is read
/// again whole.
async fn read_log(followed: &Followed, store: &mut Store) -> Result<(), ReadError> {
    let name = followed.section.name.as_str();
    let head = followed.client.tree_head().await.map_err(ReadError::Log)?;
    // A head dated ahead of this clock could be replayed for as long as it stays ahead.
    if head.timestamp_ms > (store.clock)().saturating_add(MAX_HEAD_SKEW_MS) {
        return Err(ReadError::Log(anyhow::anyhow!(
            "the tree head is dated {} ms ahead of this clock",
            head.timestamp_ms
        )));
    }
    let mut next = store.tracker.next_index(name, head.tree_size, followed.section.start_index);
    if head.tree_size < next {
        let shrunk =
            anyhow::anyhow!("the log's tree has {} entries, fewer than the {next} already read", head.tree_size);
        return Err(ReadError::Log(shrunk));
    }
    while next < head.tree_size {
        let end = (head.tree_size - 1).min(next.saturating_add(MAX_BATCH - 1));
        let leaves = followed.client.leaves(next, end).await.map_err(ReadError::Log)?;
        let now = (store.clock)();
        let certificates: Vec<Option<Certificate>> = leaves
            .iter()
            .enumerate()
            .map(|(offset, leaf)| entry_certificate(leaf, &followed.domain, next + offset as u64, now))
            .collect();
        if store.tracker.lacks_room_for(certificates.iter().flatten(), now) {
            let full = anyhow::anyhow!("too many open findings to read entries {next} onward; the log waits");
            return Err(ReadError::Log(full));
        }
        for (offset, certificate) in certificates.iter().enumerate() {
            if let Some(certificate) = certificate {
                store.tracker.ingest(name, next + offset as u64, certificate, now);
            }
        }
        next += certificates.len() as u64;
        store.tracker.advance(name, next, None, now);
        store.persist().map_err(ReadError::State)?;
    }
    // The log counts as read as of the head's signed time, so a replayed old head does not keep it healthy.
    store.tracker.advance(name, next, Some(head.timestamp_ms), (store.clock)());
    store.persist().map_err(ReadError::State)?;
    Ok(())
}

/// Polls the node's attestation for at most `budget` and records the TLS key a verified one names.
/// A node that does not answer names no key and wrongly names none; one whose attestation does not
/// verify alerts.
async fn poll_attestation(source: &AttestationSource, budget: Duration, store: &mut Store) {
    let polled = match timeout(budget, source.poll()).await {
        Ok(polled) => polled,
        Err(_) => Poll::Unreachable(anyhow::anyhow!("the poll used its {}s budget", budget.as_secs())),
    };
    match polled {
        Poll::Verified(key) => {
            let now = (store.clock)();
            store.tracker.attested(&key, now);
            info!(alert_resolved = "attestation_unverified", "the node's attestation verifies");
        }
        Poll::Unreachable(e) => warn!("the node's attestation is unavailable: {e:#}"),
        Poll::Rejected(e) => {
            error!(alert = "attestation_unverified", "the node's attestation does not verify: {e:#}")
        }
    }
}

/// The certificate in a log entry. An entry that does not parse is skipped and logged, unless it
/// mentions the domain, in which case it stands in as a certificate for the domain with no known
/// key: a log must not be blinded by one malformed entry, nor hide a certificate in it.
fn entry_certificate(leaf: &[u8], domain: &str, index: u64, now: u64) -> Option<Certificate> {
    match parse_leaf(leaf) {
        Ok(certificate) => Some(certificate),
        Err(e) => {
            let suspect = suspect_unparsed(leaf, domain, now);
            warn!(index, suspect = suspect.is_some(), "log entry does not parse: {e:#}");
            suspect
        }
    }
}

impl Store {
    /// Saves the state if it changed.
    /// Error cases:
    /// - the state file cannot be written
    fn persist(&mut self) -> anyhow::Result<()> {
        if self.tracker.take_changed() {
            self.file.save(&self.tracker.state)?;
        }
        Ok(())
    }
}

/// Watcher milliseconds since the epoch.
fn now() -> u64 {
    u64::try_from(now_ms()).expect("the clock is after the epoch")
}
