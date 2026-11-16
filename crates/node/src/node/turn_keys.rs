//! The generation the finalised schedule holds for each daily turn the log assigns this node's
//! operator key, which the turn's `TurnTaken` waits for the keys to be sealed in (`sequencer::turns`).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::anyhow;
use engine_core::types::AccountId;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

use crate::checkpoints::{CheckpointChain, View};
use crate::sequencer::Request;
use crate::tee::sealing::Release;

/// Publishes to `generations`, every `every`, the generation the finalised schedule holds for each
/// turn the log assigns `operator` and has yet to see taken, when the schedule names `release` for
/// it. A turn the log mirrors ahead of its boundary is read before it applies, so its `TurnTaken`
/// waits on nothing then. `started`, the generations a member's start published from the latest
/// agreed block, stand for their turns until finality reaches them (`published`). A failed read keeps
/// what was published, and is retried.
/// Error cases:
/// - the sequencer has gone away
pub(super) async fn watch_turn_generations(
    vault: impl CheckpointChain,
    (release, started): (Release, BTreeMap<u64, u64>),
    operator: AccountId,
    sequencer: mpsc::Sender<Request>,
    generations: watch::Sender<BTreeMap<u64, u64>>,
    every: Duration,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let turns = untaken_turns(&sequencer, operator).await?;
        match read_drawn(&vault, &turns).await {
            Ok(drawn) => {
                let read = published(&drawn, release.id(), &started, &turns);
                generations.send_if_modified(|published| std::mem::replace(published, read.clone()) != read);
            }
            Err(e) => warn!("reading the schedule's generations of this node's turns failed: {e:#}"),
        }
    }
}

/// The turns the log assigns `operator`, waiting to apply or in effect, that it has yet to see taken.
/// Error cases:
/// - the sequencer has gone away
async fn untaken_turns(sequencer: &mpsc::Sender<Request>, operator: AccountId) -> anyhow::Result<Vec<u64>> {
    let (reply, answered) = oneshot::channel();
    let query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send(crate::sequencer::turns::untaken(engine.daily_turns(), operator));
    });
    sequencer.send(Request::Query(query)).await.map_err(|_| anyhow!("sequencer gone"))?;
    answered.await.map_err(|_| anyhow!("sequencer gone"))
}

/// The generation the finalised schedule holds for each of `turns` it names `release` for.
/// Error cases:
/// - the vault or the schedule cannot be read
pub(super) async fn read_generations(
    vault: &impl CheckpointChain,
    release: &Release,
    turns: &[u64],
) -> anyhow::Result<BTreeMap<u64, u64>> {
    let drawn = read_drawn(vault, turns).await?;
    Ok(published(&drawn, release.id(), &BTreeMap::new(), turns))
}

/// The generation the finalised schedule holds for each of `turns` it has drawn, with the release it
/// names for it.
/// Error cases:
/// - the vault or the schedule cannot be read
async fn read_drawn(vault: &impl CheckpointChain, turns: &[u64]) -> anyhow::Result<BTreeMap<u64, (u64, [u8; 32])>> {
    let at = vault.observe(View::Finalized, &[]).await?;
    let mut read = BTreeMap::new();
    for &turn in turns {
        // Accepted turns have a boundary within the latest stamp.
        let Some(boundary) = engine_core::turns::boundary(turn) else { continue };
        let drawn = vault.scheduled_turn(&at, boundary).await?;
        if let Some(drawn) = drawn.filter(|drawn| drawn.turn == turn) {
            read.insert(turn, (drawn.generation, drawn.release));
        }
    }
    Ok(read)
}

/// The generation to publish for each of `turns`: one of `started` while the finalised schedule, in
/// `drawn`, holds that turn in an earlier generation or not at all, as while a promotion that carried it
/// waits for finality, since the vault's one counter only rises; otherwise the finalised schedule's,
/// when it names `release` for the turn.
fn published(
    drawn: &BTreeMap<u64, (u64, [u8; 32])>,
    release: [u8; 32],
    started: &BTreeMap<u64, u64>,
    turns: &[u64],
) -> BTreeMap<u64, u64> {
    let generation = |turn: &u64| {
        let finalised = drawn.get(turn).copied();
        match started.get(turn).copied() {
            Some(started) if finalised.is_none_or(|(generation, _)| generation < started) => Some(started),
            _ => finalised.filter(|(_, named)| *named == release).map(|(generation, _)| generation),
        }
    };
    turns.iter().filter_map(|turn| generation(turn).map(|generation| (*turn, generation))).collect()
}

#[cfg(test)]
mod tests;
