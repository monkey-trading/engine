//! The mock provider's staking frontier. From `V75` an epoch closes only once the staking mirror's
//! finalised frontier has passed its end, and the mock has no chain to mirror, so once an epoch has
//! ended past the frontier it completes a scan stamped at the engine's time, as a mirror that keeps up
//! would: one command per epoch, and a dev node's epochs keep closing. A `production` build refuses the
//! mock provider.

use std::time::Duration;

use alloy::primitives::B256;
use engine_core::Engine;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use tokio::sync::mpsc;
use tracing::warn;

use crate::custody::credit::{query, submit};
use crate::custody::stall::PollError;
use crate::sequencer::Request;

/// How often the mock looks for an epoch whose end the frontier has not passed.
const INTERVAL: Duration = Duration::from_secs(1);

/// Completes a mock staking scan whenever the latest epoch the sequenced time has ended lies past the
/// frontier, until the sequencer stops.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn run(tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(INTERVAL);
    loop {
        interval.tick().await;
        let Some((block, block_time)) = query(&tx, due_scan).await? else { continue };
        let progress =
            Command::StakingProgress { block, block_hash: B256::repeat_byte(0x11), block_time, named_turn: None };
        match submit(&tx, progress).await {
            Ok(events) if events.iter().any(|event| matches!(event, Event::AdminRejected { .. })) => {
                warn!(block, "the engine refused the mock staking frontier: {events:?}");
            }
            Ok(_) => {}
            Err(PollError::Fatal(error)) => return Err(error),
            Err(error) => warn!(block, "the mock staking frontier was not sequenced: {error:?}"),
        }
    }
}

/// The scan the mock completes now, if one is due: from `V75`, once staking is initialised and the
/// latest epoch the engine's time has ended lies past the frontier, the next block stamped at the
/// engine's time.
fn due_scan(engine: &Engine) -> Option<(u64, Option<Millis>)> {
    let staking = engine.staking().filter(|_| engine.protocol_version() >= ProtocolVersion::V75)?;
    let config = staking.config;
    let ended = config.epochs_elapsed(engine.now()).checked_sub(1)?;
    let reached = staking.completed_through.and_then(|checkpoint| checkpoint.time);
    if reached.is_some_and(|reached| reached >= config.epoch_end(ended)) {
        return None;
    }
    let block = staking
        .completed_through
        .map_or(1, |checkpoint| checkpoint.block + 1)
        .max(staking.mirrored_through.unwrap_or(0));
    Some((block, Some(engine.now())))
}

#[cfg(test)]
mod tests {
    use crate::stats::tests::Rig;

    use super::*;

    #[test]
    fn a_scan_is_due_once_an_epoch_has_ended_past_the_frontier() {
        let mut rig = Rig::staking_at(ProtocolVersion::V75);
        let config = rig.engine.staking().unwrap().config;
        assert_eq!(due_scan(&rig.engine), None, "no epoch has ended");
        let end = config.epoch_end(0);
        rig.apply(end, Command::Tick);
        assert_eq!(due_scan(&rig.engine), Some((1, Some(end))));
        let progress = Command::StakingProgress {
            block: 1,
            block_hash: B256::repeat_byte(0x11),
            block_time: Some(end),
            named_turn: None,
        };
        assert!(!rig.apply(end, progress).iter().any(|event| matches!(event, Event::AdminRejected { .. })));
        assert_eq!(due_scan(&rig.engine), None, "the frontier has passed the end");
        let closed = rig.apply(end + 1_000, Command::Tick);
        assert!(
            closed.iter().any(|event| matches!(event, Event::DistributionEpochClosed { epoch: 0, .. })),
            "{closed:?}"
        );
        let next = config.epoch_end(1);
        rig.apply(next, Command::Tick);
        assert_eq!(due_scan(&rig.engine), Some((2, Some(next))));
    }

    #[test]
    fn no_scan_is_due_before_v75() {
        let mut rig = Rig::staking_at(ProtocolVersion::V74);
        let end = rig.engine.staking().unwrap().config.epoch_end(0);
        rig.apply(end, Command::Tick);
        assert_eq!(due_scan(&rig.engine), None);
    }
}
