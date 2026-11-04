//! The Fulu `BeaconState` (consensus specification, mainnet preset), field by field: what a
//! serialized state's fields hash to, and the two branches `SeedRelay` checks from its root, to
//! `fork.current_version` and to one epoch's RANDAO mix. The state comes from a beacon API the host
//! reaches, so every length and offset is checked before it is read, and the root it gives is
//! compared with the beacon block's `state_root` before anything is sent.

use alloy::primitives::B256;
use anyhow::{Context, bail, ensure};

use crate::seed_relay::ssz::{
    branch, container_root, element_list_root, hash, merkleize, mix_in_length, pack, packed_list_root,
};

/// The state's fields fill a 64-leaf tree.
const STATE_DEPTH: usize = 6;
pub(crate) const FORK_FIELD: usize = 3;
pub(crate) const RANDAO_MIXES_FIELD: usize = 13;
const SLOT_FIELD: usize = 2;
/// `Fork` is `(previous_version, current_version, epoch)`, a 4-leaf tree.
const FORK_DEPTH: usize = 2;
pub(crate) const EPOCHS_PER_HISTORICAL_VECTOR: u64 = 65536;
const MIXES_DEPTH: usize = 16;
const SLOTS_PER_EPOCH: u64 = 32;

const VALIDATOR: [usize; 8] = [48, 32, 8, 1, 8, 8, 8, 8];
const ETH1_DATA: [usize; 3] = [32, 8, 32];
const HISTORICAL_SUMMARY: [usize; 2] = [32, 32];
const PENDING_DEPOSIT: [usize; 5] = [48, 32, 8, 96, 8];
const PENDING_PARTIAL_WITHDRAWAL: [usize; 3] = [8, 8, 8];
const PENDING_CONSOLIDATION: [usize; 2] = [8, 8];
/// `ExecutionPayloadHeader` before its `extra_data`, which sits between `timestamp` and
/// `base_fee_per_gas`.
const PAYLOAD_HEADER_HEAD: [usize; 10] = [32, 20, 32, 32, 256, 32, 8, 8, 8, 8];
const PAYLOAD_HEADER_TAIL: [usize; 6] = [32, 32, 32, 32, 8, 8];
const MAX_EXTRA_DATA_BYTES: usize = 32;

/// A field's SSZ type, as far as hashing it needs.
#[derive(Clone, Copy)]
enum Field {
    /// A fixed-size container of these field sizes, or a basic type or short byte vector (one size).
    Fixed(&'static [usize]),
    /// `Vector[Root, n]` or `Vector[Bytes32, n]`.
    Roots(usize),
    /// `Vector[uint64, n]`, packed.
    Integers(usize),
    /// `List[Root, limit]`.
    RootList(u64),
    /// A list of fixed-size containers of these field sizes.
    Elements(&'static [usize], u64),
    /// `List[uint64, limit]` or `List[uint8, limit]`: the integer size and the limit.
    PackedList(usize, u64),
    /// `SyncCommittee`: 512 public keys and their aggregate.
    SyncCommittee,
    /// `ExecutionPayloadHeader`, variable in size through its `extra_data`.
    PayloadHeader,
}

/// The Fulu `BeaconState`'s 38 fields, in order.
const FULU: [Field; 38] = [
    Field::Fixed(&[8]),                                // genesis_time
    Field::Fixed(&[32]),                               // genesis_validators_root
    Field::Fixed(&[8]),                                // slot
    Field::Fixed(&[4, 4, 8]),                          // fork
    Field::Fixed(&[8, 8, 32, 32, 32]),                 // latest_block_header
    Field::Roots(8192),                                // block_roots
    Field::Roots(8192),                                // state_roots
    Field::RootList(1 << 24),                          // historical_roots
    Field::Fixed(&ETH1_DATA),                          // eth1_data
    Field::Elements(&ETH1_DATA, 2048),                 // eth1_data_votes
    Field::Fixed(&[8]),                                // eth1_deposit_index
    Field::Elements(&VALIDATOR, 1 << 40),              // validators
    Field::PackedList(8, 1 << 40),                     // balances
    Field::Roots(65536),                               // randao_mixes
    Field::Integers(8192),                             // slashings
    Field::PackedList(1, 1 << 40),                     // previous_epoch_participation
    Field::PackedList(1, 1 << 40),                     // current_epoch_participation
    Field::Fixed(&[1]),                                // justification_bits
    Field::Fixed(&[8, 32]),                            // previous_justified_checkpoint
    Field::Fixed(&[8, 32]),                            // current_justified_checkpoint
    Field::Fixed(&[8, 32]),                            // finalized_checkpoint
    Field::PackedList(8, 1 << 40),                     // inactivity_scores
    Field::SyncCommittee,                              // current_sync_committee
    Field::SyncCommittee,                              // next_sync_committee
    Field::PayloadHeader,                              // latest_execution_payload_header
    Field::Fixed(&[8]),                                // next_withdrawal_index
    Field::Fixed(&[8]),                                // next_withdrawal_validator_index
    Field::Elements(&HISTORICAL_SUMMARY, 1 << 24),     // historical_summaries
    Field::Fixed(&[8]),                                // deposit_requests_start_index
    Field::Fixed(&[8]),                                // deposit_balance_to_consume
    Field::Fixed(&[8]),                                // exit_balance_to_consume
    Field::Fixed(&[8]),                                // earliest_exit_epoch
    Field::Fixed(&[8]),                                // consolidation_balance_to_consume
    Field::Fixed(&[8]),                                // earliest_consolidation_epoch
    Field::Elements(&PENDING_DEPOSIT, 1 << 27),        // pending_deposits
    Field::Elements(&PENDING_PARTIAL_WITHDRAWAL, 1 << 27), // pending_partial_withdrawals
    Field::Elements(&PENDING_CONSOLIDATION, 1 << 18),  // pending_consolidations
    Field::Integers(64),                               // proposer_lookahead
];

/// What proves one epoch's RANDAO mix from a state: `SeedRelay`'s witness, less the block header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MixProof {
    pub slot: u64,
    pub state_root: B256,
    pub fork_version: [u8; 4],
    /// From `fork.current_version` up to the state root.
    pub fork_proof: Vec<B256>,
    pub mix: B256,
    /// From the mix up to the state root.
    pub mix_proof: Vec<B256>,
}

impl Field {
    /// The field's size in the state's fixed part, or `None` for a variable-size field, which an
    /// offset stands in for.
    fn fixed_size(self) -> Option<usize> {
        match self {
            Field::Fixed(sizes) => Some(sizes.iter().sum()),
            Field::Roots(count) => Some(32 * count),
            Field::Integers(count) => Some(8 * count),
            Field::SyncCommittee => Some(48 * 512 + 48),
            Field::RootList(_) | Field::Elements(..) | Field::PackedList(..) | Field::PayloadHeader => None,
        }
    }

    /// The field's `hash_tree_root` from its serialization.
    /// Error cases:
    /// - the bytes do not fit the field's type
    fn root(self, bytes: &[u8]) -> anyhow::Result<[u8; 32]> {
        match self {
            Field::Fixed(sizes) if sizes.len() == 1 => {
                ensure!(bytes.len() == sizes[0], "a {}-byte field holds {} bytes", sizes[0], bytes.len());
                Ok(pack(bytes)[0])
            }
            Field::Fixed(sizes) => container_root(bytes, sizes),
            Field::Roots(count) => merkleize(pack(bytes), count as u64),
            Field::Integers(count) => merkleize(pack(bytes), (8 * count as u64).div_ceil(32)),
            Field::RootList(limit) => {
                ensure!(bytes.len() % 32 == 0, "a list of roots holds {} bytes", bytes.len());
                Ok(mix_in_length(merkleize(pack(bytes), limit)?, (bytes.len() / 32) as u64))
            }
            Field::Elements(sizes, limit) => {
                element_list_root(bytes, sizes.iter().sum(), limit, |element| container_root(element, sizes))
            }
            Field::PackedList(size, limit) => packed_list_root(bytes, size, limit),
            Field::SyncCommittee => {
                let (keys, aggregate) = bytes.split_at(48 * 512);
                let keys = keys.chunks(48).map(|key| merkleize(pack(key), 2)).collect::<anyhow::Result<_>>()?;
                Ok(hash(&merkleize(keys, 512)?, &merkleize(pack(aggregate), 2)?))
            }
            Field::PayloadHeader => payload_header_root(bytes),
        }
    }
}

/// `MixProof` for the mix at the end of `epoch`, from a serialized Fulu state.
/// Error cases:
/// - the bytes are not a Fulu state, or one past the slot at which `randao_mixes` reuses the epoch's
///   index, or one whose epoch has not ended
pub fn prove_mix(state: &[u8], epoch: u64) -> anyhow::Result<MixProof> {
    let fields = split(state)?;
    let slot = u64::from_le_bytes(fields[SLOT_FIELD].try_into().context("slot")?);
    let first_after = |epochs: u64| epoch.checked_add(epochs)?.checked_mul(SLOTS_PER_EPOCH);
    ensure!(
        first_after(1).is_some_and(|end| slot >= end),
        "the state's slot {slot} is not past epoch {epoch}"
    );
    ensure!(
        first_after(EPOCHS_PER_HISTORICAL_VECTOR).is_none_or(|reuse| slot < reuse),
        "the state's slot {slot} has reused epoch {epoch}'s mix index"
    );
    let roots =
        FULU.iter().zip(&fields).map(|(field, bytes)| field.root(bytes)).collect::<anyhow::Result<Vec<_>>>()?;
    let (state_root, top_to_fork) = branch(roots.clone(), STATE_DEPTH, FORK_FIELD)?;
    let (_, top_to_mixes) = branch(roots.clone(), STATE_DEPTH, RANDAO_MIXES_FIELD)?;
    // `previous_version`, `current_version` and `epoch`, one chunk each.
    let fork = fields[FORK_FIELD];
    let fork_leaves = vec![pack(&fork[..4])[0], pack(&fork[4..8])[0], pack(&fork[8..])[0]];
    let (fork_root, fork_proof) = branch(fork_leaves, FORK_DEPTH, 1)?;
    ensure!(fork_root == roots[FORK_FIELD], "the fork's branch does not give the fork's root");
    let index = (epoch % EPOCHS_PER_HISTORICAL_VECTOR) as usize;
    let mixes = pack(fields[RANDAO_MIXES_FIELD]);
    let mix = B256::from(mixes[index]);
    let (_, mix_proof) = branch(mixes, MIXES_DEPTH, index)?;
    Ok(MixProof {
        slot,
        state_root: B256::from(state_root),
        fork_version: fork[4..8].try_into().context("fork version")?,
        fork_proof: fork_proof.into_iter().chain(top_to_fork).collect(),
        mix,
        mix_proof: mix_proof.into_iter().chain(top_to_mixes).collect(),
    })
}

/// The state's fields' serializations, in order: each fixed field in place, and each variable one
/// between its offset and the next.
/// Error cases:
/// - the fixed part is cut short, or the offsets do not run in order from its end to the end
fn split(state: &[u8]) -> anyhow::Result<Vec<&[u8]>> {
    let fixed: usize = FULU.iter().map(|field| field.fixed_size().unwrap_or(4)).sum();
    ensure!(state.len() >= fixed, "{} bytes are fewer than a Fulu state's fixed part", state.len());
    let mut spans = Vec::with_capacity(FULU.len());
    let mut variable = Vec::new();
    let mut at = 0;
    for field in FULU {
        match field.fixed_size() {
            Some(size) => {
                spans.push(Some((at, at + size)));
                at += size;
            }
            None => {
                let offset = u32::from_le_bytes(state[at..at + 4].try_into().context("offset")?) as usize;
                variable.push((spans.len(), offset));
                spans.push(None);
                at += 4;
            }
        }
    }
    let ends = variable.iter().skip(1).map(|(_, offset)| *offset).chain([state.len()]);
    let mut previous = fixed;
    for ((index, start), end) in variable.iter().copied().zip(ends) {
        if start != previous || end < start {
            bail!("a Fulu state's offsets do not run in order from {fixed} to {}", state.len());
        }
        spans[index] = Some((start, end));
        previous = end;
    }
    Ok(spans.into_iter().map(|span| span.map(|(start, end)| &state[start..end]).unwrap_or_default()).collect())
}

/// `ExecutionPayloadHeader`'s root: its fixed fields, then `extra_data`, a list of at most 32 bytes
/// its offset points to, at the end.
/// Error cases:
/// - the header is cut short, its offset is not its fixed part's end, or `extra_data` is too long
fn payload_header_root(bytes: &[u8]) -> anyhow::Result<[u8; 32]> {
    let head: usize = PAYLOAD_HEADER_HEAD.iter().sum();
    let tail: usize = PAYLOAD_HEADER_TAIL.iter().sum();
    let fixed = head + 4 + tail;
    ensure!(bytes.len() >= fixed, "an execution payload header of {} bytes is cut short", bytes.len());
    let offset = u32::from_le_bytes(bytes[head..head + 4].try_into().context("offset")?) as usize;
    ensure!(offset == fixed, "an execution payload header's extra data starts at {offset}, not {fixed}");
    let extra = &bytes[fixed..];
    ensure!(extra.len() <= MAX_EXTRA_DATA_BYTES, "extra data of {} bytes exceeds 32", extra.len());
    let mut chunks = Vec::with_capacity(17);
    let mut at = 0;
    for &size in &PAYLOAD_HEADER_HEAD {
        chunks.push(field_chunk(&bytes[at..at + size])?);
        at += size;
    }
    chunks.push(mix_in_length(merkleize(pack(extra), 1)?, extra.len() as u64));
    at += 4;
    for &size in &PAYLOAD_HEADER_TAIL {
        chunks.push(field_chunk(&bytes[at..at + size])?);
        at += size;
    }
    let count = chunks.len() as u64;
    merkleize(chunks, count)
}

/// One field's chunk in a container: itself if it fits in one, else its packed tree's root.
/// Error cases:
/// - none in practice; see `merkleize`
fn field_chunk(bytes: &[u8]) -> anyhow::Result<[u8; 32]> {
    if bytes.len() <= 32 { Ok(pack(bytes)[0]) } else { merkleize(pack(bytes), bytes.len().div_ceil(32) as u64) }
}

/// A Fulu state at `slot` with every list empty, `version` as both fork versions, and each
/// `randao_mixes` index holding `mix` of the latest epoch at or before the state's to use it, the
/// dev scenario's stand-in for a beacon node's.
#[cfg(test)]
pub(crate) fn synthetic_state(slot: u64, version: [u8; 4], mix: impl Fn(u64) -> [u8; 32]) -> Vec<u8> {
    let fixed_len: usize = FULU.iter().map(|field| field.fixed_size().unwrap_or(4)).sum();
    let epoch = slot / SLOTS_PER_EPOCH;
    let (mut fixed, mut variable) = (Vec::new(), Vec::new());
    for (index, field) in FULU.iter().enumerate() {
        let Some(size) = field.fixed_size() else {
            fixed.extend(u32::try_from(fixed_len + variable.len()).unwrap().to_le_bytes());
            if matches!(field, Field::PayloadHeader) {
                let head: usize = PAYLOAD_HEADER_HEAD.iter().sum();
                let header_len = head + 4 + PAYLOAD_HEADER_TAIL.iter().sum::<usize>();
                let mut header = vec![0u8; header_len];
                header[head..head + 4].copy_from_slice(&u32::try_from(header_len).unwrap().to_le_bytes());
                variable.extend(header);
            }
            continue;
        };
        match index {
            SLOT_FIELD => fixed.extend(slot.to_le_bytes()),
            FORK_FIELD => fixed.extend(version.iter().chain(&version).copied().chain(epoch.to_le_bytes())),
            RANDAO_MIXES_FIELD => fixed.extend((0..EPOCHS_PER_HISTORICAL_VECTOR).flat_map(|index| {
                let behind = (epoch + EPOCHS_PER_HISTORICAL_VECTOR - index) % EPOCHS_PER_HISTORICAL_VECTOR;
                epoch.checked_sub(behind).map(&mix).unwrap_or_default()
            })),
            _ => fixed.extend(vec![0u8; size]),
        }
    }
    fixed.extend(variable);
    fixed
}

#[cfg(test)]
mod tests;
