//! Running the node for the process: its delivered secrets, its alert routing, and, for a
//! rotation-set member, the wait for each holding between runs (`member`).

use anyhow::anyhow;
use tokio::task::JoinSet;
use tracing::{error, warn};

use crate::config::NodeConfig;
use crate::node::member::{Member, is_member, run_turns};
use crate::node::{FAILED_START_HOLD, KeyStart, SHUTDOWN_GRACE, secrets, serve_node};
use crate::telemetry::Telemetry;

/// Runs the node with the CPU reserved before its runtime was created, and the operator's choice of
/// keys for a release that has none sealed.
/// Error cases:
/// - see `run`
pub(crate) async fn run_with_cpu(
    mut cfg: NodeConfig,
    mut telemetry: Telemetry,
    cpu: Option<usize>,
    start: KeyStart,
) -> anyhow::Result<()> {
    cfg.refuse_environment_secrets_on_a_node()?;
    cfg.validate_secrets_on_a_node()?;
    // Alert lines queue until routing starts, once this has set any delivered webhook.
    let mut delivered =
        secrets::startup(&mut cfg, &mut telemetry, &start).await.inspect_err(|e| error!("refusing to serve: {e:#}"))?;
    // Takes deliveries through everything below, however long the vault takes to name this key.
    let mut listening = JoinSet::new();
    if let Some(listener) = delivered.listener.take() {
        listening.spawn(listener.run());
    }
    let Telemetry { metrics, alerts, pages_critical } = telemetry;
    if !pages_critical {
        warn!("no alerts.critical webhook: Critical alerts are only logged");
    }
    cfg.alerts.outbox.check_disk_location(&cfg.node.log_dir)?;
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await?;
    // Bound once, it answers while a member follows as well as across each of its runs.
    let metrics_listener = crate::metrics::bind(cfg.metrics.listen).await?;
    // The process's own tasks, one each for its life, while it follows as well as while it holds a
    // turn: the seed relayer, which every rotation-set member's node runs.
    let mut process = ProcessTasks::new();
    if let Some(seed_relay) = crate::seed_relay::task(&cfg, &metrics)? {
        process.spawn(async move { ("seed relayer", seed_relay.await) });
    }
    let mut chain_read = false;
    // A rotation-set member follows between its holdings, and goes back to following once it hands over.
    let member = is_member(&cfg, &start);
    let turns = async {
        // A member checks the registry's rotation switch before it follows, as any node does before
        // its keys.
        if member {
            crate::node::handover::check_rotation_switch(&cfg, &mut chain_read).await?;
        }
        let mut waiting = member.then(|| Member::new(&cfg, &start, metrics.clone())).transpose()?;
        let operator = cfg.operator()?.address;
        run_turns(
            member,
            async |handed| {
                let waiting = waiting.as_mut().expect("a member waits for its turns");
                if let Some(handed) = handed {
                    waiting.handed_over(handed);
                    delivered.following(operator);
                }
                waiting.wait().await
            },
            async |replica, handed| {
                let chain_read = &mut chain_read;
                // On the heap: the serving future is large, and a test awaits `run` on its own stack.
                Box::pin(serve_node(
                    &cfg,
                    metrics.clone(),
                    &mut routing,
                    cpu,
                    &start,
                    &delivered,
                    chain_read,
                    replica,
                    handed,
                ))
                .await
            },
        )
        .await
    };
    let outcome = tokio::select! {
        outcome = crate::metrics::alongside(metrics_listener, metrics.clone(), supervised(&mut process, turns)) => outcome,
        Some(stopped) = listening.join_next() => Err(anyhow!("the delivery listener stopped: {stopped:?}")),
    };
    // A provider refusing a delivered key stops a start at its first read of the chain, which only a
    // delivery repairs, so this stays up to take one before it exits to be started again.
    if let Err(e) = &outcome
        && !chain_read
        && delivered.chain_read_keyed
    {
        error!(
            alert = "secrets_start_failed",
            "the node cannot read the chain, perhaps on its delivered key, so its delivery listener takes a working \
             key for {FAILED_START_HOLD:?} before it exits, and the restart runs that key: {e:#}"
        );
        secrets::wait_for_a_new_bundle(&delivered, &mut listening, FAILED_START_HOLD).await;
    }
    routing.drain(SHUTDOWN_GRACE).await;
    outcome
}

/// The process's supervised tasks, each named, which `supervised` ends the process with.
pub(super) type ProcessTasks = JoinSet<(&'static str, anyhow::Result<()>)>;

/// Runs `run` beside `tasks`, the process's own, spawned off `run`'s future, and ends with it.
/// Error cases:
/// - `run` fails, or one of `tasks` stops or panics before it ends
pub(super) async fn supervised<T>(
    tasks: &mut ProcessTasks,
    run: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        outcome = run => outcome,
        Some(stopped) = tasks.join_next() => Err(match stopped {
            Ok((name, Ok(()))) => anyhow!("the {name} stopped"),
            Ok((name, Err(e))) => e.context(format!("the {name} stopped")),
            Err(e) => anyhow!("a process task panicked: {e}"),
        }),
    }
}
