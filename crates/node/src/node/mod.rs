//! Node wiring: attest the enclave key, the config, and the API's TLS key, recover the engine from
//! its log and the history index, then run the sequencer, ticker, price feeds, custody, trusted time
//! check, history index, API, rate-limit upkeep, monitor, metrics listener, and alert routing as
//! supervised tasks. Any task stopping stops the node. On SIGTERM or ctrl-c, every task but the
//! sequencer is stopped; the sequencer then drains what is queued and writes a final checkpoint,
//! and the alerts already logged are delivered.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::net::TcpListener;
use tokio::signal::ctrl_c;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::{JoinError, JoinSet};
use tokio::time::{interval, timeout};
use tracing::{error, info, warn};

use crate::alerts::Routing;
use crate::api::{ApiState, Feed, MAX_BACKFILLS, Signing, router};
use crate::clock::{Clock, SystemClock, now_ms};
use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig, TeeProvider};
use crate::custody::Custody;
use crate::feeds;
use crate::feeds::venue::{self, Timing};
use crate::history::HistoryPlan;
mod preflight;
mod signer_history;
use crate::limits::{ApiLimits, run_retain};
use crate::log::Manifest;
use crate::metrics::Metrics;
use crate::sequencer::{ReplayedLog, Request, Sequencer, SequencerConfig, genesis_commands};
use crate::tee::{AttestationBundle, Attestor, ConfigfsTsm, EnclaveKey, MockAttestor, attest_bundle};
use crate::telemetry::Telemetry;
use crate::tls::{TlsIdentity, acme::CertificateTask, serve};
use crate::verify::Relay;
use crate::{metrics, monitor, trusted_time};
use preflight::{authorize, emission_preflight};

/// How long the sequencer gets to drain and checkpoint after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize = 65_536;
type TaskResult = (&'static str, anyhow::Result<()>);

/// Runs the node until a shutdown signal, or until the first supervised task stops, then delivers
/// the alerts already logged, for up to `SHUTDOWN_GRACE`.
/// Error cases:
/// - see `serve_node`
pub async fn run(cfg: NodeConfig, telemetry: Telemetry) -> anyhow::Result<()> {
    run_with_cpu(cfg, telemetry, None).await
}

/// Runs the node with the CPU reserved before its runtime was created.
/// Error cases:
/// - see `run`
pub(crate) async fn run_with_cpu(cfg: NodeConfig, telemetry: Telemetry, cpu: Option<usize>) -> anyhow::Result<()> {
    let Telemetry { metrics, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    cfg.alerts.outbox.check_disk_location(&cfg.node.log_dir)?;
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    let outcome = serve_node(cfg, metrics, &mut routing, cpu).await;
    routing.drain(SHUTDOWN_GRACE).await;
    outcome
}

/// Runs every task but alert routing, which it supervises.
/// Error cases:
/// - operator rotation is enabled
/// - `feeds.provider = "venues"` without `[feeds.venues]`, an API key variable a venue connection
///   names is unset, or the TLS config or the ACME client cannot be built
/// - generating the TLS key, attesting the keys, opening the history index (or a fresh one in place
///   of an index the log does not match), recovering the log (including a log that records another
///   client auth or operator, or whose genesis did not apply in full, is not the config's, or is one
///   no current config can give), connecting custody, or binding a listener fails
/// - the engine would reject a genesis command
/// - with checkpoints configured, the vault's MONKEY cap cannot be read, or it was derived from
///   another emission period budget than the one the log records
/// - any supervised task stops or panics, or the sequencer cannot drain in time
async fn serve_node(
    cfg: NodeConfig,
    metrics: Arc<Metrics>,
    routing: &mut Routing,
    cpu: Option<usize>,
) -> anyhow::Result<()> {
    if cfg.operator.rotation_enabled {
        bail!("operator rotation is not implemented yet; set operator.rotation_enabled = false");
    }
    if cfg.node.auth == AuthMode::None {
        warn!("API authentication is OFF: development only");
    }

    let attestor: Box<dyn Attestor> = match cfg.tee.provider {
        TeeProvider::Mock => {
            warn!("mock TEE: attestation reports are not backed by hardware");
            Box::new(MockAttestor)
        }
        TeeProvider::ConfigfsTsm => Box::new(ConfigfsTsm::default()),
    };
    let key = Arc::new(
        EnclaveKey::for_config(&cfg.tee)
            .inspect_err(|e| error!(alert = "enclave_key_failed", "refusing to serve: {e:#}"))?,
    );
    if cfg.tee.use_published_mock_key {
        warn!("using the public development signing key: mock attestation only");
    }
    let tls = TlsIdentity::generate()?;
    let bundle = AttestationBundle {
        enclave_address: key.address(),
        config_sha256: cfg.file_sha256,
        tls_public_key_sha256: tls.public_key_sha256,
    };
    let attestation = Arc::new(attest_bundle(attestor.as_ref(), &bundle).context("attesting the enclave")?);
    info!(
        enclave = %key.address_hex(),
        config_sha256 = %attestation.config_sha256,
        tls_public_key_sha256 = %attestation.tls_public_key_sha256,
        provider = %attestation.provider,
        "enclave, config, and TLS key attested"
    );

    // The relay fetches in the background, so the API serves the attestation while it is pending.
    let relay = Relay::new(&cfg.tee, &attestation)
        .inspect_err(|e| error!(alert = "collateral_relay_failed", "refusing to serve: {e:#}"))?;
    let served = relay.as_ref().map(Relay::served);
    if relay.is_none() {
        info!(provider = %attestation.provider, "no attestation collateral to relay for this provider");
    }

    let checkpoint_setup = crate::checkpoints::setup::connect(&cfg, &key)
        .await
        .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    let mut anchor = None;
    let authorities = signer_history::load(checkpoint_setup.as_ref(), &cfg.node.log_dir)
        .await
        .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    let (mut log, history_plan) = read_log_and_history_authorities(
        &cfg,
        Some(AccountId(key.address())),
        checkpoint_setup.as_ref().map(|setup| setup.parent),
        authorities,
        |entry| {
            if let crate::log::LogEntry::Checkpoint(cp) = entry
                && checkpoint_setup.as_ref().and_then(|setup| setup.posted_seq) == Some(cp.seq)
            {
                anchor = Some(cp.clone());
            }
        },
    )?;
    log.prove_legacy_withdrawals(&cfg.custody)
        .await
        .inspect_err(|e| error!(alert = "legacy_withdrawal_unproven", "refusing writable recovery: {e:#}"))?;

    // The API also refuses an emission schedule with another budget than the vault's.
    let emission_budget = match &checkpoint_setup {
        Some(setup) => Some(emission_preflight(&setup.poster, log.emissions()).await?),
        None => None,
    };

    let vault: AccountId = cfg.custody.vault.parse().context("custody.vault")?;
    let client_auth = cfg.client_auth()?;
    let operator = cfg.operator()?;
    let access = cfg.access.as_ref().map(crate::access::AccessClient::new).transpose()?.map(Arc::new);
    let (stream_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
    let (withdrawals_tx, withdrawals_rx) = mpsc::unbounded_channel();
    let (manifest_tx, manifest_rx) = watch::channel(Manifest::default());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut seq_cfg = SequencerConfig {
        access_policy: cfg.access.as_ref().map(crate::config::AccessSection::policy),
        log_dir: &cfg.node.log_dir,
        segment_commands: cfg.node.segment_commands,
        manifest: manifest_tx,
        checkpoint_every: cfg.node.checkpoint_every,
        checkpoint_registry: checkpoint_setup.as_ref().and_then(|setup| setup.poster.registry.clone()),
        approval_registry: checkpoint_setup.as_ref().map(|setup| setup.approvals.clone()),
        checkpoint_parent: checkpoint_setup.as_ref().map(|setup| setup.parent).unwrap_or_default(),
        checkpoints: checkpoint_setup.as_ref().map(|setup| setup.tx.clone()),
        chain_id: cfg.custody.chain_id,
        vault,
        clock: clock.clone(),
        max_drift: cfg.time.max_drift,
        client_auth: client_auth.clone(),
        operator,
        metrics: metrics.clone(),
        genesis: genesis_commands(&cfg.genesis),
        history_sealing: None,
    };
    let custody = Custody::connect(&cfg.custody, log.last_seq())
        .inspect_err(|e| error!(alert = "custody_connect_failed", "refusing to serve: {e:#}"))?
        .with_access(access.clone());
    authorize(
        checkpoint_setup.as_ref().map(|setup| (&setup.poster, anchor.as_ref())),
        Some((&log, &seq_cfg, AccountId(key.address()))),
    )
    .await?;
    let (log, mut history) = history_plan
        .finish_with_log(log, key.clone())
        .inspect_err(|e| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}"))?;
    seq_cfg.history_sealing = history.sealing();
    let mut seq = Sequencer::recover_from(log, seq_cfg, key.clone(), stream_tx.clone(), withdrawals_tx)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    if cfg.benchmarks.is_some() {
        seq.wait_for_benchmarks()
            .inspect_err(|e| error!(alert = "benchmark_sync_prerequisites_missing", "refusing to serve: {e:#}"))?;
    }
    // Protocol activation and genesis wait, like any command, for a stamp trusted time admits;
    // with no sample yet, they are queued. Parameters an older log set take the config's toxicity
    // notional weighting first, which the latest version needs.
    let weighting = cfg.genesis.params.toxicity.notional.context("genesis.params.toxicity.notional")?;
    seq.carry_toxicity_notional(weighting)
        .and_then(|()| seq.activate_protocol())
        .inspect_err(|e| error!(alert = "protocol_activation_failed", "refusing to serve: {e:#}"))?;
    if seq.needs_genesis() {
        info!(markets = cfg.genesis.markets.len(), "genesis queued for the first trusted time sample");
        seq.genesis(&cfg.genesis).inspect_err(|e| error!(alert = "genesis_refused", "refusing to serve: {e:#}"))?;
    }
    history.catch_up_to(seq.last_seq());

    let (tx, rx) = mpsc::channel::<Request>(CHANNEL_CAPACITY);
    let recovery = seq.take_recovery_checkpoints();
    let mut sequencer = crate::sequencer_thread::Worker::start(seq.run(rx), cpu)?;
    let mut tasks: JoinSet<TaskResult> = JoinSet::new();
    if let Some(setup) = checkpoint_setup {
        let checkpoint_tx = tx.clone();
        tasks.spawn(async move {
            ("checkpoints", setup.poster.run_with_approvals(setup.rx, checkpoint_tx, recovery, setup.approvals).await)
        });
    }

    let tick = cfg.node.tick;
    let ticks = tx.clone();
    tasks.spawn(async move { ("ticker", run_ticker(ticks, tick).await) });
    if let Some(relay) = relay {
        tasks.spawn(async move { ("collateral", relay.run().await) });
    }

    // The monitor exports quote times only for the sources this config still feeds.
    let sources = feeds::configured_sources(&cfg.feeds);
    let feeds = cfg.feeds;
    match feeds.provider {
        FeedProvider::Mock => {
            let feed_tx = tx.clone();
            tasks.spawn(async move {
                feeds::run_mock(&feeds, feed_tx).await;
                ("feeds", Ok(()))
            });
        }
        FeedProvider::Venues => {
            let venues = feeds.venues.context("feeds.provider = \"venues\" needs [feeds.venues]")?;
            let timing = Timing::new(feeds.interval, &venues);
            let tls = venue::tls_config()?;
            for conn in venues.connections {
                let (name, tls, feed_tx) = (conn.venue.name(), tls.clone(), tx.clone());
                let api_key = conn.api_key(|var| std::env::var(var).ok())?;
                info!(venue = name, url = %conn.url, symbols = conn.symbols.len(), "starting venue feed");
                tasks.spawn(async move { (name, venue::run(conn, api_key, timing, tls, feed_tx).await) });
            }
        }
    }

    let time_cfg = cfg.time;
    let time_tx = tx.clone();
    tasks.spawn(async move { ("trusted_time", trusted_time::run(time_cfg, clock, time_tx).await) });

    let mock_custody = cfg.custody.provider == CustodyProvider::Mock;
    let custody_tx = tx.clone();
    tasks.spawn(async move { ("custody", custody.run(withdrawals_rx, custody_tx).await) });

    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    let history_reader = history.reader();
    let history_feed = Feed::new(
        stream_tx.subscribe(),
        cfg.node.log_dir.clone(),
        manifest_rx.clone(),
        backfills.clone(),
        Some(history.resume_at()),
    );
    // History runs outside `tasks`: on shutdown it outlives the sequencer, so it can take the final
    // checkpoint's capture, and stops at the end of the stream. Its own set still aborts it if this
    // function returns early.
    let history_stopper = history.stopper();
    let mut history_task = JoinSet::new();
    history_task.spawn(history.run(history_feed));
    let mut history_finished = false;

    let monitor_tx = tx.clone();
    let (monitor_metrics, no_price_after) = (metrics.clone(), cfg.alerts.no_price_after);
    tasks.spawn(async move { ("monitor", monitor::run(monitor_tx, monitor_metrics, no_price_after, sources).await) });

    let metrics_listen = cfg.metrics.listen;
    let metrics_listener =
        TcpListener::bind(metrics_listen).await.with_context(|| format!("binding {metrics_listen}"))?;
    info!(listen = %metrics_listen, "metrics up");
    let certificates = CertificateTask::configured(&cfg.api, tls.key, &metrics)?;
    let metrics_app = metrics::router(metrics);
    tasks.spawn(async move { ("metrics", axum::serve(metrics_listener, metrics_app).await.context("metrics server")) });

    let listener = TcpListener::bind(cfg.node.listen).await.with_context(|| format!("binding {}", cfg.node.listen))?;
    let limits = Arc::new(ApiLimits::new(&cfg.api));
    let app = router(ApiState {
        protocol: engine_core::protocol::ProtocolVersion::LATEST,
        tx,
        stream: stream_tx,
        attestation,
        auth: client_auth,
        access,
        operator,
        mock_custody,
        payouts_enabled: mock_custody || cfg.checkpoints.is_some(),
        clock: now_ms,
        log_dir: cfg.node.log_dir.clone(),
        manifest: manifest_rx,
        backfills,
        limits: limits.clone(),
        history: history_reader,
        emission_budget,
        collateral: served,
        listing: Default::default(),
        signing: Arc::new(Signing::new(key, cfg.custody.chain_id, vault, cfg.api.heartbeat_interval)),
    });
    info!(listen = %cfg.node.listen, "api up over TLS");
    // After the bind, so the CA's validation connections find the listener.
    if let Some(certificates) = certificates {
        tasks.spawn(async move { ("certificate", certificates.run().await) });
    }
    let (api, retained) = (cfg.api, limits.clone());
    tasks.spawn(async move { ("api", serve(listener, tls.acceptors, app, limits, api).await) });
    tasks.spawn(async move { ("rate limits", run_retain(retained).await) });

    let outcome = tokio::select! {
        joined = tasks.join_next() => Err(stopped_task(joined)),
        joined = history_task.join_next() => {
            history_finished = true;
            Err(stopped_task(joined.map(|joined| joined.map(|result| ("history", result)))))
        }
        err = routing.stopped() => {
            error!(alert = "task_stopped", "{err:#}");
            Err(err)
        }
        joined = &mut sequencer.outcome => {
            tasks.shutdown().await;
            history_task.abort_all();
            sequencer_outcome(joined)?;
            bail!("sequencer stopped");
        }
        signal = shutdown_signal() => signal.map(|()| info!("shutting down")),
    };
    // Stopping the other tasks drops every sender, so the sequencer drains and checkpoints.
    tasks.shutdown().await;
    let drained = timeout(SHUTDOWN_GRACE, &mut sequencer.outcome).await.context("sequencer did not drain in time")?;
    sequencer_outcome(drained)?;
    if !history_finished {
        finish_history(history_task, history_stopper).await;
    }
    outcome
}

/// Ends history once the sequencer has drained. Authenticated history takes the rest of the
/// stream; the legacy index takes no captures and resumes from its own cursor, so it stops at once.
async fn finish_history(mut history: JoinSet<anyhow::Result<()>>, stopper: Option<crate::history::runtime::Stopper>) {
    match stopper {
        Some(stopper) => drain_history(history, stopper).await,
        None => history.abort_all(),
    }
}

/// Lets history take the rest of the stream, including the shutdown checkpoint's capture. A
/// history still behind after the shutdown grace stops at its next turn boundary, with a Warning,
/// so no store is left mid-transaction; the next start indexes the rest from the log.
async fn drain_history(mut history: JoinSet<anyhow::Result<()>>, stopper: crate::history::runtime::Stopper) {
    let finished = match timeout(SHUTDOWN_GRACE, history.join_next()).await {
        Ok(finished) => finished,
        Err(_) => {
            warn!(alert = "history_drain_timed_out", "history did not finish the stream in time; stopping it");
            stopper.stop();
            match timeout(SHUTDOWN_GRACE, history.join_next()).await {
                Ok(finished) => finished,
                Err(_) => {
                    error!(alert = "task_stopped", "history did not stop in time");
                    history.abort_all();
                    return;
                }
            }
        }
    };
    match finished {
        None | Some(Ok(Ok(()))) => {}
        Some(Ok(Err(e))) => error!(alert = "task_stopped", "history failed while draining: {e:#}"),
        Some(Err(e)) => error!(alert = "task_stopped", "history panicked while draining: {e}"),
    }
}

/// Recovers before any append, checking history against that read during full replay or against
/// its cursor's segment after authenticated snapshot recovery. A mismatched index is rebuilt.
/// Error cases:
/// - the history index cannot be opened, nor a fresh one in its place (see `HistoryCheck`)
/// - the log cannot be read or replayed (see `Sequencer::read_log`)
#[cfg(test)]
fn read_log_and_history(cfg: &NodeConfig) -> anyhow::Result<(ReplayedLog, crate::history::History)> {
    let (log, plan) =
        read_log_and_history_observe(cfg, Some(AccountId(EnclaveKey::test_key().address())), None, |_| {})?;
    plan.finish_with_log(log, Arc::new(EnclaveKey::test_key()))
}

/// Replays and checks history while exposing the same entries for the on-chain anchor check.
/// Error cases:
/// - the log or history index cannot be read or verified
#[cfg(test)]
fn read_log_and_history_observe(
    cfg: &NodeConfig,
    signer: Option<AccountId>,
    parent: Option<crate::log::CheckpointParent>,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan)> {
    read_log_and_history_authorities(cfg, signer, parent, Default::default(), &mut observe)
}

/// Replays with independently finalized historical signer evidence before any persisted mutation,
/// from a verified snapshot when snapshots are configured.
/// Error cases:
/// - history, log, or authenticated signer history fails validation
fn read_log_and_history_authorities(
    cfg: &NodeConfig,
    signer: Option<AccountId>,
    parent: Option<crate::log::CheckpointParent>,
    authorities: crate::checkpoints::SignerHistory,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan)> {
    if cfg.snapshots.is_some() {
        let (log, plan) = Sequencer::read_paired_log(cfg, signer, parent, authorities, observe)
            .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
        return Ok((log, HistoryPlan::authenticated(plan)?));
    }
    let history_failed = |e: &anyhow::Error| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}");
    let mut check = HistoryPlan::open(&cfg.history).inspect_err(history_failed)?;
    let log = Sequencer::read_log_with_authorities(
        &cfg.node.log_dir,
        Some(crate::log::CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? }),
        signer,
        parent,
        authorities,
        |entry| {
            check.observe(entry);
            observe(entry);
        },
    )
    .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    Ok((log, check))
}

/// Sequences a `Tick` every `every`, so sessions and rollovers fire without other traffic.
/// Error cases:
/// - the sequencer has gone away
async fn run_ticker(tx: mpsc::Sender<Request>, every: Duration) -> anyhow::Result<()> {
    let mut ticks = interval(every);
    loop {
        ticks.tick().await;
        tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.map_err(|_| anyhow!("sequencer gone"))?;
    }
}

/// Resolves on SIGTERM or ctrl-c.
/// Error cases:
/// - a signal handler cannot be installed
async fn shutdown_signal() -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    tokio::select! {
        r = ctrl_c() => r.context("listening for ctrl-c"),
        _ = term.recv() => Ok(()),
    }
}

/// The error for a supervised task that stopped, raised as a critical alert.
fn stopped_task(joined: Option<Result<TaskResult, JoinError>>) -> anyhow::Error {
    let err = match joined {
        Some(Ok((name, Ok(())))) => anyhow!("{name} task stopped"),
        Some(Ok((name, Err(e)))) => e.context(format!("{name} task failed")),
        Some(Err(e)) => anyhow!(e).context("task panicked"),
        None => anyhow!("no tasks running"),
    };
    error!(alert = "task_stopped", "{err:#}");
    err
}

/// The sequencer's own result, or its panic.
/// Error cases:
/// - the sequencer panicked, or returned an error (a log write failure)
fn sequencer_outcome(joined: Result<anyhow::Result<()>, tokio::sync::oneshot::error::RecvError>) -> anyhow::Result<()> {
    joined.context("sequencer thread panicked")?.inspect_err(|e| error!(alert = "sequencer_failed", "{e:#}"))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod paired_tests;
