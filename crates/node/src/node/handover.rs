//! The registry's rotation switch against the config at start, and the incumbent's election
//! watcher: alerts while an election naming another operator is mirrored and unhanded.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, ensure};
use engine_core::types::Millis;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, info, warn};

use super::keys::RELEASE_CHECK_EVERY;
use super::shutdown::TaskResult;
use crate::checkpoints::CheckpointChain;
use crate::clock::Clock;
use crate::config::NodeConfig;
use crate::sequencer::Request;
use crate::trusted_time::{Sample, TrustedClock};
use engine_core::types::AccountId;

/// Spawns the incumbent's election watch under rotation; pages when an election names another
/// operator and, past the effective time, when no successor has taken the keys.
pub(super) fn spawn_watch(
    tasks: &mut tokio::task::JoinSet<TaskResult>,
    tx: mpsc::Sender<Request>,
    operator: AccountId,
    clock: Arc<dyn Clock>,
    samples: watch::Receiver<Option<Sample>>,
    max_drift: Duration,
) {
    tasks.spawn(async move {
        (
            "handover",
            watch_handover(tx, operator, TrustedClock::new(clock, samples, max_drift), RELEASE_CHECK_EVERY).await,
        )
    });
}

/// The alert episode an incumbent's election implies: none while the election names this operator
/// or is cancelled, `elected` once another operator is named, and `overdue` past the registry's
/// effective time — this node still running is then the proof no handover happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Episode {
    None,
    Elected,
    Overdue,
}

/// Maps the mirrored election and trusted time to the episode, pure so the escalation is testable.
pub(super) fn episode(
    last: Option<engine_core::election::Election>,
    operator: AccountId,
    now: Option<Millis>,
) -> Episode {
    let Some(last) = last.filter(|last| !last.cancelled && last.admin_key != operator) else {
        return Episode::None;
    };
    // The registry's effective time is when the vault names the successor's release: from then a
    // handover can complete, so the incumbent is overdue once trusted time passes it, whatever the
    // engine's own deferral logged later.
    match now.is_some_and(|now| now >= last.registry_effective_at) {
        true => Episode::Overdue,
        false => Episode::Elected,
    }
}

/// Checks `operator.rotation_enabled` against the vault's registry before the keys, setting
/// `chain_read` once the chain answers.
/// Error cases:
/// - see `check_rotation_switch`, or, while rotation is on, the registry names another API hostname
pub(super) async fn check_rotation(cfg: &NodeConfig, chain_read: &mut bool) -> anyhow::Result<()> {
    check_rotation_for(cfg, chain_read, None).await
}

/// `check_rotation` for a member taking a turn of `candidate`'s, whose registered hostname its
/// certificate names, read at the finalized block rather than taken from the operator the latest
/// block names, which may not yet have passed the boundary.
/// Error cases:
/// - see `check_rotation`
pub(super) async fn check_rotation_for(
    cfg: &NodeConfig,
    chain_read: &mut bool,
    candidate: Option<engine_core::types::AccountId>,
) -> anyhow::Result<()> {
    // While rotation is on, the certificate the enclave orders names the API hostname the
    // registry records for the operator in effect.
    if let Some((vault, true)) = check_rotation_switch(cfg, chain_read).await? {
        crate::handover::check_operator_hostname(cfg, &vault, candidate).await?;
    }
    Ok(())
}

/// Checks `operator.rotation_enabled` against the vault's registry's rotation switch, setting
/// `chain_read` once the chain answers: a rotation-set member checks it before it follows, as any
/// node does before the keys. Returns the vault's reader and the switch, with `[checkpoints]`.
/// Error cases:
/// - `operator.rotation_enabled = true` while the vault's registry has rotation off, or without
///   `[checkpoints]` EVM custody to read it
/// - the chain cannot be read
pub(super) async fn check_rotation_switch(
    cfg: &NodeConfig,
    chain_read: &mut bool,
) -> anyhow::Result<Option<(crate::checkpoints::CheckpointRpc, bool)>> {
    let Some(vault) = crate::handover::vault_reader(cfg)? else {
        ensure!(
            !cfg.operator.rotation_enabled,
            "operator.rotation_enabled = true needs [checkpoints] with EVM custody to read the registry"
        );
        return Ok(None);
    };
    let on = vault.rotation_enabled().await?;
    *chain_read = true;
    info!(alert_resolved = "secrets_start_failed", "the node read the chain");
    if cfg.operator.rotation_enabled && !on {
        bail!("operator.rotation_enabled = true, but the vault's registry has rotation off");
    }
    if on && !cfg.operator.rotation_enabled {
        warn!(
            "the vault's registry has rotation on, but operator.rotation_enabled is false: an election naming another operator moves the admin key this node does not expect"
        );
    }
    Ok(Some((vault, on)))
}

/// Watches the mirrored election an incumbent with sealed keys runs under: raises `operator_elected`
/// (Warning) once an election names another operator, and `handover_overdue` (Critical) once trusted
/// time passes the registry's effective time with this node still running, on any transition into
/// `Overdue`. A cancellation, or an election naming this operator's key again, resolves both alerts
/// with the fields they fired under, so alert routing can close the episodes.
/// Error cases:
/// - none; it runs until the node stops
pub(super) async fn watch_handover(
    tx: mpsc::Sender<Request>,
    operator: AccountId,
    trusted: TrustedClock,
    every: Duration,
) -> anyhow::Result<()> {
    let mut current = Episode::None;
    let mut fired: Option<engine_core::election::Election> = None;
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let (reply, answered) = oneshot::channel();
        if tx
            .send(Request::Query(Box::new(move |engine| {
                let _ = reply.send(engine.operator_election().last);
            })))
            .await
            .is_err()
        {
            return Ok(());
        }
        let last = match answered.await {
            Ok(last) => last,
            Err(_) => return Ok(()),
        };
        let challenger = last.filter(|last| !last.cancelled && last.admin_key != operator);
        let next = episode(last, operator, trusted.now_ms());
        match (current, next) {
            (Episode::None, Episode::Elected) => {
                if let Some(last) = challenger {
                    warn!(
                        alert = "operator_elected",
                        epoch = last.epoch,
                        elected = %last.candidate,
                        effective_at = last.registry_effective_at,
                        "an election names another operator: a successor enclave is expected to connect at \
                         this node's handover endpoint by the effective time"
                    );
                    fired = Some(last);
                }
            }
            (from, Episode::Overdue) if from != Episode::Overdue => {
                if let Some(last) = challenger {
                    error!(
                        alert = "handover_overdue",
                        epoch = last.epoch,
                        effective_at = last.registry_effective_at,
                        "the election's effective time passed with no successor confirmed: this node still \
                         holds the keys the elected operator's enclave should have taken; check its \
                         handover endpoint and archive reachability"
                    );
                    fired = Some(last);
                }
            }
            (Episode::Elected | Episode::Overdue, Episode::None) => {
                if current == Episode::Overdue {
                    resolve_overdue(&fired);
                }
                resolve(&mut fired);
            }
            _ => {}
        }
        current = next;
    }
}

/// Emits the `handover_overdue` resolution for an open episode, with the fields it fired under.
fn resolve_overdue(fired: &Option<engine_core::election::Election>) {
    if let Some(last) = fired {
        info!(
            alert_resolved = "handover_overdue",
            epoch = last.epoch,
            effective_at = last.registry_effective_at,
            "the overdue handover episode ends"
        );
    }
}

/// Emits the `operator_elected` resolution with the fields it fired under and clears it, so alert
/// routing closes the same episode the firing opened.
fn resolve(fired: &mut Option<engine_core::election::Election>) {
    if let Some(last) = fired.take() {
        info!(
            alert_resolved = "operator_elected",
            epoch = last.epoch,
            elected = %last.candidate,
            effective_at = last.registry_effective_at,
            "the election naming another operator ended: cancelled, superseded, or handed over"
        );
    }
}
