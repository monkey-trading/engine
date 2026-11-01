//! The ticker: a `Tick` command at a fixed interval, so sessions and rollovers fire without other traffic.

use std::time::Duration;

use anyhow::anyhow;
use engine_core::command::Command;
use tokio::sync::mpsc;
use tokio::time::interval;

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
