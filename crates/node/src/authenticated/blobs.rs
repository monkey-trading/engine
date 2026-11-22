//! Logical values use a proved descriptor and fixed-size, independently proved chunk leaves, or, in a
//! namespace written compactly, a value of at most one chunk stored whole in its own leaf.

use std::io::{Read, Write};

use anyhow::{Result, ensure};
use jmt::{KeyHash, SimpleHasher};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::authenticated::reader::{Checked, Reader};
use crate::authenticated::receipts::internal_key;
use crate::authenticated::{CHUNK_BYTES, Hasher, PHYSICAL_UPDATES, Root};

/// Point reads under one root sharing one checked node cache. The cache is renewed after
/// `PHYSICAL_UPDATES` reads, so each reader stays within its finite per-operation envelope; the nodes
/// the operation has checked carry over.
pub(super) struct Points<'a> {
    conn: &'a Connection,
    root: Root,
    checked: &'a Checked,
    reader: Option<Reader<'a>>,
    used: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Blob {
    pub length: u64,
    pub digest: [u8; 32],
}

/// The first byte of a value stored whole in its own leaf, which no descriptor's JSON starts with.
const INLINE_TAG: u8 = 0;

/// A logical value's proved leaf: the descriptor of its chunks, or the value itself.
pub(super) enum Stored {
    Chunked(Blob),
    Inline(Vec<u8>),
}

impl<'a> Points<'a> {
    /// Starts reads under `root` with the nodes the operation has `checked`; nothing is read until the
    /// first point.
    pub(super) fn new(conn: &'a Connection, root: &Root, checked: &'a Checked) -> Self {
        Self { conn, root: root.clone(), checked, reader: None, used: 0 }
    }

    /// Returns exact verified bytes or verified absence of one physical key.
    /// Error cases:
    /// - native storage access, structural checks or upstream point proof fails
    pub(super) fn get(&mut self, key: KeyHash) -> Result<Option<Vec<u8>>> {
        if self.used == PHYSICAL_UPDATES {
            self.reader = None;
        }
        let reader = match &mut self.reader {
            Some(reader) => reader,
            None => {
                self.used = 0;
                self.reader.insert(Reader::new(self.conn, &self.root, true, self.checked)?)
            }
        };
        self.used += 1;
        reader.read(key)
    }
}

impl Blob {
    /// Returns the number of physical chunks without overflow at u64::MAX.
    pub(super) fn chunks(&self) -> u64 {
        self.length / CHUNK_BYTES as u64 + u64::from(!self.length.is_multiple_of(CHUNK_BYTES as u64))
    }
}

impl Stored {
    /// The chunk leaves the value has besides its own: none for a value stored whole.
    pub(super) fn chunks(&self) -> u64 {
        match self {
            Self::Chunked(blob) => blob.chunks(),
            Self::Inline(_) => 0,
        }
    }
}

/// The leaf of `value`, of at most one chunk, stored whole.
pub(super) fn inline(value: &[u8]) -> Vec<u8> {
    [&[INLINE_TAG], value].concat()
}

/// Locates the descriptor, or the value stored whole, only after upstream membership/nonmembership
/// verification.
/// Error cases:
/// - failed proof, malformed descriptor, excessive fixed descriptor size or a value stored whole past one
///   chunk
pub(super) fn descriptor(points: &mut Points<'_>, key: KeyHash) -> Result<Option<Stored>> {
    points
        .get(key)?
        .map(|mut raw| {
            if raw.first() == Some(&INLINE_TAG) {
                ensure!(raw.len() <= CHUNK_BYTES + 1, "oversized inline logical value");
                raw.remove(0);
                return Ok(Stored::Inline(raw));
            }
            ensure!(raw.len() <= 512, "oversized logical value descriptor");
            Ok(Stored::Chunked(serde_json::from_slice(&raw)?))
        })
        .transpose()
}

/// Derives a fixed-size chunk locator from the caller key hash and exact ordinal.
/// Error cases:
/// - hashing fails
pub(super) fn chunk_key(namespace: &[u8; 32], key: KeyHash, index: u64) -> Result<KeyHash> {
    let mut suffix = [0u8; 40];
    suffix[..32].copy_from_slice(&key.0);
    suffix[32..].copy_from_slice(&index.to_be_bytes());
    internal_key(namespace, b"chunk/", &suffix)
}

/// Streams exact logical bytes from verified chunks; the sink remains provisional until success.
/// Error cases:
/// - any missing/forged chunk, inconsistent length/digest or sink failure
pub(super) fn read_to(points: &mut Points<'_>, key: KeyHash, sink: &mut impl Write) -> Result<Option<u64>> {
    let blob = match descriptor(points, key)? {
        None => return Ok(None),
        Some(Stored::Inline(value)) => {
            sink.write_all(&value)?;
            return Ok(Some(value.len() as u64));
        }
        Some(Stored::Chunked(blob)) => blob,
    };
    let mut hasher = <Hasher as SimpleHasher>::new();
    let mut remaining = blob.length;
    for index in 0..blob.chunks() {
        let chunk = points
            .get(chunk_key(&points.root.namespace, key, index)?)?
            .ok_or_else(|| anyhow::anyhow!("missing logical value chunk"))?;
        let length = remaining.min(CHUNK_BYTES as u64) as usize;
        ensure!(chunk.len() == length, "logical value chunk length mismatch");
        hasher.update(&chunk);
        sink.write_all(&chunk)?;
        remaining -= length as u64;
    }
    ensure!(remaining == 0 && hasher.finalize() == blob.digest, "logical value digest mismatch");
    Ok(Some(blob.length))
}

/// Collects a logical value only after its length was authenticated; streaming avoids this allocation.
/// Error cases:
/// - unavailable allocation, unrepresentable length, bad proof or malformed logical value
pub(super) fn read(points: &mut Points<'_>, key: KeyHash) -> Result<Option<Vec<u8>>> {
    let blob = match descriptor(points, key)? {
        None => return Ok(None),
        Some(Stored::Inline(value)) => return Ok(Some(value)),
        Some(Stored::Chunked(blob)) => blob,
    };
    let length = usize::try_from(blob.length)?;
    let mut value = Vec::new();
    value.try_reserve_exact(length)?;
    ensure!(read_to(points, key, &mut value)? == Some(blob.length), "logical descriptor changed inside transaction");
    Ok(Some(value))
}

/// Reads exactly one bounded physical chunk from a trusted caller-provided source.
/// Error cases:
/// - premature EOF or source IO failure
pub(super) fn input_chunk(source: &mut impl Read, remaining: u64) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; remaining.min(CHUNK_BYTES as u64) as usize];
    source.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(super) struct Compare<'a> {
    pub expected: &'a [u8],
    pub offset: usize,
}

impl Write for Compare<'_> {
    /// Compares streamed proved bytes directly to the exact input without another large allocation.
    /// Error cases:
    /// - comparison offset overflow or bytes differ from the exact caller input
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let end = self.offset.checked_add(bytes.len()).ok_or_else(|| std::io::Error::other("comparison overflow"))?;
        if self.expected.get(self.offset..end) != Some(bytes) {
            return Err(std::io::Error::other("persisted caller bytes differ"));
        }
        self.offset = end;
        Ok(bytes.len())
    }

    /// A comparison sink has no buffered output.
    /// Error cases:
    /// - none
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
