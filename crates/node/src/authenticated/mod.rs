//! Proof-verified, host-backed JMT versions with authenticated persistent retirement receipts.
//! `open_existing` is strictly read-only; the parent authorizes writes only after recovery preflight.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Result, ensure};
use jmt::SimpleHasher;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

mod batch;
mod blobs;
mod codec;
mod db;
mod geometry;
mod reader;
mod receipts;
mod staging;
mod versions;
mod write;
pub use versions::Lease;
mod gc;
pub use gc::{COMPLETE_RECEIPT_BUDGET, GcReport};

/// The exact packed representation and upstream proof interpretation bound by cache signatures.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Codec {
    pub packed_format: u32,
    pub logical_format: u32,
    pub proof: Proof,
}

/// A named upstream proof scheme; changing its interpretation requires a new cache format.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Proof {
    Jmt012Sha256,
}

pub const CODEC: Codec = Codec { packed_format: 2, logical_format: 2, proof: Proof::Jmt012Sha256 };

// Physical representation bounds; logical keys, values, changes and lease counts have no such cap.
pub const CHUNK_BYTES: usize = 64 * 1024;
pub const MAX_UPDATES: usize = 64;
/// A caller's finite staging target; logical Store updates and values remain uncapped.
pub const MAX_BATCH_BYTES: usize = MAX_UPDATES * CHUNK_BYTES;
const PHYSICAL_UPDATES: usize = MAX_UPDATES + 2;
const MAX_NODE_BYTES: usize = 16 * 1024;
const MAX_STORED_VALUE: usize = 4 * 1024 * 1024;
const MAX_RECEIPT_ROWS: usize = PHYSICAL_UPDATES * 66 + 1;
const MAX_RECEIPT_BYTES: usize = MAX_RECEIPT_ROWS * 512 + 4096;
const INTERNAL: &[u8] = b"\0engine.authenticated/";
// jmt 0.12.0 src/lib.rs:147; the upstream empty-tree constant, not a hash algorithm.
const EMPTY_HASH: [u8; 32] = *b"SPARSE_MERKLE_PLACEHOLDER_HASH__";
type Changes = Vec<(jmt::KeyHash, Option<Vec<u8>>)>;

/// Caller-authenticated tree identity; `None` denotes a namespace before its first commit.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    pub namespace: [u8; 32],
    #[serde(deserialize_with = "required_version")]
    pub version: Option<u64>,
    pub hash: [u8; 32],
}

/// Clones share one writer, root authority, cancellation-safe leases and write authorization.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

/// A retired generation file is removable only after every store clone and actual lease ends.
#[derive(Clone)]
pub struct Lifetime(Weak<Inner>);

impl Lifetime {
    /// Zero live owners cannot be resurrected by an already admitted reader.
    pub fn alive(&self) -> bool {
        self.0.strong_count() != 0
    }
}

struct Inner {
    path: PathBuf,
    /// Opened with a hot rollback journal: reads take the database file as it is, without the
    /// journal, until `enable_writes` rolls the journal back.
    crash_image: AtomicBool,
    files: Mutex<Option<WriteFiles>>,
    writer: Mutex<()>,
    runtime: Mutex<Runtime>,
}

struct WriteFiles {
    database: File,
    _ownership_lock: File,
}

#[derive(Default)]
struct Runtime {
    namespaces: BTreeMap<[u8; 32], Namespace>,
    poisoned: bool,
}

#[derive(Default)]
struct Namespace {
    pins: BTreeMap<Root, Weak<versions::Pin>>,
    versions: BTreeMap<u64, usize>,
    head: Option<Root>,
    floor: u64,
}

/// Nameable upstream hasher interface backed by the existing workspace SHA2 0.11 implementation.
struct Hasher(Sha256);

impl SimpleHasher for Hasher {
    /// Creates standard SHA-256.
    fn new() -> Self {
        Self(Sha256::new())
    }
    /// Adds bytes using the library implementation.
    fn update(&mut self, data: &[u8]) {
        Digest::update(&mut self.0, data);
    }
    /// Returns a standard SHA-256 digest.
    fn finalize(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

impl Root {
    /// Creates a pre-version empty namespace; generation uniqueness belongs to the trusted owner.
    pub fn empty(namespace: [u8; 32]) -> Self {
        Self { namespace, version: None, hash: EMPTY_HASH }
    }

    /// Checks native storage representability and the explicit pre-version identity.
    /// Error cases:
    /// - an unrepresentable version or invalid pre-version hash
    fn validate(&self) -> Result<()> {
        ensure!(self.version.is_none_or(|v| v <= i64::MAX as u64), "version exceeds storage representation");
        ensure!(self.version.is_some() || self.hash == EMPTY_HASH, "invalid pre-version root");
        Ok(())
    }
}

impl Store {
    /// Requires every role and reader for one file to share its registered owner lineage.
    pub fn same_owner(&self, other: &Store) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Tracks actual blocking-reader lifetime without keeping the retired generation alive.
    pub fn lifetime(&self) -> Lifetime {
        Lifetime(Arc::downgrade(&self.inner))
    }

    /// Returns exact verified bytes or verified absence; use `read_to` for bounded-memory output.
    /// Error cases:
    /// - retired root, invalid storage/proofs, allocation failure, or reserved caller key
    pub fn read(&self, root: &Root, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.lease(root)?.read(key)
    }

    /// Streams individually verified chunks; sink output is provisional until this call succeeds.
    /// Error cases:
    /// - invalid proofs/lengths, storage failure, reserved key, or sink IO failure
    pub fn read_to(&self, root: &Root, key: &[u8], sink: impl std::io::Write) -> Result<Option<u64>> {
        self.lease(root)?.read_to(key, sink)
    }
}

/// Requires an explicitly present nullable version during serde deserialization.
/// Error cases:
/// - the field has an invalid representation
fn required_version<'de, D: Deserializer<'de>>(decoder: D) -> std::result::Result<Option<u64>, D::Error> {
    Option::<u64>::deserialize(decoder)
}

/// Rejects reserved caller keys; key bytes themselves never become an SQL allocation bound.
/// Error cases:
/// - the raw key occupies the internal namespace
fn caller_key(namespace: &[u8; 32], key: &[u8]) -> Result<jmt::KeyHash> {
    ensure!(!key.starts_with(INTERNAL), "reserved authenticated-store key");
    key_hash(namespace, key)
}

/// Hashes an arbitrary raw key with fixed domain and namespace separation through upstream traits.
/// Error cases:
/// - none
fn key_hash(namespace: &[u8; 32], key: &[u8]) -> Result<jmt::KeyHash> {
    let mut hasher = <Hasher as SimpleHasher>::new();
    hasher.update(b"engine.authenticated.key.v2\0");
    hasher.update(namespace);
    hasher.update(key);
    Ok(jmt::KeyHash(hasher.finalize()))
}

#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod large_tests;
#[cfg(test)]
mod preflight_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tamper_tests;
#[cfg(test)]
pub(crate) mod tests;
