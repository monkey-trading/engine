//! `engine.log-head.v2` heads (#595): while a turn of the schedule names the operator, a head is the
//! heartbeat. Beside v1's `(seq, link, parent)` it binds the turn assignment whose keys the enclave
//! holds, its release and the assignment's generation, the stamp the log holds at `seq`, the signing
//! time (trusted time's lower bound), the hold it is signed under, the member whose enclave confirmed
//! holding the log through `seq`, and, for a held head, a recent block. `LogHeads.postV2` checks the
//! signature against the vault's signer in its current epoch, whoever relays the head.

use std::time::Duration;

use alloy::primitives::{Address, B256, keccak256};
use alloy::sol;
use alloy::sol_types::SolCall;
use engine_core::turns::{BOUNDARY_OFFSET_MS, TURN_MS};
use engine_core::types::Millis;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::liveness::operator::Signing;
use crate::liveness::{MAX_SLOW_PPM, Released, Signable};
use crate::log_heads::Head;
use crate::trusted_time::TrustedClock;

// `LogHeads.postV2`'s ABI, as `contracts/src/LogHeads.sol` declares it.
sol! {
    struct HeadV2Call {
        uint64 seq;
        bytes32 link;
        uint64 parentSeq;
        bytes32 parentLink;
        uint64 turn;
        uint16 position;
        bytes32 release;
        uint64 generation;
        uint64 stamp;
        uint64 signedAt;
        uint8 hold;
        address confirmer;
        uint64 blockNumber;
        bytes32 blockHash;
    }
    function postV2(HeadV2Call head, bytes signature) external;
}

/// The domain a v2 head's digest starts with.
const DOMAIN: &[u8] = b"engine.log-head.v2";
/// The shortest read period a heartbeat interval sets, against a zero interval.
const MIN_READ_EVERY: Duration = Duration::from_millis(100);
/// While no turn names the operator, how long past each turn's boundary the poster reads whether one does,
/// as the agreed block may still trail the boundary: every `BOUNDARY_READ_EVERY`, from when true time may
/// have reached the boundary until it has certainly passed it by this much.
const BOUNDARY_SEARCH_MS: Millis = 30_000;
/// How often the poster reads from a boundary while it searches.
const BOUNDARY_READ_EVERY: Duration = Duration::from_secs(1);

/// What a head is signed under: live, or held through one of the two stops a heartbeat continues
/// through, as `LogHeads` numbers them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Hold {
    /// Sequencing runs: the head's signing time is checked against the block's.
    #[default]
    Live,
    /// A benchmark-sync hold: the sequencer takes nothing but the publisher's snapshot.
    BenchmarkSync,
    /// A trusted-time halt: no sample bounds the host clock.
    TimeHalt,
}

/// An `engine.log-head.v2` head, every field `LogHeads.postV2` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadV2 {
    pub head: Head,
    /// The head it extends, `None` for the first post of the signer epoch.
    pub parent: Option<Head>,
    /// The assignment whose keys the enclave holds, zeros when it holds none the schedule names.
    pub turn: u64,
    pub position: u16,
    pub release: B256,
    pub generation: u64,
    /// The stamp the log holds at `head.seq`, in milliseconds.
    pub stamp: u64,
    /// Trusted time's lower bound when it was signed, in milliseconds.
    pub signed_at: u64,
    pub hold: Hold,
    /// The member whose enclave confirmed holding the log through `head.seq`; zero with no other
    /// member in the turn's set.
    pub confirmer: Address,
    /// A held head's recent block, zeros for a live head.
    pub block_number: u64,
    pub block_hash: B256,
}

/// A v2 head with the epoch it was signed under and its signature: what the signed stream carries
/// and what a follower relays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignedHead {
    pub epoch: u64,
    pub head: HeadV2,
    /// 65-byte `r || s || v`, 0x hex.
    pub signature: String,
}

/// What a read shows of the turn naming the operator, for a v2 head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnHeads {
    /// The heartbeat interval: a v2 head goes out twice in it (`cadence`).
    pub interval: Duration,
    /// The turn in effect, its latest assignment's position and generation, and the release the
    /// registry names for its member.
    pub turn: u64,
    pub position: u16,
    pub release: B256,
    pub generation: u64,
    /// The epoch's stored head's signing time, in milliseconds, and bound block.
    pub signed_at: u64,
    pub bound_block: u64,
    /// The block read at, which a held head binds.
    pub block_number: u64,
    pub block_hash: B256,
    /// The block that marked the latest marked slot, which a held head's block may not be older than.
    pub marked_block: u64,
    /// The rotation set the turn in effect was drawn from.
    pub set: u32,
}

/// Reads the release and generation this enclave's keys are sealed in now: a running holder's keys move
/// to a later generation when the schedule gives its next turn a fresh one.
pub(crate) type HeldKeys = std::sync::Arc<dyn Fn() -> (B256, u64) + Send + Sync>;

/// What a v2 heartbeat reads beside the chain, while rotation may name the operator.
pub(crate) struct Heartbeat {
    /// The release and generation this enclave's keys are sealed in, read at each head, `None` without
    /// sealed keys.
    pub keys: Option<HeldKeys>,
    /// Where the liveness rule lets a head be signed.
    pub signing: watch::Receiver<Signing>,
    /// The released head with its stamp.
    pub released: watch::Receiver<crate::liveness::Released>,
    pub hold: watch::Receiver<Hold>,
    pub gate: crate::liveness::Gate,
    /// Where each signed v2 head goes, for the signed stream to carry.
    pub signed: watch::Sender<Option<SignedHead>>,
    /// Trusted time, whose lower bound a head's signing time is.
    pub trusted: crate::trusted_time::TrustedClock,
}

/// A turn assignment a head binds: one whose keys the enclave holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub turn: u64,
    pub position: u16,
    pub release: B256,
    pub generation: u64,
}

/// What the poster weighs beside the chain's read when it signs a v2 head.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inputs {
    /// Where the liveness rule lets it sign.
    pub signing: Signing,
    /// The log's tip and its stamp.
    pub tip: Released,
    pub hold: Hold,
    /// The epoch's head on chain this log holds, which the head extends.
    pub parent: Option<Head>,
    /// Trusted time's lower bound, `None` before the first sample.
    pub reached: Option<i64>,
    /// The newest signing time of a head this process signed.
    pub last_signed: u64,
    /// The vault's signer epoch the head is signed under.
    pub epoch: u64,
}

/// The poster's reads from each turn's boundary while no turn names the operator, and until the first head
/// of a turn they found goes out, so that it goes out from the boundary rather than from a later read tick.
#[derive(Debug, Default)]
pub(crate) struct BoundaryReads {
    /// The boundary the last of them was for, and when it was made.
    last: Option<(Millis, Instant)>,
}

impl BoundaryReads {
    /// Waits for the next read due from a turn's boundary as the latest trusted time sample places it, on
    /// monotonic time, whatever the host clock reads: from when true time may have reached the boundary, on
    /// a monotonic clock running up to `MAX_SLOW_PPM` slow or as fast, every `BOUNDARY_READ_EVERY` until it
    /// has certainly passed it by `BOUNDARY_SEARCH_MS`, then from the next boundary. Never completes without
    /// `trusted`, nor before its first sample.
    pub(crate) async fn next(&mut self, trusted: Option<&TrustedClock>) -> Instant {
        let bounds = trusted.and_then(|trusted| trusted.drift_bounds_ms(MAX_SLOW_PPM));
        let Some((earliest, latest)) = bounds else { return std::future::pending().await };
        let now = Instant::now();
        // The latest boundary true time may have reached, and whether it may still be in the search after it.
        let boundary = latest - latest.saturating_sub(BOUNDARY_OFFSET_MS).rem_euclid(TURN_MS);
        let searching = earliest < boundary.saturating_add(BOUNDARY_SEARCH_MS);
        let (read_for, at) = match self.last {
            Some((read_for, made)) if searching && read_for == boundary => (boundary, made + BOUNDARY_READ_EVERY),
            _ if searching => (boundary, now),
            _ => (boundary + TURN_MS, now + Duration::from_millis((boundary + TURN_MS - latest).unsigned_abs())),
        };
        tokio::time::sleep_until(at).await;
        let made = Instant::now();
        self.last = Some((read_for, made));
        made
    }
}

impl Assignment {
    /// The assignment `turn` reads in effect, when its release and generation are the ones this
    /// enclave's `keys` are sealed in: it holds the keys under it, by a handover, a promotion or a
    /// repeat turn continuing from the boundary, since each generation is the schedule's once.
    pub(crate) fn held(turn: &TurnHeads, keys: Option<(B256, u64)>) -> Option<Self> {
        (keys == Some((turn.release, turn.generation))).then_some(Self {
            turn: turn.turn,
            position: turn.position,
            release: turn.release,
            generation: turn.generation,
        })
    }
}

/// The poster's read period and the pace of its heads while a turn names the operator, with the heartbeat
/// `interval` and the configured read interval `retry`: the period is at most a sixth of the interval and
/// at most `retry`, and the pace is half the interval as a whole number of periods, so the head due every
/// half interval goes out at its tick whatever the two intervals.
pub(crate) fn cadence(interval: Duration, retry: Duration) -> (Duration, Duration) {
    let most = retry.min((interval / 6).max(MIN_READ_EVERY));
    let half = interval / 2;
    let ticks = u32::try_from(half.as_nanos().div_ceil(most.as_nanos())).unwrap_or(u32::MAX).max(1);
    let period = half / ticks;
    // Only a zero interval leaves no period to divide.
    if period.is_zero() { (most, most) } else { (period, period * ticks) }
}

/// The v2 head to sign now, if any. A live head is at the seq the liveness rule lets it sign, newer
/// than the head on chain, with a signing time newer than the chain's and this process's last one. A
/// held head is at the seq the rule lets it sign too, the newest another member confirmed, repeating
/// the head on chain or newer, and binds the block read, when that is newer than the stored head's and
/// no older than the latest mark's. It may trail a tip no member received before the hold, by at most
/// the unconfirmed run the lag stop allows, since a hold stamps nothing: requiring the tip would let a
/// stream gap at a trusted-time halt, which holds every member alike, start the promotion the held
/// heads exist to prevent. Each binds `bound` when the enclave holds its keys, otherwise no assignment.
pub(crate) fn next(
    turn: &TurnHeads,
    bound: Option<Assignment>,
    keys: Option<(B256, u64)>,
    inputs: Inputs,
) -> Option<HeadV2> {
    let at = match inputs.signing {
        Signing::Nothing => return None,
        // The tip and a confirmation count only under the epoch and set they were given in: across a
        // change of set, neither is an anchor the turn's members can be promoted from.
        Signing::Tip(scope, until) if scope == Some((inputs.epoch, turn.set)) => {
            Signable { released: inputs.tip, confirmer: Address::ZERO, scope, until }
        }
        Signing::Tip(..) => return None,
        Signing::Confirmed(signable) if signable.scope.is_none_or(|scope| scope == (inputs.epoch, turn.set)) => {
            signable
        }
        Signing::Confirmed(_) => return None,
    };
    let signed_at = u64::try_from(inputs.reached?).ok()?;
    let head = at.released.head;
    let held = inputs.hold != Hold::Live;
    if held {
        let repeats = |parent: Head| head.seq > parent.seq || head == parent;
        if !inputs.parent.is_none_or(repeats) {
            return None;
        }
        if turn.block_number <= turn.bound_block || turn.block_number < turn.marked_block {
            return None;
        }
    } else {
        if inputs.parent.is_some_and(|parent| head.seq <= parent.seq) {
            return None;
        }
        if signed_at <= turn.signed_at.max(inputs.last_signed) {
            return None;
        }
    }
    let release = bound.map(|bound| bound.release).or(keys.map(|(release, _)| release)).unwrap_or_default();
    Some(HeadV2 {
        head,
        parent: inputs.parent,
        turn: bound.map_or(0, |bound| bound.turn),
        position: bound.map_or(0, |bound| bound.position),
        release,
        generation: bound.map_or(0, |bound| bound.generation),
        stamp: at.released.stamp,
        signed_at,
        hold: inputs.hold,
        confirmer: at.confirmer,
        block_number: if held { turn.block_number } else { 0 },
        block_hash: if held { turn.block_hash } else { B256::ZERO },
    })
}

impl Hold {
    /// `LogHeads`' number for it.
    pub(crate) fn code(self) -> u8 {
        match self {
            Hold::Live => 0,
            Hold::BenchmarkSync => 1,
            Hold::TimeHalt => 2,
        }
    }
}

impl HeadV2 {
    /// The digest `LogHeads._digestV2` computes for this head under `epoch` of `vault` on `chain_id`:
    /// every field packed after the domain, as `abi.encodePacked` lays them out.
    pub(crate) fn digest(&self, chain_id: u64, vault: Address, epoch: u64) -> [u8; 32] {
        let (parent_seq, parent_link) = self.parent.map_or((0, B256::ZERO), |parent| (parent.seq, parent.link));
        let mut packed = Vec::with_capacity(DOMAIN.len() + 8 * 9 + 20 * 2 + 32 * 5 + 2 + 1);
        packed.extend_from_slice(DOMAIN);
        packed.extend_from_slice(&chain_id.to_be_bytes());
        packed.extend_from_slice(vault.as_slice());
        packed.extend_from_slice(&epoch.to_be_bytes());
        packed.extend_from_slice(&self.head.seq.to_be_bytes());
        packed.extend_from_slice(self.head.link.as_slice());
        packed.extend_from_slice(&parent_seq.to_be_bytes());
        packed.extend_from_slice(parent_link.as_slice());
        packed.extend_from_slice(&self.turn.to_be_bytes());
        packed.extend_from_slice(&self.position.to_be_bytes());
        packed.extend_from_slice(self.release.as_slice());
        packed.extend_from_slice(&self.generation.to_be_bytes());
        packed.extend_from_slice(&self.stamp.to_be_bytes());
        packed.extend_from_slice(&self.signed_at.to_be_bytes());
        packed.push(self.hold.code());
        packed.extend_from_slice(self.confirmer.as_slice());
        packed.extend_from_slice(&self.block_number.to_be_bytes());
        packed.extend_from_slice(self.block_hash.as_slice());
        keccak256(packed).0
    }

    /// The `postV2` call carrying this head and `signature`.
    /// Error cases:
    /// - the signature is not hex
    pub(crate) fn calldata(&self, signature: &str) -> anyhow::Result<Vec<u8>> {
        let (parent_seq, parent_link) = self.parent.map_or((0, B256::ZERO), |parent| (parent.seq, parent.link));
        let head = HeadV2Call {
            seq: self.head.seq,
            link: self.head.link,
            parentSeq: parent_seq,
            parentLink: parent_link,
            turn: self.turn,
            position: self.position,
            release: self.release,
            generation: self.generation,
            stamp: self.stamp,
            signedAt: self.signed_at,
            hold: self.hold.code(),
            confirmer: self.confirmer,
            blockNumber: self.block_number,
            blockHash: self.block_hash,
        };
        Ok(postV2Call { head, signature: signature.parse()? }.abi_encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A head with every field set apart, so a field out of place changes the digest.
    fn head() -> HeadV2 {
        HeadV2 {
            head: Head { seq: 9, link: B256::repeat_byte(0x11) },
            parent: Some(Head { seq: 7, link: B256::repeat_byte(0x22) }),
            turn: 20_400,
            position: 3,
            release: B256::repeat_byte(0x33),
            generation: 5,
            stamp: 1_800_000_000_123,
            signed_at: 1_800_000_000_456,
            hold: Hold::TimeHalt,
            confirmer: Address::repeat_byte(0x44),
            block_number: 77,
            block_hash: B256::repeat_byte(0x55),
        }
    }

    #[test]
    fn the_digest_packs_every_field_in_the_contracts_order() {
        let h = head();
        let vault = Address::repeat_byte(0x7a);
        let mut packed = b"engine.log-head.v2".to_vec();
        for part in [
            &42161u64.to_be_bytes()[..],
            vault.as_slice(),
            &4u64.to_be_bytes(),
            &9u64.to_be_bytes(),
            &[0x11; 32],
            &7u64.to_be_bytes(),
            &[0x22; 32],
            &20_400u64.to_be_bytes(),
            &3u16.to_be_bytes(),
            &[0x33; 32],
            &5u64.to_be_bytes(),
            &1_800_000_000_123u64.to_be_bytes(),
            &1_800_000_000_456u64.to_be_bytes(),
            &[2],
            &[0x44; 20],
            &77u64.to_be_bytes(),
            &[0x55; 32],
        ] {
            packed.extend_from_slice(part);
        }
        assert_eq!(h.digest(42161, vault, 4), keccak256(packed).0);
        // A first post names no parent: both parent fields are zero.
        let first = HeadV2 { parent: None, ..h };
        assert_ne!(first.digest(42161, vault, 4), h.digest(42161, vault, 4));
    }

    /// `LogHeadsV2.t.sol`'s known-answer vector: the same head, signed by key 0x1234, gives the
    /// signature the contract's test fixes, so the digest's field order is the contract's.
    #[test]
    fn the_digest_signs_to_the_contracts_known_answer_vector() {
        let word = |value: u64| B256::from(alloy::primitives::U256::from(value));
        let head = HeadV2 {
            head: Head { seq: 42, link: word(0x2a) },
            parent: Some(Head { seq: 41, link: word(0x29) }),
            turn: 20_765,
            position: 3,
            release: word(0x72),
            generation: 9,
            stamp: 1_794_123_456_000,
            signed_at: 1_794_123_456_789,
            hold: Hold::BenchmarkSync,
            confirmer: Address::repeat_byte(0x22),
            block_number: 300_000_000,
            block_hash: word(0xb10c),
        };
        let mut secret = [0u8; 32];
        secret[30..].copy_from_slice(&[0x12, 0x34]);
        let key = crate::tee::EnclaveKey::from_secret(&secret).unwrap();
        let signature = key.sign_digest(head.digest(31_337, Address::repeat_byte(0x11), 7));
        let expected = concat!(
            "0xb1e55c9eee77638ce7547af58836fd069aacffd71ebc73b46a128b33b4fe0265",
            "77009055603c1d79cc1952e5a78119e2cab2c3d4ed30000581b9627694eb2357",
            "1c"
        );
        assert_eq!(signature, expected);
    }

    #[test]
    fn the_call_carries_the_head_and_the_signature() {
        let h = head();
        let signature = format!("0x{}", "ab".repeat(65));
        let call = postV2Call::abi_decode(&h.calldata(&signature).unwrap()).unwrap();
        assert_eq!((call.head.seq, call.head.parentSeq, call.head.hold), (9, 7, 2));
        assert_eq!((call.head.turn, call.head.position, call.head.generation), (20_400, 3, 5));
        assert_eq!(call.head.confirmer, Address::repeat_byte(0x44));
        assert_eq!(call.signature.len(), 65);
    }
}
