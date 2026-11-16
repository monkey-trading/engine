//! The log entries the predecessor's tests publish and write: commands, checkpoints and their stream
//! messages.

use std::sync::Arc;

use engine_core::command::{Command, Sequenced};
use tokio::sync::broadcast;

use crate::log::{Checkpoint, CommandEntry, LogEntry, Offset};
use crate::sequencer::StreamMsg;

/// Publishes `entry` as the sequencer does.
pub(super) fn publish(tx: &broadcast::Sender<Arc<StreamMsg>>, entry: LogEntry) {
    let seq = match &entry {
        LogEntry::Command(command) => command.sequenced.seq,
        LogEntry::Checkpoint(cp) => cp.seq,
        LogEntry::Withdrawal(_) => 0,
    };
    let _ = tx.send(Arc::new(StreamMsg::new(Offset::start_of(seq), entry)));
}

pub(super) fn command(seq: u64) -> CommandEntry {
    CommandEntry {
        sequenced: Sequenced { seq, ts: 1_000 + seq as i64, cmd: Command::Tick },
        events: Some(Vec::new()),
        seal: None,
    }
}

pub(super) fn checkpoint(seq: u64, root: &str) -> Checkpoint {
    serde_json::from_value(serde_json::json!({
        "seq": seq,
        "root": root,
        "signature": "0x",
        "signer": "0x",
        "domain": { "chain_id": 31337, "vault": "0x0000000000000000000000000000000000000000" },
    }))
    .unwrap()
}
