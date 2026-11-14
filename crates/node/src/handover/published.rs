//! What the sequencer has published since the node started, which becomes a handover's final head.

use std::sync::Arc;

use anyhow::{Context, ensure};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tracing::info;

use crate::log::Checkpoint;
use crate::sequencer::{StreamBody, StreamMsg};
use crate::tee::keys::Head;

/// What the sequencer has published since the node started, read from its own stream: its last
/// command and its last checkpoint, which become a handover's final head once it drains. Held in
/// memory, so the host's disk plays no part; a receiver that falls behind skips the oldest entries
/// only, never the newest.
pub(crate) struct Published {
    receiver: broadcast::Receiver<Arc<StreamMsg>>,
    last_seq: Option<u64>,
    /// The sequenced time of the last command published since the start.
    last_ts: Option<engine_core::types::Millis>,
    checkpoint: Option<Head>,
}

impl Published {
    /// Follows `receiver` on from the recovered log's last command and last checkpoint.
    pub(crate) fn new(
        receiver: broadcast::Receiver<Arc<StreamMsg>>,
        last_seq: Option<u64>,
        last_checkpoint: Option<&Checkpoint>,
    ) -> Self {
        let checkpoint = last_checkpoint.map(|cp| Head { seq: cp.seq, root: cp.root.clone() });
        Self { receiver, last_seq, last_ts: None, checkpoint }
    }

    /// The final checkpoint once the sequencer has drained, which publishes everything it wrote
    /// before it returns: the checkpoint of its last command, whose sequenced time it logs as the
    /// incumbent's last stamp. A withheld final checkpoint is only a
    /// post-commit defense: the stop the node commits to already wrote one, so this error names the
    /// real recovery — new keys through the timelock — since a restart of this release refuses
    /// once the vault names the successor.
    /// Error cases:
    /// - no checkpoint was written, or the drain withheld the last command's (custody not yet
    ///   reconciled), so the keys stay with this release
    pub(crate) fn final_head(mut self) -> anyhow::Result<Head> {
        loop {
            match self.receiver.try_recv() {
                Ok(message) => match &message.body {
                    StreamBody::Command(command) => {
                        (self.last_seq, self.last_ts) = (Some(command.sequenced.seq), Some(command.sequenced.ts));
                    }
                    StreamBody::Checkpoint(cp) => self.checkpoint = Some(Head { seq: cp.seq, root: cp.root.clone() }),
                    StreamBody::Withdrawal(_) => {}
                },
                Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let head = self.checkpoint.context("the log has no checkpoint to hand over")?;
        ensure!(
            Some(head.seq) == self.last_seq,
            "the final checkpoint was withheld after seq {}, so the keys stay with this release: the vault \
             already names the successor and a restart of this release refuses, so the successor needs new \
             keys, which the timelock registers",
            head.seq
        );
        info!(seq = head.seq, last_command_ts = self.last_ts, "the incumbent's last sequenced command before the stop");
        Ok(head)
    }
}
