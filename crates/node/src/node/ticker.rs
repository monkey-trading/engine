//! The ticker: a `Tick` command at a fixed interval, so sessions and rollovers fire without other traffic,
//! and the start of the operator list's reader.

use std::time::Duration;

use anyhow::anyhow;
use engine_core::command::Command;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::interval;

use crate::config::{ApiSection, CustodySection};
use crate::custody::{OperatorsFeed, OperatorsReader};
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
