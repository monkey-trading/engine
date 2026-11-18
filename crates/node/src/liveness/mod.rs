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
//! fail; for its whole life, following or holding a turn, it fixes the fallback of a turn no relay
//! seeded (`fallback`). A member the schedule promotes starts in process from its copy (`promoted`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use alloy::primitives::{Address, B256, keccak256};
use engine_core::types::AccountId;

pub(crate) mod caller;
pub(crate) mod chain;
#[cfg(test)]
pub(crate) mod fake;
pub(crate) mod fallback;
pub(crate) mod fence;
pub(crate) mod link;
pub(crate) mod operator;

/// The stop rule's margin: the bound assumed on how far Arbitrum block timestamps lead true time
/// (10 s, pinned in the runbook), plus the enclave's time to stop (5 s). `OperatorSchedule`'s
/// `STOP_MARGIN` keeps a skip's take before it; a change to either changes both.
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
    /// Stopped for good, whatever the fence shows: the promotion that made this key the signer is gone.
    stopped: AtomicBool,
    /// No start check has passed since this process started: nothing is sequenced.
    unchecked: AtomicBool,
    /// The log runs more than one heartbeat interval of sequenced time past the newest confirmed seq.
    lagging: AtomicBool,
    /// The newest seq another member confirmed holding, which a checkpoint may not pass; `u64::MAX`
    /// while the rule does not apply.
    confirmed: AtomicU64,
    /// The latest stamp a command may carry, in milliseconds: one heartbeat interval past the first
    /// command past the newest confirmed seq, or past the last round's trusted time with none;
    /// `u64::MAX` while the rule does not apply. The sequencer checks it at each stamp, so a failed
    /// read cannot hold it open.
    stamp_bound: AtomicU64,
    /// The seq of the checkpoint that waits for a confirmation, with nothing stamped past it; zero
    /// with none.
    wanted: AtomicU64,
    /// Wakes the operator's rule when a checkpoint starts to wait.
    wake: tokio::sync::Notify,
    /// Latched once a read shows the vault naming another key: the node ends its run.
    replaced: tokio::sync::watch::Sender<bool>,
    /// `E` in milliseconds, which every check reads against `clock` itself, so nothing is signed past
    /// `E - DELTA` however late the fence tick runs; `u64::MAX` with none.
    deadline: AtomicU64,
    /// The trusted clock the deadline is read against, which the operator's rule sets once.
    clock: std::sync::OnceLock<crate::trusted_time::TrustedClock>,
    /// The time in milliseconds from which another set than the last read's may apply, past which no
    /// checkpoint passes and no head is signed on that read's confirmation or exemption until a fresh
    /// read renews it; `u64::MAX` with none.
    holds_until: AtomicU64,
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
    /// The signer epoch and rotation set the confirmation was given under; `None` for the tip with no
    /// other member to ask.
    pub scope: Option<(u64, u32)>,
    /// The time in milliseconds from which another set than the one it was given under may apply
    /// (`Gate::holds_until`): nothing is signed at it once trusted time may have reached it.
    pub until: u64,
}

/// The vault's signer epoch and the key it names, at the latest block the rule read: a client takes
/// an answer only from that key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct VaultSigner {
    pub epoch: u64,
    pub address: AccountId,
}

/// What a member's node states about its copy under a signer epoch: the seq and cumulative prefix its
/// copy durably holds, the key that sealed that command, and the newest head of the epoch it has seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    pub epoch: u64,
    /// The copy's durable head: its last command's seq and the cumulative prefix after it; `None`
    /// while the copy holds no command, as a member's does before the operator's first reaches it.
    pub durable: Option<crate::log_heads::Head>,
    /// The key that sealed the durable head's command, zero when the copy does not know it: a failed
    /// operator restarted as a member holds a tail its own, earlier key sealed.
    pub sealer: AccountId,
    /// The newest head of the epoch the member has seen, `None` with none.
    pub seen: Option<crate::log_heads::Head>,
}

impl Default for GateState {
    /// Open in every respect.
    fn default() -> Self {
        Self {
            fenced: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            unchecked: AtomicBool::new(false),
            lagging: AtomicBool::new(false),
            confirmed: AtomicU64::new(u64::MAX),
            stamp_bound: AtomicU64::new(u64::MAX),
            wanted: AtomicU64::new(0),
            wake: tokio::sync::Notify::new(),
            replaced: tokio::sync::watch::channel(false).0,
            deadline: AtomicU64::new(u64::MAX),
            clock: std::sync::OnceLock::new(),
            holds_until: AtomicU64::new(u64::MAX),
        }
    }
}

impl Gate {
    /// A gate closed in every respect, for an operator the rule may apply to until its first read
    /// says whether it does.
    pub(crate) fn closed() -> Self {
        let state = GateState {
            fenced: AtomicBool::new(true),
            stopped: AtomicBool::new(false),
            unchecked: AtomicBool::new(true),
            lagging: AtomicBool::new(false),
            confirmed: AtomicU64::new(0),
            stamp_bound: AtomicU64::new(0),
            wanted: AtomicU64::new(0),
            wake: tokio::sync::Notify::new(),
            replaced: tokio::sync::watch::channel(false).0,
            deadline: AtomicU64::new(u64::MAX),
            clock: std::sync::OnceLock::new(),
            holds_until: AtomicU64::new(u64::MAX),
        };
        Self(Arc::new(state))
    }

    /// Records `E`, `due_ms` (`None` before the first read, 0 once the vault named another key), which
    /// every check then reads against `clock`'s upper bound on true time as the fence tick does.
    pub(crate) fn deadline(&self, due_ms: Option<u64>, clock: &crate::trusted_time::TrustedClock) {
        let _ = self.0.clock.set(clock.clone());
        self.0.deadline.store(due_ms.unwrap_or(0), Ordering::SeqCst);
    }

    /// Whether the enclave's upper bound on true time has reached the recorded `E - DELTA`, or cannot
    /// be read; false with no deadline recorded.
    fn past_deadline(&self) -> bool {
        let due = self.0.deadline.load(Ordering::SeqCst);
        let delta = u64::try_from(DELTA.as_millis()).expect("DELTA fits in milliseconds");
        due != u64::MAX && self.reached(due.saturating_sub(delta))
    }

    /// Whether the enclave's upper bound on true time has reached `at_ms`, or cannot be read; false
    /// before the rule set the clock.
    fn reached(&self, at_ms: u64) -> bool {
        let Some(clock) = self.0.clock.get() else { return false };
        let upper = clock.upper_bound_ms(MAX_SLOW_PPM).and_then(|upper| u64::try_from(upper).ok());
        upper.is_none_or(|upper| upper >= at_ms)
    }

    /// Latches that the vault names another key than this enclave's, which ends the node's run: a
    /// member goes back to following, or starts as the operator a promotion of its standby key made it.
    pub(crate) fn replace(&self) {
        self.0.replaced.send_replace(true);
    }

    /// Whether the vault named another key (`replace`), as it changes.
    pub(crate) fn replacement(&self) -> tokio::sync::watch::Receiver<bool> {
        self.0.replaced.subscribe()
    }

    /// The newest seq another member of the last read's set confirmed holding, or any under an exempt
    /// read: a checkpoint is signed at no later seq. Either stands only until another set may apply
    /// (`holds_until`), and none past it until a fresh read renews it.
    pub(crate) fn confirmed(&self) -> u64 {
        match self.before(self.0.holds_until.load(Ordering::SeqCst)) {
            true => self.0.confirmed.load(Ordering::SeqCst),
            false => 0,
        }
    }

    /// Whether the enclave's upper bound on true time has certainly not reached `at_ms`, `u64::MAX`
    /// never coming: a permission that lapses at `at_ms` still stands.
    pub(crate) fn before(&self, at_ms: u64) -> bool {
        at_ms == u64::MAX || !self.reached(at_ms)
    }

    /// Records `until_ms`, from which another set than the read's may apply: the confirmations taken
    /// under that read let checkpoints pass, and heads be signed, only until then.
    pub(crate) fn holds_until(&self, until_ms: u64) {
        self.0.holds_until.store(until_ms, Ordering::SeqCst);
    }

    /// Records the newest confirmed seq.
    pub(crate) fn confirm(&self, seq: u64) {
        self.0.confirmed.store(seq, Ordering::SeqCst);
    }

    /// Records that the checkpoint at `seq` waits for a confirmation, and wakes the operator's rule to
    /// ask for one at once.
    pub(crate) fn want(&self, seq: u64) {
        self.0.wanted.fetch_max(seq, Ordering::SeqCst);
        self.0.wake.notify_one();
    }

    /// The seq of the newest checkpoint that waited for a confirmation.
    pub(crate) fn wanted(&self) -> u64 {
        self.0.wanted.load(Ordering::SeqCst)
    }

    /// The seq a checkpoint waits for a confirmation of, while one does.
    pub(crate) fn waiting(&self) -> Option<u64> {
        let wanted = self.wanted();
        (wanted > self.confirmed()).then_some(wanted)
    }

    /// Resolves when a checkpoint starts to wait (`want`), or at once when one started since the last
    /// call.
    pub(crate) async fn woken(&self) {
        self.0.wake.notified().await;
    }

    /// Records the latest stamp a command may carry: one heartbeat interval past the stamp of the first
    /// command after the newest confirmed seq.
    pub(crate) fn bound_stamps(&self, stamp: u64) {
        self.0.stamp_bound.store(stamp, Ordering::SeqCst);
    }

    /// Whether a command stamped `ts` may be admitted: the gate sequences, and `ts` is within the stamp
    /// bound (`bound_stamps`).
    pub(crate) fn admits(&self, ts: engine_core::types::Millis) -> bool {
        self.sequences() && u64::try_from(ts).is_ok_and(|ts| ts <= self.0.stamp_bound.load(Ordering::SeqCst))
    }

    /// Whether the enclave may sign answers and heads.
    pub(crate) fn signs(&self) -> bool {
        !self.0.fenced.load(Ordering::SeqCst) && !self.0.stopped.load(Ordering::SeqCst) && !self.past_deadline()
    }

    /// Stops signing and sequencing for good: no fence tick reopens it.
    pub(crate) fn stop(&self) {
        self.0.stopped.store(true, Ordering::SeqCst);
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

    /// Opens every part but the fence, which the fence tick keeps: a read under which the rule does
    /// not apply, until `until_ms`, from which another set may apply (see `confirmed`).
    pub(crate) fn exempt(&self, until_ms: u64) {
        self.holds_until(until_ms);
        self.checked(true);
        self.lag(false);
        self.confirm(u64::MAX);
        self.bound_stamps(u64::MAX);
    }

    /// Opens every part: the rule does not apply.
    #[cfg(test)]
    pub(crate) fn open(&self) {
        self.fence(false);
        self.checked(true);
        self.lag(false);
        self.confirm(u64::MAX);
        self.bound_stamps(u64::MAX);
    }
}

impl Status {
    /// The digest `member`'s standby key signs over this status, for the asker's `nonce`, in the
    /// deployment `vault` on `chain_id`: so a reply recorded for one question answers no other.
    pub(crate) fn digest(&self, chain_id: u64, vault: Address, member: AccountId, nonce: &[u8; 32]) -> [u8; 32] {
        let (seen_seq, seen_link) = self.seen.map_or((0, B256::ZERO), |seen| (seen.seq, seen.link));
        let (durable_seq, durable_link) = self.durable.map_or((0, B256::ZERO), |durable| (durable.seq, durable.link));
        let mut packed = STATUS_DOMAIN.to_vec();
        packed.extend_from_slice(&chain_id.to_be_bytes());
        packed.extend_from_slice(vault.as_slice());
        packed.extend_from_slice(&member.0);
        packed.extend_from_slice(&self.epoch.to_be_bytes());
        packed.extend_from_slice(&durable_seq.to_be_bytes());
        packed.extend_from_slice(durable_link.as_slice());
        packed.extend_from_slice(&self.sealer.0);
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
        gate.fence(false);
        gate.stop();
        gate.fence(false);
        gate.open();
        assert!(!gate.signs() && !gate.sequences(), "a stop holds whatever the fence and the rule set later");
    }

    /// The deadline is read at every check, against the enclave's own clock: a gate the fence tick last
    /// left open stops signing and sequencing at `E - DELTA` though no tick runs, and a later `E` reopens
    /// it.
    #[test]
    fn the_gate_closes_at_its_deadline_by_its_own_clock_however_late_the_tick() {
        use crate::clock::Clock;
        use crate::clock::test_clock::TestClock;
        use crate::trusted_time::{Sample, TrustedClock};

        const DUE_MS: u64 = 1_800_000_060_000;
        let clock = Arc::new(TestClock::at(1_800_000_000_000));
        let sample = Sample { at: clock.instant(), earliest_ms: 1_800_000_000_000, latest_ms: 1_800_000_000_000 };
        let trusted =
            TrustedClock::new(clock.clone(), tokio::sync::watch::channel(Some(sample)).1, Duration::from_secs(10));
        let gate = Gate::default();
        gate.deadline(Some(DUE_MS), &trusted);
        assert!(gate.signs() && gate.sequences(), "45 s before E - DELTA");
        clock.advance(Duration::from_secs(45));
        assert!(!gate.signs() && !gate.sequences(), "past E - DELTA with no tick, the gate stayed open");
        gate.deadline(Some(DUE_MS + 60_000), &trusted);
        assert!(gate.signs(), "a later E reopens it");
        gate.deadline(None, &trusted);
        assert!(!gate.signs(), "no E, or a vault naming another key, signs nothing");
    }

    #[test]
    fn a_status_digest_binds_the_member_the_epoch_the_heads_the_sealer_and_the_nonce() {
        let status = Status {
            epoch: 3,
            durable: Some(crate::log_heads::Head { seq: 10, link: B256::repeat_byte(1) }),
            sealer: AccountId([6; 20]),
            seen: Some(crate::log_heads::Head { seq: 8, link: B256::repeat_byte(2) }),
        };
        let (vault, member, nonce) = (Address::repeat_byte(7), AccountId([9; 20]), [4; 32]);
        let digest = status.digest(1, vault, member, &nonce);
        assert_ne!(digest, status.digest(1, vault, AccountId([8; 20]), &nonce));
        assert_ne!(digest, status.digest(1, vault, member, &[5; 32]));
        assert_ne!(digest, Status { epoch: 4, ..status }.digest(1, vault, member, &nonce));
        assert_ne!(digest, Status { seen: None, ..status }.digest(1, vault, member, &nonce));
        assert_ne!(digest, Status { durable: None, ..status }.digest(1, vault, member, &nonce));
        assert_ne!(digest, Status { sealer: AccountId([5; 20]), ..status }.digest(1, vault, member, &nonce));
        assert_ne!(digest, status.digest(2, vault, member, &nonce));
    }
}
