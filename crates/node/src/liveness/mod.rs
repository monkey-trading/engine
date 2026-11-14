//! Minute-scale liveness (#595). While a daily turn of the schedule names the operator and its set
//! has another member, the operator's enclave:
//! - signs a log head, or a checkpoint, only at a seq another member's enclave has confirmed holding
//!   (`operator`), and stops taking commands while its log runs more than one heartbeat interval of
//!   sequenced time past the newest confirmation;
//! - posts a v2 head at least twice per heartbeat interval (`log_heads`), held through a
//!   benchmark-sync hold or a trusted-time halt, so the heads are its heartbeat;
//! - fences itself by its own clock (`fence`): from each read of the chain it knows `E`, the earliest
//!   block time a promotion could pass, and stops taking commands, signing answers and signing heads
//!   once its upper bound on true time reaches `E - DELTA`;
//! - before it sequences after a start, asks the other members for the newest head and confirmed seq
//!   they hold, and stays stopped if one holds more than its log or a conflicting link.
//!
//! Every member's node, between its turns, follows the operator, confirms what its copy durably holds
//! and answers on its registered handover endpoint (`link`), witnesses each slot and promotes once the
//! operator misses its heartbeats (`caller`), and relays the operator's signed heads when its posts
//! fail. A member the schedule promotes starts in process from its copy (`promoted`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use alloy::primitives::{Address, B256, keccak256};
use engine_core::types::AccountId;

pub(crate) mod caller;
pub(crate) mod chain;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod fence;
pub(crate) mod link;
pub(crate) mod operator;

/// The stop rule's margin: the bound assumed on how far Arbitrum block timestamps lead true time
/// (10 s, pinned in the runbook), plus the enclave's time to stop (5 s).
pub(crate) const DELTA: Duration = Duration::from_secs(15);
/// The slowest rate, in parts per million, the drift assumption lets the enclave's monotonic clock run
/// against true time; the fence widens monotonic time by it.
pub(crate) const MAX_SLOW_PPM: u64 = 500;
/// The longest a head takes from signing to inclusion, which the rule's detection window must exceed
/// twice beside `DELTA`.
pub(crate) const HEAD_ROUND_TRIP: Duration = Duration::from_secs(30);
/// The domain a member's status digest starts with.
const STATUS_DOMAIN: &[u8] = b"engine.liveness.status.v1";

/// What sequencing, signed answers and heads wait on while the liveness rule applies. Open by
/// default: a node the rule does not apply to never closes it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Gate(Arc<GateState>);

#[derive(Debug)]
struct GateState {
    /// The enclave's upper bound on true time has reached `E - DELTA`, no read has given it an `E`,
    /// the vault no longer names its key, or the rule's parameters are refused: nothing is signed.
    fenced: AtomicBool,
    /// No start check has passed since this process started: nothing is sequenced.
    unchecked: AtomicBool,
    /// The log runs more than one heartbeat interval of sequenced time past the newest confirmed seq.
    lagging: AtomicBool,
    /// The newest seq another member confirmed holding, which a checkpoint may not pass; `u64::MAX`
    /// while the rule does not apply.
    confirmed: AtomicU64,
}

/// The newest head the sequencer released: its seq and cumulative prefix, and the stamp at it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Released {
    pub head: crate::log_heads::Head,
    /// The stamp of the command at `head.seq`, in milliseconds.
    pub stamp: u64,
}

/// A head the operator may sign: the newest seq a member confirmed, or its tip with no other member
/// to ask, with the stamp the log holds there and the confirming member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Signable {
    pub released: Released,
    /// The member whose enclave confirmed it; zero with no other member in the turn's set.
    pub confirmer: Address,
}

/// The vault's signer epoch and the key it names, at the latest block the rule read: a client takes
/// an answer only from that key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct VaultSigner {
    pub epoch: u64,
    pub address: AccountId,
}

/// What a member's node states about its copy under a signer epoch: the seq and cumulative prefix its
/// copy durably holds, and the newest head of the epoch it has seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    pub epoch: u64,
    /// The copy's durable head: its last command's seq and the cumulative prefix after it.
    pub durable: crate::log_heads::Head,
    /// The newest head of the epoch the member has seen, `None` with none.
    pub seen: Option<crate::log_heads::Head>,
}

impl Default for GateState {
    /// Open in every respect.
    fn default() -> Self {
        Self {
            fenced: AtomicBool::new(false),
            unchecked: AtomicBool::new(false),
            lagging: AtomicBool::new(false),
            confirmed: AtomicU64::new(u64::MAX),
        }
    }
}

impl Gate {
    /// A gate closed in every respect, for an operator the rule may apply to until its first read
    /// says whether it does.
    pub(crate) fn closed() -> Self {
        let state = GateState {
            fenced: AtomicBool::new(true),
            unchecked: AtomicBool::new(true),
            lagging: AtomicBool::new(false),
            confirmed: AtomicU64::new(0),
        };
        Self(Arc::new(state))
    }

    /// The newest seq another member confirmed holding: a checkpoint is signed at no later seq.
    pub(crate) fn confirmed(&self) -> u64 {
        self.0.confirmed.load(Ordering::SeqCst)
    }

    /// Records the newest confirmed seq.
    pub(crate) fn confirm(&self, seq: u64) {
        self.0.confirmed.store(seq, Ordering::SeqCst);
    }

    /// Whether the enclave may sign answers and heads.
    pub(crate) fn signs(&self) -> bool {
        !self.0.fenced.load(Ordering::SeqCst)
    }

    /// Whether the sequencer may admit a command.
    pub(crate) fn sequences(&self) -> bool {
        self.signs() && !self.0.unchecked.load(Ordering::SeqCst) && !self.0.lagging.load(Ordering::SeqCst)
    }

    /// Sets the fence.
    pub(crate) fn fence(&self, fenced: bool) {
        self.0.fenced.store(fenced, Ordering::SeqCst);
    }

    /// Records whether a start check has passed.
    pub(crate) fn checked(&self, checked: bool) {
        self.0.unchecked.store(!checked, Ordering::SeqCst);
    }

    /// Records whether the log runs too far past the newest confirmation.
    pub(crate) fn lag(&self, lagging: bool) {
        self.0.lagging.store(lagging, Ordering::SeqCst);
    }

    /// Whether a start check has passed.
    pub(crate) fn start_checked(&self) -> bool {
        !self.0.unchecked.load(Ordering::SeqCst)
    }

    /// Opens every part: the rule does not apply.
    pub(crate) fn open(&self) {
        self.fence(false);
        self.checked(true);
        self.lag(false);
        self.confirm(u64::MAX);
    }
}

impl Status {
    /// The digest `member`'s standby key signs over this status, for the asker's `nonce`, in the
    /// deployment `vault` on `chain_id`: so a reply recorded for one question answers no other.
    pub(crate) fn digest(&self, chain_id: u64, vault: Address, member: AccountId, nonce: &[u8; 32]) -> [u8; 32] {
        let (seen_seq, seen_link) = self.seen.map_or((0, B256::ZERO), |seen| (seen.seq, seen.link));
        let mut packed = STATUS_DOMAIN.to_vec();
        packed.extend_from_slice(&chain_id.to_be_bytes());
        packed.extend_from_slice(vault.as_slice());
        packed.extend_from_slice(&member.0);
        packed.extend_from_slice(&self.epoch.to_be_bytes());
        packed.extend_from_slice(&self.durable.seq.to_be_bytes());
        packed.extend_from_slice(self.durable.link.as_slice());
        packed.extend_from_slice(&seen_seq.to_be_bytes());
        packed.extend_from_slice(seen_link.as_slice());
        packed.extend_from_slice(nonce);
        keccak256(packed).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_gate_is_open_and_a_closed_one_opens_part_by_part() {
        let gate = Gate::default();
        assert!(gate.signs() && gate.sequences());
        assert_eq!(gate.confirmed(), u64::MAX, "an open gate holds no checkpoint back");
        let gate = Gate::closed();
        assert!(!gate.signs() && !gate.sequences());
        assert_eq!(gate.confirmed(), 0);
        gate.fence(false);
        assert!(gate.signs() && !gate.sequences(), "the start check still holds sequencing");
        gate.checked(true);
        assert!(gate.sequences());
        gate.lag(true);
        assert!(gate.signs() && !gate.sequences(), "a lagging log stops commands, not answers");
        gate.lag(false);
        gate.fence(true);
        assert!(!gate.signs() && !gate.sequences(), "the fence stops everything");
    }

    #[test]
    fn a_status_digest_binds_the_member_the_epoch_the_heads_and_the_nonce() {
        let status = Status {
            epoch: 3,
            durable: crate::log_heads::Head { seq: 10, link: B256::repeat_byte(1) },
            seen: Some(crate::log_heads::Head { seq: 8, link: B256::repeat_byte(2) }),
        };
        let (vault, member, nonce) = (Address::repeat_byte(7), AccountId([9; 20]), [4; 32]);
        let digest = status.digest(1, vault, member, &nonce);
        assert_ne!(digest, status.digest(1, vault, AccountId([8; 20]), &nonce));
        assert_ne!(digest, status.digest(1, vault, member, &[5; 32]));
        assert_ne!(digest, Status { epoch: 4, ..status }.digest(1, vault, member, &nonce));
        assert_ne!(digest, Status { seen: None, ..status }.digest(1, vault, member, &nonce));
        assert_ne!(digest, status.digest(2, vault, member, &nonce));
    }
}
