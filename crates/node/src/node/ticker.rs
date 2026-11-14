//! The ticker: a `Tick` command at a fixed interval, so sessions and rollovers fire without other traffic,
//! and the start of the operator list's reader.

use std::time::Duration;

use anyhow::anyhow;
use engine_core::command::Command;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::interval;

use crate::config::{ApiSection, CustodySection, NodeConfig};
use crate::custody::{OperatorsFeed, OperatorsReader};
use crate::handover::hostname::{HostnameFeed, watch_hostname};
use crate::node::shutdown::TaskResult;
use crate::sequencer::Request;

/// Sequences a `Tick` every `every`, so sessions and rollovers fire without other traffic.
/// Error cases:
/// - the sequencer has gone away
pub(super) async fn run_ticker(tx: mpsc::Sender<Request>, every: Duration) -> anyhow::Result<()> {
    let mut ticks = interval(every);
    loop {
        ticks.tick().await;
        tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.map_err(|_| anyhow!("sequencer gone"))?;
    }
}

/// Spawns the reader of the operator list through its own custody quorum, apart from the deposit
/// watcher's, and returns the feed `/v1/operators` serves from; `None` without a configured lens.
/// Error cases:
/// - `custody.vault` or a provider's client does not parse or build
pub(super) fn start_operators(
    custody: &CustodySection,
    api: &ApiSection,
    tasks: &mut JoinSet<TaskResult>,
) -> anyhow::Result<Option<OperatorsFeed>> {
    let Some((reader, feed)) = OperatorsReader::connect(custody, api)? else { return Ok(None) };
    tracing::info!(every = ?api.operators_interval, "reading the operator list through the registry lens");
    tasks.spawn(async move { ("operators", reader.run().await) });
    Ok(Some(feed))
}

/// Spawns the reader of the operator in effect's registered API hostname, every
/// `api.operators_interval`, for a rotating node with `[checkpoints]`; `None` otherwise, so
/// `/v1/operator` omits the field.
/// Error cases:
/// - `[checkpoints]` needs EVM custody the config lacks, or the vault address or RPC settings are malformed
pub(super) fn start_operator_hostname(
    cfg: &NodeConfig,
    tasks: &mut JoinSet<TaskResult>,
    trusted: crate::trusted_time::TrustedClock,
) -> anyhow::Result<Option<HostnameFeed>> {
    if !cfg.operator.rotation_enabled {
        return Ok(None);
    }
    let Some(vault) = crate::handover::vault_reader(cfg)? else { return Ok(None) };
    let (feed, rx) = tokio::sync::watch::channel(None);
    let every = cfg.api.operators_interval;
    let reached = move || trusted.reached_ms();
    tasks.spawn(async move { ("operator_hostname", watch_hostname(vault, every, feed, reached).await) });
    Ok(Some(rx))
}
