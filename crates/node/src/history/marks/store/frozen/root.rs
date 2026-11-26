//! The root a daily snapshot carries over the rows of its frozen marks copy (`Frozen::root`): the root of an
//! Ethereum Merkle-Patricia trie over the rows, built by alloy-trie's `HashBuilder` from keys in order, which holds
//! only the path to the last key it took, so a copy of any size is hashed, and checked, in one pass that holds one
//! row at a time. A row's key is its market (4 bytes), its resolution's storage tag (1 byte) and its bucket (8 bytes,
//! the sign bit flipped so the bytes order as the numbers do), big-endian: the order the copy is read in, by market,
//! resolution and bucket. Its value is the keccak-256 of `ROW_TAG` and every other column the row stores, each
//! length-prefixed or flagged: its OHLC, its provenance, the bucket it names below it, its tag and the tag it binds.
//! A row changed, added, removed or moved to another key gives another root.

use alloy_trie::{HashBuilder, Nibbles};
use anyhow::ensure;
use engine_core::eip712::B256;
use engine_core::types::{MarketId, Millis};
use sha3::{Digest, Keccak256};

use crate::history::marks::Resolution;
use crate::history::marks::store::SnapRow;

/// What a row's value hashes first, which no other hash the node makes starts with.
const ROW_TAG: &[u8] = b"engine.marks.frozen.row.v1";

/// A row's key in the trie: its market, resolution tag and bucket, ordered as the copy is read.
type Key = [u8; 13];

/// The root over a frozen copy's rows, built as they are read in key order.
#[derive(Default)]
pub(crate) struct Root {
    builder: HashBuilder,
    last: Option<Key>,
}

impl Root {
    /// Takes `row`, of `market` at `resolution`, which must come after the last row taken.
    /// Error cases:
    /// - the row does not come after the last one in key order
    pub(crate) fn add(&mut self, market: MarketId, resolution: Resolution, row: &SnapRow) -> anyhow::Result<()> {
        let key = key(market, resolution, row.bucket);
        ensure!(
            self.last.is_none_or(|last| last < key),
            "the frozen {market} {resolution:?} bucket at {} is out of order",
            row.bucket
        );
        // In order, as checked above, which `add_leaf` asserts.
        self.builder.add_leaf(Nibbles::unpack(key), value(row).as_slice());
        self.last = Some(key);
        Ok(())
    }

    /// The root over the rows taken: alloy-trie's empty root over none.
    pub(crate) fn finish(mut self) -> B256 {
        self.builder.root()
    }
}

/// The key of the row of `market` at `resolution` and `bucket`.
fn key(market: MarketId, resolution: Resolution, bucket: Millis) -> Key {
    let mut key = [0; 13];
    key[..4].copy_from_slice(&market.to_be_bytes());
    key[4] = resolution as u8;
    key[5..].copy_from_slice(&(bucket.cast_unsigned() ^ (1 << 63)).to_be_bytes());
    key
}

/// The value of `row`: the keccak-256 of `ROW_TAG` and every column it stores besides its key.
fn value(row: &SnapRow) -> B256 {
    let mut hash = Keccak256::new();
    hash.update(ROW_TAG);
    for text in [&row.open, &row.high, &row.low, &row.close, &row.provenance] {
        hash.update((text.len() as u64).to_be_bytes());
        hash.update(text.as_bytes());
    }
    match row.prev {
        Some(prev) => {
            hash.update([1]);
            hash.update(prev.to_be_bytes());
        }
        None => hash.update([0]),
    }
    hash.update(row.tag);
    match row.prev_tag {
        Some(prev_tag) => {
            hash.update([1]);
            hash.update(prev_tag);
        }
        None => hash.update([0]),
    }
    B256::from(<[u8; 32]>::from(hash.finalize()))
}

#[cfg(test)]
mod tests;
