//! SSZ merkleization (the consensus specification's `ssz/simple-serialize.md`): 32-byte chunks,
//! binary SHA-256 trees padded with zero subtrees, and list lengths mixed into a list's root. It
//! builds the relayer's witnesses only: `SeedRelay` checks every branch against a beacon block root
//! on chain, so a mistake here costs a relay, never a wrong seed.

use std::sync::LazyLock;

use alloy::primitives::B256;
use anyhow::{bail, ensure};
use sha2::{Digest, Sha256};

/// Deepest tree any field reaches: a list of 2^40 elements, with room for its length.
const MAX_DEPTH: usize = 41;

/// `ZERO_HASHES[d]`: the root of a tree of height `d` whose leaves are all zero.
static ZERO_HASHES: LazyLock<[[u8; 32]; MAX_DEPTH + 1]> = LazyLock::new(|| {
    let mut hashes = [[0u8; 32]; MAX_DEPTH + 1];
    for depth in 1..=MAX_DEPTH {
        hashes[depth] = hash(&hashes[depth - 1], &hashes[depth - 1]);
    }
    hashes
});

/// SHA-256 of two chunks side by side.
pub(crate) fn hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// The depth of a tree with room for `limit` chunks: the bits of the next power of two.
fn depth_for(limit: u64) -> usize {
    limit.max(1).next_power_of_two().trailing_zeros() as usize
}

/// `bytes` in 32-byte chunks, the last one padded with zeros; none for no bytes.
pub(crate) fn pack(bytes: &[u8]) -> Vec<[u8; 32]> {
    bytes
        .chunks(32)
        .map(|part| {
            let mut chunk = [0u8; 32];
            chunk[..part.len()].copy_from_slice(part);
            chunk
        })
        .collect()
}

/// The root of `chunks` in a tree with room for `limit` of them.
/// Error cases:
/// - more chunks than `limit`
pub(crate) fn merkleize(chunks: Vec<[u8; 32]>, limit: u64) -> anyhow::Result<[u8; 32]> {
    ensure!(chunks.len() as u64 <= limit.max(1), "{} chunks exceed the type's {limit}", chunks.len());
    let depth = depth_for(limit);
    ensure!(depth <= MAX_DEPTH, "a tree of {limit} chunks is deeper than any state field");
    let mut layer = chunks;
    if layer.is_empty() {
        return Ok(ZERO_HASHES[depth]);
    }
    for zero in &ZERO_HASHES[..depth] {
        if layer.len() % 2 == 1 {
            layer.push(*zero);
        }
        layer = layer.chunks(2).map(|pair| hash(&pair[0], &pair[1])).collect();
    }
    Ok(layer[0])
}

/// A list's root: its elements' root with their count mixed in.
pub(crate) fn mix_in_length(root: [u8; 32], length: u64) -> [u8; 32] {
    let mut chunk = [0u8; 32];
    chunk[..8].copy_from_slice(&length.to_le_bytes());
    hash(&root, &chunk)
}

/// The root of a fixed-size container from its serialization and its fields' sizes: a field of at
/// most 32 bytes (an integer, a boolean, or a short byte vector) is one chunk, and a longer byte
/// vector its own packed tree.
/// Error cases:
/// - `bytes` is not exactly the fields' total size
pub(crate) fn container_root(bytes: &[u8], sizes: &[usize]) -> anyhow::Result<[u8; 32]> {
    ensure!(bytes.len() == sizes.iter().sum::<usize>(), "a container of {} bytes has the wrong size", bytes.len());
    let mut offset = 0;
    let mut chunks = Vec::with_capacity(sizes.len());
    for &size in sizes {
        let field = &bytes[offset..offset + size];
        chunks.push(if size <= 32 { pack(field)[0] } else { merkleize(pack(field), size.div_ceil(32) as u64)? });
        offset += size;
    }
    merkleize(chunks, sizes.len() as u64)
}

/// The root of a list of fixed-size elements, each `size` bytes, with room for `limit` of them.
/// Error cases:
/// - the bytes are not whole elements, or hold more than `limit`
pub(crate) fn element_list_root(
    bytes: &[u8],
    size: usize,
    limit: u64,
    root: impl Fn(&[u8]) -> anyhow::Result<[u8; 32]>,
) -> anyhow::Result<[u8; 32]> {
    ensure!(bytes.len().is_multiple_of(size), "a list of {size}-byte elements holds {} bytes", bytes.len());
    let roots = bytes.chunks(size).map(root).collect::<anyhow::Result<Vec<_>>>()?;
    let length = roots.len() as u64;
    Ok(mix_in_length(merkleize(roots, limit)?, length))
}

/// The root of a list of `size`-byte integers packed into chunks, with room for `limit` of them.
/// Error cases:
/// - the bytes are not whole integers, or hold more than `limit`
pub(crate) fn packed_list_root(bytes: &[u8], size: usize, limit: u64) -> anyhow::Result<[u8; 32]> {
    ensure!(bytes.len().is_multiple_of(size), "a list of {size}-byte integers holds {} bytes", bytes.len());
    let length = (bytes.len() / size) as u64;
    ensure!(length <= limit, "a list of {length} exceeds its limit of {limit}");
    Ok(mix_in_length(merkleize(pack(bytes), (limit * size as u64).div_ceil(32))?, length))
}

/// The root of `leaves` in a tree of `depth`, and the siblings of the leaf at `index` from the
/// bottom up, as `is_valid_merkle_branch` reads them.
/// Error cases:
/// - more leaves than the tree holds, or `index` outside it
pub(crate) fn branch(leaves: Vec<[u8; 32]>, depth: usize, index: usize) -> anyhow::Result<([u8; 32], Vec<B256>)> {
    ensure!(depth <= MAX_DEPTH && leaves.len() <= 1 << depth, "{} leaves exceed a tree of depth {depth}", leaves.len());
    if index >= 1 << depth {
        bail!("leaf {index} lies outside a tree of depth {depth}");
    }
    let mut layer = leaves;
    let mut position = index;
    let mut siblings = Vec::with_capacity(depth);
    for zero in &ZERO_HASHES[..depth] {
        if layer.len() % 2 == 1 {
            layer.push(*zero);
        }
        siblings.push(B256::from(layer.get(position ^ 1).copied().unwrap_or(*zero)));
        layer = layer.chunks(2).map(|pair| hash(&pair[0], &pair[1])).collect();
        position /= 2;
    }
    let root = layer.first().copied().unwrap_or(ZERO_HASHES[depth]);
    Ok((root, siblings))
}

/// The consensus specification's `is_valid_merkle_branch`, as `SeedRelay` checks it; the tests check
/// the witnesses with it.
#[cfg(test)]
pub(crate) fn is_valid_merkle_branch(leaf: [u8; 32], branch: &[B256], index: usize, root: [u8; 32]) -> bool {
    let value = branch.iter().enumerate().fold(leaf, |value, (level, sibling)| {
        if (index >> level) & 1 == 1 { hash(&sibling.0, &value) } else { hash(&value, &sibling.0) }
    });
    value == root
}

#[cfg(test)]
mod tests;
