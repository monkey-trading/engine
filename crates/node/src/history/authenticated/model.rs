//! Fixed-size list/queue headers and authenticated, scope-qualified payload chunk addresses.

use anyhow::{Context, Result, bail, ensure};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};

use crate::history::authenticated::state::State;
use crate::history::{HistoryEvent, KINDS};
use crate::log::Offset;

pub(super) const PAYLOAD_CHUNK_BYTES: usize = crate::authenticated::CHUNK_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ListId {
    pub account: AccountId,
    pub kind: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Metadata {
    pub generation: u64,
    pub lo: u64,
    pub hi: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Position {
    pub generation: u64,
    pub rank: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Payload {
    pub length: u64,
    pub chunks: u64,
}

/// Dense lists and the queue carry headers, never unbounded serialized events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Row {
    pub account: AccountId,
    pub kind: u8,
    pub cursor: Offset,
    pub ts: Millis,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Queued {
    pub row: Row,
    pub all: Position,
    pub kind: Position,
}

impl ListId {
    /// Separates the account's all-kind and individual-kind metadata keys.
    pub fn key(self) -> Vec<u8> {
        let mut key = b"history/v2/meta/".to_vec();
        key.extend_from_slice(&self.account.0);
        match self.kind {
            None => key.push(0),
            Some(kind) => key.extend_from_slice(&[1, kind]),
        }
        key
    }
}

impl Metadata {
    /// Checks a populated dense interval and its allocated generation.
    /// Error cases:
    /// - empty/reversed interval or unallocated generation
    pub fn validate(&self, state: &State) -> Result<()> {
        ensure!(self.lo < self.hi, "empty or reversed history list");
        ensure!(self.generation < state.next_generation, "unallocated history list generation");
        Ok(())
    }

    /// Identifies a rank within this list's globally unique generation.
    pub fn position(&self, rank: u64) -> Position {
        Position { generation: self.generation, rank }
    }
}

impl Position {
    /// Addresses one fixed-size rank header in the global tree.
    pub fn key(self) -> Vec<u8> {
        let mut key = b"history/v2/rank/".to_vec();
        key.extend_from_slice(&self.generation.to_be_bytes());
        key.extend_from_slice(&self.rank.to_be_bytes());
        key
    }
}

impl Payload {
    /// Checks the exact chunk count derived from the authenticated serialized length.
    /// Error cases:
    /// - zero length or inconsistent count
    pub fn validate(self) -> Result<()> {
        let size = PAYLOAD_CHUNK_BYTES as u64;
        let expected = self.length / size + u64::from(!self.length.is_multiple_of(size));
        ensure!(self.length != 0 && self.chunks == expected, "invalid history payload descriptor");
        Ok(())
    }

    /// Computes an individual chunk's exact expected length without wrapping.
    /// Error cases:
    /// - invalid descriptor/index or arithmetic/representation overflow
    pub fn chunk_len(self, ordinal: u64) -> Result<usize> {
        self.validate()?;
        ensure!(ordinal < self.chunks, "history payload chunk outside descriptor");
        let offset = ordinal.checked_mul(PAYLOAD_CHUNK_BYTES as u64).context("payload offset overflow")?;
        let remaining = self.length.checked_sub(offset).context("payload offset exceeds length")?;
        usize::try_from(remaining.min(PAYLOAD_CHUNK_BYTES as u64)).context("payload chunk length")
    }
}

impl Row {
    /// Checks header identity, cursor/time progress, exact retention and chunk geometry.
    /// Error cases:
    /// - invalid account/kind, cursor/time/horizon, or payload descriptor
    pub fn validate(&self, list: ListId, state: &State) -> Result<()> {
        ensure!(self.account == list.account, "history account mismatch");
        ensure!(usize::from(self.kind) < KINDS.len(), "unknown history event kind");
        ensure!(list.kind.is_none_or(|kind| kind == self.kind), "history list kind mismatch");
        let last = state.last_command.context("history row without command progress")?;
        ensure!(self.cursor.seq <= last.seq, "history cursor beyond command progress");
        ensure!(self.ts >= 0 && self.ts <= last.ts, "history event time beyond command progress");
        ensure!(self.ts >= state.horizon(), "expired history event remains in a list");
        self.payload.validate()
    }

    /// Identifies the all-kind list.
    pub fn all_list(&self) -> ListId {
        ListId { account: self.account, kind: None }
    }

    /// Identifies the individual-kind list.
    pub fn kind_list(&self) -> ListId {
        ListId { account: self.account, kind: Some(self.kind) }
    }

    /// Scope includes namespace (in Store), account, kind, original event cursor and chunk ordinal.
    pub fn chunk_key(&self, ordinal: u64) -> Vec<u8> {
        let mut key = b"history/v2/payload/".to_vec();
        key.extend_from_slice(&self.account.0);
        key.push(self.kind);
        key.extend_from_slice(&self.cursor.seq.to_be_bytes());
        key.extend_from_slice(&self.cursor.index.to_be_bytes());
        key.extend_from_slice(&ordinal.to_be_bytes());
        key
    }

    /// Restores the unchanged public API shape after the complete event was verified.
    pub fn into_event(self, event: Event) -> HistoryEvent {
        HistoryEvent { cursor: self.cursor, ts: self.ts, event }
    }
}

/// Resolves an existing public event kind to its storage tag.
/// Error cases:
/// - unknown kind or an unrepresentable tag
pub(super) fn kind_id(kind: &str) -> Result<u8> {
    let index = KINDS.iter().position(|known| *known == kind).context("unknown history kind")?;
    u8::try_from(index).context("history kind tag overflow")
}

/// Selects only the type tag without materializing an unbounded event JSON value.
/// Error cases:
/// - non-account event or kind missing from the existing public set
pub(super) fn event_kind(event: &Event) -> Result<u8> {
    let kind = match event {
        Event::Deposited { .. } => "deposited",
        Event::DepositRejected { .. } => "deposit_rejected",
        Event::WithdrawalApproved { .. } => "withdrawal_approved",
        Event::WithdrawalRejected { .. } => "withdrawal_rejected",
        Event::OrderAccepted { .. } => "order_accepted",
        Event::OrderRejected { .. } => "order_rejected",
        Event::OrderCancelled { .. } => "order_cancelled",
        Event::Fill(_) => "fill",
        Event::CloseOut { .. } => "close_out",
        Event::BadDebt { .. } => "bad_debt",
        Event::Financing { .. } => "financing",
        Event::EmissionAllocated { .. } => "emission_allocated",
        // Listed, not matched by a wildcard, so a new event does not compile until it is classified.
        Event::HouseCapitalMoved { .. }
        | Event::BenchmarkRateMissing { .. }
        | Event::BenchmarkRateObserved { .. }
        | Event::BenchmarkRateUndated { .. }
        | Event::MarketOpened { .. }
        | Event::MarketClosed { .. }
        | Event::StressChanged { .. }
        | Event::PricesRejected { .. }
        | Event::AdminApplied
        | Event::AdminRejected { .. }
        | Event::EmissionPeriodClosed { .. }
        | Event::EmissionPeriodsCancelled { .. } => bail!("event is not an account-history kind"),
    };
    kind_id(kind)
}

/// Addresses one chronological queue ordinal outside the Store's reserved internal prefix.
pub(super) fn queue_key(ordinal: u64) -> Vec<u8> {
    let mut key = b"history/v2/queue/".to_vec();
    key.extend_from_slice(&ordinal.to_be_bytes());
    key
}
