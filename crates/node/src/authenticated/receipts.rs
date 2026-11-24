//! Queue/cursor and retirement receipts are ordinary leaves under the caller-authenticated root.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use jmt::KeyHash;
use jmt::storage::NodeKey;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::authenticated::reader::{Checked, Reader};
use crate::authenticated::{INTERNAL, MAX_RECEIPT_BYTES, MAX_RECEIPT_ROWS, Root, codec, key_hash};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Queue {
    pub lo: u64,
    pub hi: u64,
    pub offset: usize,
    pub floor: u64,
}

/// A version's retired rows, stored in its fixed-width form (`Receipt::encode`), a few bytes a node.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Receipt {
    pub old: Root,
    pub version: u64,
    pub maintenance: bool,
    pub rows: Vec<RetiredRow>,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(super) enum RetiredRow {
    Node(NodeKey),
    Value { key: KeyHash, version: u64 },
}

/// A stored receipt's fields, read in order.
struct Fields<'a>(&'a [u8]);

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
    Reader::new(conn, root, false, &Checked::default())?.read(key)
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
    let receipt = Receipt::decode(&raw)?;
    receipt.validate(root, version)?;
    Ok(receipt)
}

impl Receipt {
    /// Its stored form: the old root's namespace, its version (0, or 1 and the version) and hash, the version
    /// the receipt retires for, whether a collection wrote it (0 or 1) and its row count, then each row, a node
    /// (0, its version and its path key, `codec::path`) or a value (1, its key and version); every integer is
    /// big-endian.
    /// Error cases:
    /// - a node's path key cannot be formed, or the rows are too many to count
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(96 + self.rows.len() * 41);
        bytes.extend_from_slice(&self.old.namespace);
        match self.old.version {
            None => bytes.push(0),
            Some(version) => {
                bytes.push(1);
                bytes.extend_from_slice(&version.to_be_bytes());
            }
        }
        bytes.extend_from_slice(&self.old.hash);
        bytes.extend_from_slice(&self.version.to_be_bytes());
        bytes.push(u8::from(self.maintenance));
        bytes.extend_from_slice(&u32::try_from(self.rows.len())?.to_be_bytes());
        for row in &self.rows {
            match row {
                RetiredRow::Node(key) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&key.version().to_be_bytes());
                    bytes.extend_from_slice(&codec::path(key)?);
                }
                RetiredRow::Value { key, version } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&key.0);
                    bytes.extend_from_slice(&version.to_be_bytes());
                }
            }
        }
        Ok(bytes)
    }

    /// The receipt whose stored form (`encode`) is exactly `raw`, its fields unchecked against any root.
    /// Error cases:
    /// - an unknown tag, a malformed path key, a count past the bytes, or a byte past the receipt
    pub(super) fn decode(raw: &[u8]) -> Result<Self> {
        let mut fields = Fields(raw);
        let namespace = fields.take::<32>()?;
        let old_version = match fields.byte()? {
            0 => None,
            1 => Some(fields.u64()?),
            _ => anyhow::bail!("invalid receipt root version tag"),
        };
        let old = Root { namespace, version: old_version, hash: fields.take()? };
        let version = fields.u64()?;
        let maintenance = match fields.byte()? {
            0 => false,
            1 => true,
            _ => anyhow::bail!("invalid receipt maintenance flag"),
        };
        let count = u32::from_be_bytes(fields.take()?);
        ensure!(count as usize <= MAX_RECEIPT_ROWS, "receipt outside its row envelope");
        let rows = (0..count)
            .map(|_| match fields.byte()? {
                0 => {
                    let version = fields.u64()?;
                    let nibbles = fields.byte()?;
                    let packed = fields.slice(usize::from(nibbles).div_ceil(2))?;
                    Ok(RetiredRow::Node(codec::node_key(version, &[&[nibbles], packed].concat())?))
                }
                1 => Ok(RetiredRow::Value { key: KeyHash(fields.take()?), version: fields.u64()? }),
                _ => anyhow::bail!("invalid retired row tag"),
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(fields.0.is_empty(), "bytes past a receipt");
        Ok(Self { old, version, maintenance, rows })
    }

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

impl<'a> Fields<'a> {
    /// The next `n` bytes.
    /// Error cases:
    /// - fewer are left
    fn slice(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= n, "short receipt");
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(taken)
    }

    /// The next `N` bytes.
    /// Error cases:
    /// - fewer are left
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.slice(N)?.try_into()?)
    }

    /// The next byte.
    /// Error cases:
    /// - none is left
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    /// The next big-endian `u64`.
    /// Error cases:
    /// - fewer than eight bytes are left
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take()?))
    }
}
