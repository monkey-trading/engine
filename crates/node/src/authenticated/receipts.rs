//! Queue/cursor and retirement receipts are ordinary leaves under the caller-authenticated root.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use jmt::KeyHash;
use jmt::storage::NodeKey;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::authenticated::reader::Reader;
use crate::authenticated::{INTERNAL, MAX_RECEIPT_BYTES, MAX_RECEIPT_ROWS, Root, codec, key_hash};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Queue {
    pub lo: u64,
    pub hi: u64,
    pub offset: usize,
    pub floor: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub old: Root,
    pub version: u64,
    pub maintenance: bool,
    pub rows: Vec<RetiredRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) enum RetiredRow {
    Node(NodeKey),
    Value { key: KeyHash, version: u64 },
}

/// Computes the reserved queue key with the same upstream key hashing as user leaves.
/// Error cases:
/// - hashing fails
pub(super) fn queue_key(namespace: &[u8; 32]) -> Result<KeyHash> {
    internal_key(namespace, b"queue", &[])
}

/// Computes one receipt key without using an unsigned SQL queue/index.
/// Error cases:
/// - hashing fails
pub(super) fn receipt_key(namespace: &[u8; 32], version: u64) -> Result<KeyHash> {
    internal_key(namespace, b"receipt/", &version.to_be_bytes())
}

/// Computes reserved fixed-format internal keys; no variable host length is accepted.
/// Error cases:
/// - hashing fails
pub(super) fn internal_key(namespace: &[u8; 32], tag: &[u8], suffix: &[u8]) -> Result<KeyHash> {
    let mut bytes = Vec::with_capacity(INTERNAL.len() + tag.len() + suffix.len());
    bytes.extend_from_slice(INTERNAL);
    bytes.extend_from_slice(tag);
    bytes.extend_from_slice(suffix);
    key_hash(namespace, &bytes)
}

/// Reads one physical point with a fresh finite decoder/traversal envelope.
/// Error cases:
/// - native storage access, structural checks or upstream point proof fails
pub(super) fn point(conn: &Connection, root: &Root, key: KeyHash) -> Result<Option<Vec<u8>>> {
    Reader::new(conn, root, false)?.read(key)
}

/// Proves the persisted maintenance cursor; only the explicit pre-version root lacks it.
/// Error cases:
/// - missing queue, invalid proof, malformed cursor or inconsistent root version
pub(super) fn queue(conn: &Connection, root: &Root) -> Result<Queue> {
    let raw = point(conn, root, queue_key(&root.namespace)?)?;
    if root.version.is_none() {
        ensure!(raw.is_none(), "pre-version root contains a queue");
        return Ok(Queue::default());
    }
    let raw = raw.ok_or_else(|| anyhow::anyhow!("missing authenticated retirement queue"))?;
    ensure!(raw.len() <= 256, "oversized queue encoding");
    let queue: Queue = serde_json::from_slice(&raw)?;
    let next = root.version.and_then(|v| v.checked_add(1));
    ensure!(
        Some(queue.hi) == next && queue.lo < queue.hi && queue.floor < queue.hi,
        "invalid authenticated queue bounds"
    );
    ensure!(queue.offset == 0, "format requires whole-receipt cursors");
    Ok(queue)
}

/// Proves and bounds an exact receipt before it can authorize any physical deletion.
/// Error cases:
/// - absent/forged receipt, malformed rows, wrong namespace/version or duplicate targets
pub(super) fn receipt(conn: &Connection, root: &Root, version: u64) -> Result<Receipt> {
    let raw = point(conn, root, receipt_key(&root.namespace, version)?)?
        .ok_or_else(|| anyhow::anyhow!("missing authenticated retirement receipt"))?;
    ensure!(raw.len() <= MAX_RECEIPT_BYTES, "oversized receipt encoding");
    let receipt: Receipt = serde_json::from_slice(&raw)?;
    receipt.validate(root, version)?;
    Ok(receipt)
}

impl Receipt {
    /// Checks representability and validates paths before upstream key ordering can inspect them.
    /// Error cases:
    /// - wrong version/namespace, excessive targets, invalid paths or duplicate/future rows
    pub(super) fn validate(&self, root: &Root, version: u64) -> Result<()> {
        self.old.validate()?;
        ensure!(
            self.old.namespace == root.namespace
                && self.version == version
                && self.old.version.map_or(Some(0), |v| v.checked_add(1)) == Some(version),
            "receipt identity mismatch"
        );
        ensure!(
            root.version.is_some_and(|v| version <= v) && self.rows.len() <= MAX_RECEIPT_ROWS,
            "receipt outside trusted root"
        );
        for row in &self.rows {
            let previous = match row {
                RetiredRow::Node(key) => {
                    codec::path(key)?;
                    key.version()
                }
                RetiredRow::Value { version, .. } => *version,
            };
            ensure!(previous < version, "retirement is not older than its verified transition");
        }
        let unique: BTreeSet<_> = self.rows.iter().collect();
        ensure!(unique.len() == self.rows.len(), "duplicate retirement targets");
        Ok(())
    }
}
