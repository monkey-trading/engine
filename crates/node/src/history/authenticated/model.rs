//! Fixed-size list/queue headers and authenticated, scope-qualified payload chunk addresses. A header is
//! stored as its fields in order, each fixed-width and big-endian (`Stored`), a few dozen bytes each.

use anyhow::{Context, Result, bail, ensure};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};

use crate::history::authenticated::state::State;
use crate::history::{HistoryEvent, KINDS};
use crate::log::Offset;

pub(super) const PAYLOAD_CHUNK_BYTES: usize = crate::authenticated::CHUNK_BYTES;

/// A value the index stores under a key: its bytes, and back from exactly those bytes.
pub(super) trait Stored: Sized {
    /// The value's stored bytes.
    fn encode(&self) -> Vec<u8>;

    /// The value whose stored bytes are `bytes`.
    /// Error cases:
    /// - the bytes are not exactly one value's
    fn decode(bytes: &[u8]) -> Result<Self>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ListId {
    pub account: AccountId,
    pub kind: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Metadata {
    pub generation: u64,
    pub lo: u64,
    pub hi: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub generation: u64,
    pub rank: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Payload {
    pub length: u64,
    pub chunks: u64,
}

/// Dense lists carry headers, never unbounded serialized events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub account: AccountId,
    pub kind: u8,
    pub cursor: Offset,
    pub ts: Millis,
    pub payload: Payload,
}

/// A queued event's lists and its rank in each, from which expiry reads its header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Queued {
    pub account: AccountId,
    pub kind: u8,
    pub all: Position,
    pub by_kind: Position,
}

/// Reads a stored value's fixed-width fields in order.
struct Fields<'a>(&'a [u8]);

impl Stored for Metadata {
    /// Its generation, lo and hi.
    fn encode(&self) -> Vec<u8> {
        [self.generation, self.lo, self.hi].iter().flat_map(|field| field.to_be_bytes()).collect()
    }

    /// Error cases:
    /// - the bytes are not exactly three counters
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut fields = Fields(bytes);
        let metadata = Self { generation: fields.u64()?, lo: fields.u64()?, hi: fields.u64()? };
        fields.end()?;
        Ok(metadata)
    }
}

impl Stored for Row {
    /// Its account, kind, cursor's seq and index, time, and payload's length and chunks.
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(57);
        bytes.extend_from_slice(&self.account.0);
        bytes.push(self.kind);
        bytes.extend_from_slice(&self.cursor.seq.to_be_bytes());
        bytes.extend_from_slice(&self.cursor.index.to_be_bytes());
        bytes.extend_from_slice(&self.ts.to_be_bytes());
        bytes.extend_from_slice(&self.payload.length.to_be_bytes());
        bytes.extend_from_slice(&self.payload.chunks.to_be_bytes());
        bytes
    }

    /// Error cases:
    /// - the bytes are not exactly one row's fields
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut fields = Fields(bytes);
        let row = Self {
            account: AccountId(fields.take()?),
            kind: fields.take::<1>()?[0],
            cursor: Offset { seq: fields.u64()?, index: u32::from_be_bytes(fields.take()?) },
            ts: i64::from_be_bytes(fields.take()?),
            payload: Payload { length: fields.u64()?, chunks: fields.u64()? },
        };
        fields.end()?;
        Ok(row)
    }
}

impl Stored for Queued {
    /// Its account, kind, and the all-kind and individual-kind lists' generations and ranks.
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(53);
        bytes.extend_from_slice(&self.account.0);
        bytes.push(self.kind);
        for position in [self.all, self.by_kind] {
            bytes.extend_from_slice(&position.generation.to_be_bytes());
            bytes.extend_from_slice(&position.rank.to_be_bytes());
        }
        bytes
    }

    /// Error cases:
    /// - the bytes are not exactly one queue item's fields
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut fields = Fields(bytes);
        let queued = Self {
            account: AccountId(fields.take()?),
            kind: fields.take::<1>()?[0],
            all: Position { generation: fields.u64()?, rank: fields.u64()? },
            by_kind: Position { generation: fields.u64()?, rank: fields.u64()? },
        };
        fields.end()?;
        Ok(queued)
    }
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

impl Queued {
    /// Identifies the all-kind list.
    pub fn all_list(&self) -> ListId {
        ListId { account: self.account, kind: None }
    }

    /// Identifies the individual-kind list.
    pub fn kind_list(&self) -> ListId {
        ListId { account: self.account, kind: Some(self.kind) }
    }
}

impl Fields<'_> {
    /// The next `N` bytes.
    /// Error cases:
    /// - fewer are left
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        ensure!(self.0.len() >= N, "short stored history value");
        let (taken, rest) = self.0.split_at(N);
        self.0 = rest;
        Ok(taken.try_into()?)
    }

    /// The next big-endian `u64`.
    /// Error cases:
    /// - fewer than eight bytes are left
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take()?))
    }

    /// Checks no byte is left.
    /// Error cases:
    /// - a byte is left
    fn end(&self) -> Result<()> {
        ensure!(self.0.is_empty(), "bytes past a stored history value");
        Ok(())
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
        Event::WriteOffRepaid { .. } => "write_off_repaid",
        Event::Financing { .. } => "financing",
        Event::EmissionAllocated { .. } => "emission_allocated",
        Event::CustodyDepositChanged { .. } => "custody_deposit_changed",
        Event::CustodyDepositRejected { .. } => "custody_deposit_rejected",
        Event::WithdrawalQueued { .. } => "withdrawal_queued",
        Event::WithdrawalQueueUpdated { .. } => "withdrawal_queue_updated",
        Event::CorporateDividend { .. } => "corporate_dividend",
        Event::CorporateCashInLieu { .. } => "corporate_cash_in_lieu",
        Event::Staked { .. } => "staked",
        Event::Unstaked { .. } => "unstaked",
        Event::StakingRejected { .. } => "staking_rejected",
        Event::DistributionAllocated { .. } => "distribution_allocated",
        Event::OperatorShareAllocated { .. } => "operator_share_allocated",
        Event::OperatorShareWithheld { .. } => "operator_share_withheld",
        Event::EligibilityAttested { .. } => "eligibility_attested",
        Event::EligibilityRejected { .. } => "eligibility_rejected",
        Event::DepositPending { .. } => "deposit_pending",
        Event::DepositHeld { .. } => "deposit_held",
        Event::SessionKeyRegistered { .. } => "session_key_registered",
        Event::SessionKeyRevoked { .. } => "session_key_revoked",
        Event::SessionKeyRejected { .. } => "session_key_rejected",
        Event::PositionLevelsSet { .. } => "position_levels_set",
        Event::PositionLevelTriggered { .. } => "position_level_triggered",
        Event::PositionLevelsRemoved { .. } => "position_levels_removed",
        Event::AwardApproved(_) => "award_approved",
        Event::AwardPaid(_) => "award_paid",
        // Listed, not matched by a wildcard, so a new event does not compile until it is classified.
        Event::HouseCapitalMoved { .. }
        | Event::CorporateActionScheduled { .. }
        | Event::CorporateEntitlementsCaptured { .. }
        | Event::CorporateSplit { .. }
        | Event::CorporateActionApplied { .. }
        | Event::CorporateActionFailed { .. }
        | Event::FinancingApplied { .. }
        | Event::BenchmarkRateMissing { .. }
        | Event::BenchmarkRateObserved { .. }
        | Event::BenchmarkRateUndated { .. }
        | Event::MarketOpened { .. }
        | Event::MarketClosed { .. }
        | Event::MarketRolled { .. }
        | Event::StressChanged { .. }
        | Event::HouseLossChanged { .. }
        | Event::PricesRejected { .. }
        | Event::AdminApplied
        | Event::AdminRejected { .. }
        | Event::EmissionPeriodClosed { .. }
        | Event::EmissionPeriodsCancelled { .. }
        | Event::DistributionEpochClosed { .. }
        | Event::DistributionEpochsSkipped { .. }
        | Event::RotationSwitched { .. }
        | Event::OperatorElected { .. }
        | Event::OperatorInstalled { .. }
        | Event::ElectionRejected { .. }
        | Event::RotationSetAdopted { .. }
        | Event::TurnAssigned { .. }
        | Event::TurnRefused { .. }
        | Event::TurnTaken { .. }
        | Event::TurnTakenRejected { .. }
        | Event::ProposalScheduled { .. }
        | Event::RestrictionApplied { .. }
        | Event::ProposalRefused { .. }
        | Event::RestrictionLapsed { .. }
        | Event::OperatorFaultProven { .. }
        | Event::OperatorFaultRefused { .. }
        | Event::StateCarried(_)
        | Event::CatchUpDeferred { .. } => bail!("event is not an account-history kind"),
    };
    kind_id(kind)
}

/// Addresses one chronological queue ordinal outside the Store's reserved internal prefix.
pub(super) fn queue_key(ordinal: u64) -> Vec<u8> {
    let mut key = b"history/v2/queue/".to_vec();
    key.extend_from_slice(&ordinal.to_be_bytes());
    key
}
