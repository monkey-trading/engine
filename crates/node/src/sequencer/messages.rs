//! Sequencer requests and public log stream messages.

use engine_core::access::{AccessEvidence, AccessReject};
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};

use crate::benchmarks::BenchmarkSchedule;
use crate::log::{Checkpoint, CommandEntry, LogEntry, Offset};
use crate::trusted_time::Sample;

pub type Query = Box<dyn FnOnce(&Engine) + Send>;

/// Why a handover's stop was refused at the checkpoint gate: the node goes on sequencing, so the
/// predecessor refuses the successor and keeps serving. Distinct from a post-commit failure, where
/// sequencing has already stopped and the node must end.
#[derive(Debug)]
pub struct HandoverRefused(String);

impl std::fmt::Display for HandoverRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HandoverRefused {}

impl HandoverRefused {
    /// The gate's refusal, reported to the successor.
    pub fn new(reason: String) -> Self {
        Self(reason)
    }
}

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
    /// `screening` is the account's and peer's screening of the signed request, once access is
    /// enforced.
    ReissueWithdrawal {
        account: AccountId,
        nonce: u64,
        replaces: u64,
        screening: Option<ReissueScreening>,
        reply: oneshot::Sender<Reissue>,
    },
    /// An account's unpaid payouts and their latest authorizations, from the gate's memory.
    AccountPayouts {
        account: AccountId,
        after: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<AccountPayouts>,
    },
    /// A key handover's stop: answered once the final checkpoint the successor is handed is written
    /// and durable, and refused with the checkpoint gate's reason while it would withhold that
    /// checkpoint, the node going on sequencing.
    FinalCheckpoint(oneshot::Sender<anyhow::Result<()>>),
}

/// The source's evidence for a signed reissue request, and the request's digest it must name.
#[derive(Clone, Debug)]
pub struct ReissueScreening {
    pub request_hash: B256,
    pub evidence: AccessEvidence,
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
    /// Access screening refused the account or its peer, as it refuses a new withdrawal; the
    /// approval stays unpaid and held.
    Refused { reason: AccessReject },
    /// No checkpoint gate (mock custody), or no trusted time yet.
    Unavailable,
}

/// One page of an account's unpaid payouts, oldest first (nonces rise with approval order).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AccountPayouts {
    /// The last sequenced command the answer reflects.
    pub seq: Option<u64>,
    /// Whether every unpaid approval is held: false with no checkpoint gate, and while a restart is
    /// still loading approvals, so an absent nonce says nothing then.
    pub loaded: bool,
    pub payouts: Vec<PayoutView>,
    /// The nonce to pass as `after` for the rest, if the page was full.
    pub next: Option<u64>,
}

/// An unpaid approval and its latest signed authorization, `None` until one is signed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PayoutView {
    pub nonce: u64,
    pub amount: Fixed,
    pub withdrawal: Option<SignedWithdrawal>,
}

/// What `/v1/stream` publishes: each log entry at its offset, in offset order. A standby follower
/// deserializes what it serializes, and verifies each frame's signature before trusting it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamMsg {
    pub offset: Offset,
    #[serde(flatten)]
    pub body: StreamBody,
}

// Unboxed on purpose, like `Request`.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamBody {
    Command(CommandEntry),
    Checkpoint(Checkpoint),
    Withdrawal(SignedWithdrawal),
}

impl From<StreamBody> for LogEntry {
    fn from(body: StreamBody) -> Self {
        match body {
            StreamBody::Command(command) => LogEntry::Command(command),
            StreamBody::Checkpoint(checkpoint) => LogEntry::Checkpoint(checkpoint),
            StreamBody::Withdrawal(withdrawal) => LogEntry::Withdrawal(withdrawal),
        }
    }
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
