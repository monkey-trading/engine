//! Node wiring: obtain and attest the enclave key, the config, and the API's TLS key (`keys`), wait
//! for the vault to name the key (`registration`), recover the engine from its log and the history
//! index, then run the sequencer, ticker, price feeds, custody, trusted time check, history index,
//! API, rate-limit upkeep, monitor, metrics listener, and alert routing as supervised tasks. Any
//! task stopping stops the node. On SIGTERM or ctrl-c, every task but the sequencer is stopped; the
//! sequencer then drains what is queued and writes a final checkpoint, and the alerts already logged
//! are delivered.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{interval, timeout};
use tracing::{error, info, warn};

use crate::alerts::Routing;
use crate::api::{ApiState, Feed, MAX_BACKFILLS, Signing, router};
use crate::clock::{Clock, SystemClock, now_ms};
use crate::config::{AuthMode, CustodyProvider, FeedProvider, NodeConfig};
use crate::feeds;
use crate::feeds::venue::{self, Timing};
use crate::history::HistoryPlan;
mod shutdown;
use shutdown::{TaskResult, finish_history, handed_over, sequencer_outcome, supervise};
mod keys;
mod marks;
pub use keys::{KeyStart, parse_launch_digest};
mod preflight;
mod registration;
mod signer_history;
use crate::limits::{ApiLimits, run_retain};
use crate::log::Manifest;
use crate::metrics::Metrics;
use crate::sequencer::{ReplayedLog, Request, Sequencer, SequencerConfig, genesis_commands};
use crate::telemetry::Telemetry;
use crate::tls::acme::{CertificateTask, Sealed};
use crate::tls::serve;
use crate::{custody::Custody, metrics, monitor, trusted_time, verify::Relay};
use preflight::{authorize, emission_preflight};

/// How long the sequencer gets to drain and checkpoint after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize = 65_536;

/// Runs the node until a shutdown signal, or until the first supervised task stops, then delivers
/// the alerts already logged, for up to `SHUTDOWN_GRACE`.
/// Error cases:
/// - see `serve_node`
pub async fn run(cfg: NodeConfig, telemetry: Telemetry) -> anyhow::Result<()> {
    run_with_cpu(cfg, telemetry, None, KeyStart::default()).await
}

/// Runs the node with the CPU reserved before its runtime was created, and the operator's choice of
/// keys for a release that has none sealed.
/// Error cases:
/// - see `run`
pub(crate) async fn run_with_cpu(
    cfg: NodeConfig,
    telemetry: Telemetry,
    cpu: Option<usize>,
    start: KeyStart,
) -> anyhow::Result<()> {
    let Telemetry { metrics, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    cfg.alerts.outbox.check_disk_location(&cfg.node.log_dir)?;
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    let outcome = serve_node(cfg, metrics, &mut routing, cpu, start).await;
    routing.drain(SHUTDOWN_GRACE).await;
    outcome
}

/// Runs every task but alert routing, which it supervises.
/// Error cases:
/// - operator rotation is enabled
/// - `feeds.provider = "venues"` without `[feeds.venues]`, an API key variable a venue connection
///   names is unset, or the TLS config or the ACME client cannot be built
/// - obtaining the keys (see `keys::attested`), opening the history index (or a fresh one in place
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
    start: KeyStart,
) -> anyhow::Result<()> {
    if cfg.operator.rotation_enabled {
        bail!("operator rotation is not implemented yet; set operator.rotation_enabled = false");
    }
    if cfg.node.auth == AuthMode::None {
        warn!("API authentication is OFF: development only");
    }

    let keys = keys::attested(&cfg, &start).await?;
    let keys::Keys { enclave: key, tls, store, attestor, attestation } = keys;

    // The relay fetches in the background, so the API serves the attestation while it is pending.
    let relay = Relay::new(&cfg.tee, &attestation)
        .inspect_err(|e| error!(alert = "collateral_relay_failed", "refusing to serve: {e:#}"))?;
    let served = relay.as_ref().map(Relay::served);
    if relay.is_none() {
        info!(provider = %attestation.provider, "no attestation collateral to relay for this provider");
    }

    let checkpoint_setup = registration::registered(&cfg, &key, &tls.acceptors, &attestation)
        .await
        .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    let mut anchor = None;
    let authorities = signer_history::load(checkpoint_setup.as_ref(), &cfg.node.log_dir)
        .await
        .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?;
    // The marks cache opens before recovery, which seeds it through the sink as it verifies the log.
    let marks_failed = |e: &anyhow::Error| error!(alert = "marks_cache_failed", "refusing to serve: {e:#}");
    let marks = marks::startup(&cfg).inspect_err(marks_failed)?;
    let (mut log, history_plan) = read_log_and_history_authorities(
        &cfg,
        Some(AccountId(key.address())),
        checkpoint_setup.as_ref().map(|setup| setup.parent),
        authorities,
        &marks.shared,
        |entry| {
            if let crate::log::LogEntry::Checkpoint(cp) = entry
                && checkpoint_setup.as_ref().and_then(|setup| setup.posted_seq) == Some(cp.seq)
            {
                anchor = Some(cp.clone());
            }
        },
    )?;
    keys::check_predecessor_head(store.as_deref(), log.last_seq(), log.last_checkpoint())
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
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
    let (signing_tx, signing_rx) = watch::channel(true);
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
        checkpoint_signing: Some(signing_tx),
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
    let published = crate::handover::Published::new(stream_tx.subscribe(), log.last_seq(), log.last_checkpoint());
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

    // The sequencer reports live mark changes; a supervised task writes them and backfills history.
    let marks_rx = marks::track(&mut seq);
    let marks_store = marks::into_task_store(marks.shared);

    let (tx, rx) = mpsc::channel::<Request>(CHANNEL_CAPACITY);
    let recovery = seq.take_recovery_checkpoints();
    let sequencer = crate::sequencer_thread::Worker::start(seq.run(rx), cpu)?;
    let mut tasks: JoinSet<TaskResult> = JoinSet::new();
    tasks.spawn(marks::task(marks_store, marks_rx, marks.backfill, marks.sources, tx.downgrade()));
    if let Some(setup) = checkpoint_setup {
        let checkpoint_tx = tx.clone();
        tasks.spawn(async move {
            ("checkpoints", setup.poster.run_with_approvals(setup.rx, checkpoint_tx, recovery, setup.approvals).await)
        });
    }

    // A successor's request stops sequencing through `drain_rx`, once the vault approves it; the
    // stop itself goes to the sequencer as a final-checkpoint request, which refuses while the
    // checkpoint gate would withhold the checkpoint.
    let (samples, samples_rx) = watch::channel(None);
    let trusted = || trusted_time::TrustedClock::new(clock.clone(), samples_rx.clone(), cfg.time.max_drift);
    let (drain_tx, mut drain_rx) = mpsc::channel(1);
    let handover_stop_tx = tx.clone();
    let mut handover = JoinSet::new();
    if let Some(store) = &store
        && let Some(predecessor) =
            crate::handover::Predecessor::bind(&cfg, store, attestor, &key, trusted(), drain_tx, signing_rx).await?
    {
        handover.spawn(predecessor.run());
    }
    if let Some(store) = &store
        && let Some(vault) = crate::handover::vault_reader(&cfg)?
    {
        let (release, generation) = (store.release(), store.generation());
        let every = keys::RELEASE_CHECK_EVERY;
        tasks.spawn(async move { ("release", keys::watch_release(vault, release, generation, every).await) });
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

    let (trusted_clock, sealed) = (trusted(), store.clone().map(|store| Sealed { store, trusted: trusted() }));
    let time_cfg = cfg.time;
    let time_tx = tx.clone();
    tasks.spawn(async move { ("trusted_time", trusted_time::run(time_cfg, clock, time_tx, samples).await) });

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
    let history_finished = false;
    let handing_over = None;

    let monitor_tx = tx.clone();
    let (monitor_metrics, no_price_after) = (metrics.clone(), cfg.alerts.no_price_after);
    tasks.spawn(async move { ("monitor", monitor::run(monitor_tx, monitor_metrics, no_price_after, sources).await) });

    let metrics_listen = cfg.metrics.listen;
    let metrics_listener =
        TcpListener::bind(metrics_listen).await.with_context(|| format!("binding {metrics_listen}"))?;
    info!(listen = %metrics_listener.local_addr()?, "metrics up");
    let certificates = CertificateTask::configured(&cfg.api, tls.key, &metrics, sealed)?;
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
        marks: marks.reader,
        emission_budget,
        collateral: served,
        listing: Default::default(),
        signing: Arc::new(Signing::new(key, cfg.custody.chain_id, vault, cfg.api.heartbeat_interval, trusted_clock)),
    });
    info!(listen = %listener.local_addr()?, "api up over TLS");
    // After the bind, so the CA's validation connections find the listener.
    if let Some(certificates) = certificates {
        tasks.spawn(async move { ("certificate", certificates.run().await) });
    }
    let (api, retained) = (cfg.api, limits.clone());
    tasks.spawn(async move { ("api", serve(listener, tls.acceptors, app, limits, api).await) });
    tasks.spawn(async move { ("rate limits", run_retain(retained).await) });

    let mut supervised = supervise(
        shutdown::Supervised {
            tasks,
            history_task,
            history_finished,
            sequencer,
            handover,
            handing_over,
            outcome: Ok(()),
        },
        routing,
        &mut drain_rx,
        &handover_stop_tx,
    )
    .await;
    // The sequencer's drain waits for every request sender to drop: this one is spent.
    drop(handover_stop_tx);
    // Stopping the other tasks drops every sender, so the sequencer drains and checkpoints.
    supervised.tasks.shutdown().await;
    let drained =
        timeout(SHUTDOWN_GRACE, &mut supervised.sequencer.outcome).await.context("sequencer did not drain in time")?;
    sequencer_outcome(drained)?;
    if !supervised.history_finished {
        finish_history(supervised.history_task, history_stopper).await;
    }
    let outcome = supervised.outcome;
    match supervised.handing_over {
        Some(reply) => {
            let _ = reply.send(published.final_head());
            handed_over(supervised.handover).await
        }
        None => outcome,
    }
}

/// Replays with independently finalized historical signer evidence before any persisted mutation,
/// from a verified snapshot when snapshots are configured, seeding the marks cache through `marks`.
/// Error cases:
/// - history, log, authenticated signer history, or the marks cache fails validation
fn read_log_and_history_authorities(
    cfg: &NodeConfig,
    signer: Option<AccountId>,
    parent: Option<crate::log::CheckpointParent>,
    authorities: crate::checkpoints::SignerHistory,
    marks: &std::rc::Rc<std::cell::RefCell<crate::history::marks::Store>>,
    mut observe: impl FnMut(&crate::log::LogEntry),
) -> anyhow::Result<(ReplayedLog, HistoryPlan)> {
    if cfg.snapshots.is_some() {
        // Snapshot recovery replays the authenticated suffix only; the marks cache seeds from that
        // suffix, and the provider backfill gives the depth the suffix cannot.
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
        Some(marks::recovery_sink(marks)),
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

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod paired_tests;
