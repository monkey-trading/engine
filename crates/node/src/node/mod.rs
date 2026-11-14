//! Node wiring: open or wait for its delivered secrets (`secrets`), obtain and attest the enclave key,
//! the config, and the API's TLS key (`keys`), wait for the vault to name the key (`registration`),
//! recover the engine from its log and the history
//! index, then run the sequencer, ticker, price feeds, custody, trusted time check, history index,
//! API, rate-limit upkeep, monitor, and alert routing as supervised tasks, beside the metrics
//! listener the process binds once (`run`). Any task stopping stops the node. On SIGTERM or ctrl-c, every task but the sequencer is stopped; the
//! sequencer then drains what is queued and writes a final checkpoint, and the alerts already logged
//! are delivered.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use engine_core::types::AccountId;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::alerts::Routing;
use crate::api::{ApiState, Feed, MAX_BACKFILLS, Signing, router};
use crate::clock::{Clock, SystemClock};
use crate::config::{AuthMode, CustodyProvider, NodeConfig};
use crate::feeds;
mod shutdown;
use shutdown::{TaskResult, finish_history, handed_over, sequencer_outcome, supervise};
mod handover;
mod keys;
mod liveness;
mod marks;
pub(crate) mod member;
use member::{Ended, Handed};
pub(crate) mod ready;
use ready::Start;
mod run;
pub(crate) use run::run_with_cpu;
mod price_feeds;
pub(crate) mod secrets;
mod ticker;
mod turn_keys;
pub use keys::{KeyStart, parse_launch_digest};
use ticker::run_ticker;
mod preflight;
mod recovery;
use recovery::Recovered;
#[cfg(test)]
use recovery::read_log_and_history_authorities;
mod registration;
mod signer_history;
use crate::limits::{ApiLimits, run_retain};
use crate::log::Manifest;
use crate::metrics::Metrics;
use crate::sequencer::turns::TurnSealing;
use crate::sequencer::{Request, Sequencer, SequencerConfig, genesis_commands};
use crate::telemetry::Telemetry;
use crate::tls::acme::{CertificateTask, Sealed};
use crate::tls::serve;
use crate::{custody::Custody, monitor, trusted_time, verify::Relay};
use preflight::{authorize, emission_preflight};

/// How long a start that failed with a delivered key in play stays up for its repair.
const FAILED_START_HOLD: Duration = if cfg!(test) { Duration::from_secs(5) } else { Duration::from_secs(300) };
/// How long the sequencer gets to drain and checkpoint after a shutdown signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const CHANNEL_CAPACITY: usize = 65_536;

/// Runs the node until a shutdown signal, or until the first supervised task stops, then delivers
/// the alerts already logged, for up to `SHUTDOWN_GRACE`.
/// Error cases:
/// - see `serve_node`
pub async fn run(cfg: NodeConfig, telemetry: Telemetry) -> anyhow::Result<()> {
    Box::pin(run_with_cpu(cfg, telemetry, None, KeyStart::default())).await
}

/// Runs every task but alert routing, which it supervises, from a member's `replica` when given, until
/// a shutdown signal or a successor's confirmed handover ends it, which it reports with what the
/// handover gave away. A member that `handed` keys over before opens none of them again.
/// Error cases:
/// - `operator.rotation_enabled = true` while the vault's registry has rotation off, or without
///   `[checkpoints]` EVM custody to read it
/// - `feeds.provider = "venues"` without `[feeds.venues]`, an API key variable a venue connection
///   names is unset, or the TLS config or the ACME client cannot be built
/// - opening the sealed certificate (see `keys::certificates`), obtaining the keys (see
///   `keys::attested`), opening the history index (or a fresh one in place of an index the log does
///   not match), recovering the log (including a log that records another
///   client auth or operator, or whose genesis did not apply in full, is not the config's, or is one
///   no current config can give), connecting custody, or binding a listener fails
/// - the engine would reject a genesis command
/// - with checkpoints configured, the vault's MONKEY cap cannot be read, or it was derived from
///   another emission period budget than the one the log records
/// - any supervised task stops or panics, or the sequencer cannot drain in time, none of them, once
///   the first read of the chain has set `chain_read`, a delivered key's doing
#[allow(clippy::too_many_arguments)]
async fn serve_node(
    cfg: &NodeConfig,
    metrics: Arc<Metrics>,
    routing: &mut Routing,
    cpu: Option<usize>,
    start: &KeyStart,
    delivered: &secrets::Delivered,
    chain_read: &mut bool,
    member_start: Option<Box<Start>>,
    handed: Option<Handed>,
) -> anyhow::Result<Ended> {
    // A member's start read the chain and sampled trusted time before it connected for the tail:
    // from the confirmation to its first command it reads neither (`ready`).
    let (replica, taken, mut ready) = match member_start.map(|start| *start) {
        Some(Start { replica, store, ready, moved }) => {
            info!(after = moved.after, entries = moved.entries, bytes = moved.bytes, "starting from the replica");
            (Some(replica), Some(store), Some(ready))
        }
        None => (None, None, None),
    };
    match ready {
        Some(_) => *chain_read = true,
        None => handover::check_rotation(cfg, chain_read).await?,
    }
    if cfg.node.auth == AuthMode::None {
        warn!("API authentication is OFF: development only");
    }
    // Before the keys, so a store that does not open refuses before a handover takes them.
    let certificates = match ready.as_mut() {
        Some(ready) => ready.certificates.take(),
        None => keys::certificates(cfg, start)?,
    };
    let keys = match taken {
        Some(store) => ready::keys(cfg, store)?,
        None => keys::attested(cfg, start, &metrics, handed.and_then(|handed| handed.reached)).await?,
    };
    let keys::Keys { enclave: key, tls, store, attestor, attestation } = keys;
    // A takeover's first start under the new signer cuts the log to the vault's finalized
    // anchor: checkpoints the outgoing key signed but the vault never accepted are published
    // history the standby kept, but once this key is the vault's signer those signatures are
    // unpostable, and writable recovery admits only what the vault accepted. A replica ends at the
    // checkpoint its predecessor signed with this key, and its start reads no log.
    if replica.is_none() {
        keys::cut_to_finalized_anchor(cfg, &key).await?;
    }

    // The relay fetches in the background, so the API serves the attestation while it is pending.
    let relay = Relay::new(&cfg.tee, &attestation)
        .inspect_err(|e| error!(alert = "collateral_relay_failed", "refusing to serve: {e:#}"))?;
    let served = relay.as_ref().map(Relay::served);
    if relay.is_none() {
        info!(provider = %attestation.provider, "no attestation collateral to relay for this provider");
    }

    let checkpoint_setup = match ready.as_mut() {
        Some(ready) => ready.setup.take(),
        None => registration::registered(cfg, &key, &tls.acceptors, &attestation, &metrics)
            .await
            .inspect_err(|e| error!(alert = "checkpoint_startup_failed", "refusing to serve: {e:#}"))?,
    };
    // The takeover cut runs once the vault names this key: a promoted node's first start began
    // while the outgoing signer was still installed, and only now is the unposted tail cut.
    if replica.is_none() {
        keys::cut_to_finalized_anchor(cfg, &key).await?;
    }
    // The marks cache opens before recovery, which seeds it through the sink as it verifies the log.
    let marks_failed = |e: &anyhow::Error| error!(alert = "marks_cache_failed", "refusing to serve: {e:#}");
    // A member's start from its replica leaves the cache's earlier history to the marks task.
    let marks = marks::startup(cfg, key.marks_key(), replica.is_some()).inspect_err(marks_failed)?;
    // The marks a member's replica folded while it followed, which the marks task merges first.
    let folded = replica.as_ref().map(|replica| replica.marks.take());
    let replica = replica.zip(ready.as_ref());
    let Recovered { mut log, history: history_plan, pending: log_heads_pending, anchor, stats } =
        recovery::recover(cfg, &key, checkpoint_setup.as_ref(), &marks, replica, &metrics).await?;
    keys::check_predecessor_head(store.as_deref(), log.last_seq(), log.last_checkpoint())
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    // Refuses a log the on-chain posted head does not hold (Critical `log_rollback` names the range).
    let mut log_heads_setup = crate::log_heads::setup::startup(log_heads_pending, &log)?;
    let liveness = liveness::prepare(cfg, log_heads_setup.as_mut(), store.as_deref());
    log.prove_legacy_withdrawals(&cfg.custody)
        .await
        .inspect_err(|e| error!(alert = "legacy_withdrawal_unproven", "refusing writable recovery: {e:#}"))?; // The API also refuses an emission schedule with another budget than the vault's.
    let emission_budget = match (&ready, &checkpoint_setup) {
        (Some(ready), _) => ready.emission_budget,
        (None, Some(setup)) => Some(emission_preflight(&setup.poster, log.emissions()).await?),
        (None, None) => None,
    };

    let vault: AccountId = cfg.custody.vault.parse().context("custody.vault")?;
    let client_auth = cfg.client_auth()?;
    let operator = cfg.operator()?;
    let registry_operator = checkpoint_setup.as_ref().and_then(|setup| setup.registry_operator);
    let access = cfg.access.as_ref().map(crate::access::AccessClient::new).transpose()?.map(Arc::new);
    let (stream_tx, _) = broadcast::channel(CHANNEL_CAPACITY);
    let (withdrawals_tx, withdrawals_rx) = mpsc::unbounded_channel();
    let (manifest_tx, manifest_rx) = watch::channel(Manifest::default());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let (signing_tx, signing_rx) = watch::channel(true);
    // Only a rotating node with sealed keys holds a release the schedule can name; reading the vault,
    // it takes each turn once the keys are sealed in the generation the schedule holds for it.
    let turn_store = store.clone().filter(|_| cfg.operator.rotation_enabled);
    let turn_vault = if turn_store.is_some() { crate::handover::vault_reader(cfg)? } else { None };
    let (turn_generations, generations) = watch::channel(Default::default());
    let mut seq_cfg = SequencerConfig {
        access_policy: cfg.access.as_ref().map(crate::config::AccessSection::policy),
        log_dir: &cfg.node.log_dir,
        segment_commands: cfg.node.segment_commands,
        manifest: manifest_tx,
        checkpoint_every: cfg.node.checkpoint_every,
        checkpoint_registry: checkpoint_setup.as_ref().and_then(|setup| setup.poster.registry.clone()),
        checkpoint_stalled_after: cfg.custody.evm.as_ref().map_or(Duration::MAX, |evm| evm.finality_stalled_after),
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
        close_spread_ceilings: cfg.genesis.close_spread_ceilings.clone(),
        history_sealing: None,
        checkpoint_signing: Some(signing_tx),
        registry_operator,
        log_heads: None,
        turn_release: turn_store.as_ref().map(|store| engine_core::eip712::B256::from(store.release().id())),
        turn_sealing: turn_store
            .clone()
            .filter(|_| turn_vault.is_some())
            .map(|store| TurnSealing::new(store, generations)),
        governance: cfg
            .custody
            .evm
            .as_ref()
            .and_then(|evm| evm.staking.as_ref())
            .and_then(|staking| staking.registry.vote.as_ref())
            .map(crate::config::VoteCustodySection::governance)
            .transpose()?,
    };
    let custody = Custody::connect(&cfg.custody, log.last_seq())
        .inspect_err(|e| error!(alert = "custody_connect_failed", "refusing to serve: {e:#}"))?
        .with_access(access.clone());
    authorize(
        checkpoint_setup.as_ref().map(|setup| (&setup.poster, anchor.as_ref())),
        Some((&log, &seq_cfg, AccountId(key.address()))),
        ready.is_some(),
    )
    .await?;
    let (log, mut history) = history_plan
        .finish_with_log(log, key.clone())
        .inspect_err(|e| error!(alert = "history_recovery_failed", "refusing to serve: {e:#}"))?;
    seq_cfg.history_sealing = history.sealing();
    seq_cfg.log_heads = log_heads_setup.as_ref().map(|setup| setup.publisher());
    let published = crate::handover::Published::new(stream_tx.subscribe(), log.last_seq(), log.last_checkpoint());
    let mut seq = Sequencer::recover_from(log, seq_cfg, key.clone(), stream_tx.clone(), withdrawals_tx)
        .inspect_err(|e| error!(alert = "log_recovery_failed", "refusing to serve: {e:#}"))?;
    // Before activation and genesis sequence, so the record sees every command the log will hold.
    seq.track_stats(stats.clone());
    seq.pipeline_log_syncs().context("starting the log sync thread")?;
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

    // Startup has committed through recovery: move the seeded marks cache into place, attach the
    // sequencer's live mark reports, and start the supervised task that writes them and backfills.
    let (marks_store, marks_reader, marks_backfill, marks_sources, prior) =
        marks.activate().inspect_err(|e| error!(alert = "marks_cache_failed", "refusing to serve: {e:#}"))?;
    let marks_rx = marks::track(&mut seq);

    // Before anything is sequenced, so a turn whose keys are sealed in its generation is taken first.
    match (&ready, &turn_store, &turn_vault) {
        (Some(ready), ..) => drop(turn_generations.send_replace(ready.generations.clone())),
        (None, Some(store), Some(vault)) => {
            match turn_keys::read_generations(vault, &store.release(), &seq.untaken_turns()).await {
                Ok(read) => drop(turn_generations.send_replace(read)),
                Err(e) => warn!("reading the schedule's generations of this node's turns failed at start: {e:#}"),
            }
        }
        _ => {}
    }
    let (tx, rx) = mpsc::channel::<Request>(CHANNEL_CAPACITY);
    // A member's first stamps are admitted under the sample it took before connecting.
    let sample = ready.as_ref().map(|ready| ready.sample);
    if let Some(sample) = sample {
        tx.send(Request::TimeSample(sample)).await.map_err(|_| anyhow!("the sequencer's queue is closed"))?;
    }
    let marks_live = marks::live(marks_rx);
    let catch_up = crate::history::marks::run::CatchUp { prior, folded };
    let marks_task = marks::task(marks_store, tx.downgrade(), marks_backfill, marks_sources, marks_live, catch_up);
    let promoted = ready.as_ref().and_then(|ready| ready.promoted);
    let recovery = liveness::retire(seq.take_recovery_checkpoints(), promoted, &key);
    let seq_from = seq.last_seq().unwrap_or(0);
    let sequencer = crate::sequencer_thread::Worker::start(seq.run(rx), cpu)?;
    let mut tasks: JoinSet<TaskResult> = JoinSet::new();
    tasks.spawn(marks_task);
    let (samples, samples_rx) = watch::channel(sample);
    let trusted = || trusted_time::TrustedClock::new(clock.clone(), samples_rx.clone(), cfg.time.max_drift);
    if let Some(setup) = checkpoint_setup {
        let final_signer = promoted.map(|_| AccountId(key.address()));
        tasks.spawn(liveness::checkpoints(setup, tx.clone(), recovery, final_signer));
    }
    let released = log_heads_setup.as_ref().map(|setup| setup.released.subscribe());
    if let Some(setup) = log_heads_setup {
        tasks.spawn(setup.spawn(tx.clone(), trusted()));
    }
    let vouching = liveness::vouching(cfg, store.as_deref(), &key, trusted())?;
    let started = liveness::Started { promoted, from_seq: seq_from.saturating_add(1), key: AccountId(key.address()) };
    let liveness = liveness::start(cfg, liveness, released, started, &tx, trusted(), &mut tasks)?;

    // A successor's request stops sequencing through `drain_rx`, once the vault approves it; the
    // stop itself goes to the sequencer as a final-checkpoint request, which refuses while the
    // checkpoint gate would withhold the checkpoint.
    let (drain_tx, mut drain_rx) = mpsc::channel(1);
    let handover_stop_tx = tx.clone();
    let mut handover = JoinSet::new();
    if let Some(store) = &store
        && let Some(predecessor) =
            crate::handover::Predecessor::bind(cfg, store, attestor, &key, trusted(), drain_tx, signing_rx).await?
    {
        handover.spawn(predecessor.with_secrets(delivered.store.clone()).with_engine(&tx).run());
    }
    if let Some(store) = &store
        && let Some(vault) = crate::handover::vault_reader(cfg)?
    {
        let (store, every, trusted) = (store.clone(), keys::RELEASE_CHECK_EVERY, trusted());
        tasks.spawn(async move { ("release", keys::watch_release(vault, store, every, trusted).await) });
    }
    if let (Some(store), Some(vault)) = (turn_store, turn_vault) {
        let (release, every) = (store.release(), keys::RELEASE_CHECK_EVERY);
        let watch =
            turn_keys::watch_turn_generations(vault, release, operator.address, tx.clone(), turn_generations, every);
        tasks.spawn(async move { ("turn generations", watch.await) });
    }
    if store.is_some() && cfg.operator.rotation_enabled {
        // The incumbent pages when an election names another operator and, past the effective
        // time, when no successor has taken the keys.
        handover::spawn_watch(
            &mut tasks,
            tx.clone(),
            cfg.operator()?.address,
            clock.clone(),
            samples_rx.clone(),
            cfg.time.max_drift,
        );
    }

    if let Some(authority) = &delivered.authority {
        authority.send_replace(crate::secrets::Authority::Log {
            tx: tx.downgrade(),
            configured: operator.address,
            installed: registry_operator,
            trusted: Arc::new(trusted()),
        });
    }
    let tick = cfg.node.tick;
    let ticks = tx.clone();
    tasks.spawn(async move { ("ticker", run_ticker(ticks, tick).await) });
    if let Some(relay) = relay {
        tasks.spawn(async move { ("collateral", relay.run().await) });
    }

    let operator_hostname = ticker::start_operator_hostname(cfg, &mut tasks, trusted())?;
    // The monitor exports quote times only for the sources this config still feeds.
    let sources = feeds::configured_sources(&cfg.feeds);
    price_feeds::spawn(&mut tasks, cfg.feeds.clone(), delivered, &tx)?;

    let sealed =
        store.clone().zip(certificates).map(|(store, certificates)| Sealed { store, certificates, trusted: trusted() });
    let trusted_clock = trusted();
    // What trusted time had reached once a successor confirms, which the member never falls below.
    let handover_clock = trusted();
    let time_cfg = cfg.time.clone();
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

    let certificates = CertificateTask::configured(&cfg.api, tls.key, &metrics, sealed)?;
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
        clock: Arc::new(SystemClock),
        log_dir: cfg.node.log_dir.clone(),
        manifest: manifest_rx,
        backfills,
        limits: limits.clone(),
        history: history_reader,
        marks: marks_reader,
        stats,
        emission_budget,
        collateral: served,
        listing: Default::default(),
        operators: ticker::start_operators(&cfg.custody, &cfg.api, &mut tasks)?,
        operator_hostname,
        signing: Arc::new(liveness::signing(
            Signing::new(key, cfg.custody.chain_id, vault, cfg.api.heartbeat_interval, trusted_clock),
            liveness.as_ref(),
            vouching,
        )),
    });
    info!(listen = %listener.local_addr()?, "api up over TLS");
    // After the bind, so the CA's validation connections find the listener.
    if let Some(certificates) = certificates {
        tasks.spawn(async move { ("certificate", certificates.run().await) });
    }
    let (api, retained) = (cfg.api.clone(), limits.clone());
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
            let gave = Handed {
                generation: store.as_ref().map(|store| store.generation()),
                reached: handover_clock.reached_ms(),
            };
            handed_over(supervised.handover).await.map(|()| Ended::HandedOver(gave))
        }
        None => outcome.map(|()| Ended::Stopped),
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) mod paired_tests;
