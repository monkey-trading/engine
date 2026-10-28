//! Sequencer requests and public log stream messages.

use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::benchmarks::BenchmarkSchedule;
use crate::log::{Checkpoint, CommandEntry, LogEntry, Offset};
use crate::trusted_time::Sample;

pub type Query = Box<dyn FnOnce(&Engine) + Send>;

// Unboxed on purpose: boxing every command would add an allocation to the hot path.
#[allow(clippy::large_enum_variant)]
pub enum Request {
    Submit {
        cmd: Command,
        reply: Option<oneshot::Sender<Vec<Event>>>,
    },
    Query(Query),
    /// A trusted time sample from the trusted time task.
    TimeSample(Sample),
    /// Answered with the last seq once every command before it is synced, so it survives power loss.
    Sync(oneshot::Sender<Option<u64>>),
    /// Subscribes to durable rollover changes after every prior request has been flushed.
    WatchBenchmarkSchedule(oneshot::Sender<watch::Receiver<BenchmarkSchedule>>),
    /// Custody confirmation; affects signature release, never deterministic protocol state. `None`
    /// health names an observation that failed: the previous verdict stands, so a proved
    /// divergence or an unrevalidated startup is never erased by an RPC error.
    CheckpointStatus {
        posted: Option<u64>,
        health: Option<crate::checkpoints::Health>,
        lifecycle: Option<crate::checkpoints::LifecycleEvidence>,
    },
    /// A holder, already authenticated, asks for a fresh authorization of an approval whose last one
    /// expired unclaimed. `replaces` is the expiry of the authorization being replaced, 0 if none.
    ReissueWithdrawal {
        account: AccountId,
        nonce: u64,
        replaces: u64,
        reply: oneshot::Sender<Reissue>,
    },
}

/// The sequencer's answer to a reissue request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reissue {
    /// Queued behind earlier approvals; the new authorization arrives on the stream.
    Queued,
    /// No unpaid approval of this account and nonce is loaded: it is unknown, paid, or still waiting
    /// for a restart's recovery to load it.
    Unknown,
    /// The request replaces an authorization other than the latest, whose expiry is `current`.
    Stale { current: u64 },
    /// The latest authorization is still usable until `expires_at` (unix seconds).
    StillValid { expires_at: u64 },
    /// No checkpoint gate (mock custody), or no trusted time yet.
    Unavailable,
}

/// What `/v1/stream` publishes: each log entry at its offset, in offset order.
#[derive(Clone, Debug, Serialize)]
pub struct StreamMsg {
    pub offset: Offset,
    #[serde(flatten)]
    pub body: StreamBody,
}

// Unboxed on purpose, like `Request`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamBody {
    Command(CommandEntry),
    Checkpoint(Checkpoint),
    Withdrawal(SignedWithdrawal),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedWithdrawal {
    pub account: AccountId,
    pub amount: Fixed,
    pub nonce: u64,
    pub chain_id: u64,
    pub vault: AccountId,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<crate::withdrawals::Authorization>,
}

impl StreamMsg {
    /// The stream form of the log entry at `offset`.
    pub fn new(offset: Offset, entry: LogEntry) -> Self {
        let body = match entry {
            LogEntry::Command(c) => StreamBody::Command(c),
            LogEntry::Checkpoint(cp) => StreamBody::Checkpoint(cp),
            LogEntry::Withdrawal(w) => StreamBody::Withdrawal(w),
        };
        Self { offset, body }
    }
}
