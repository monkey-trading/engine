//! Point lookups authenticated before upstream traversal, within one SQLite snapshot.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use anyhow::{Result, bail, ensure};
use jmt::storage::{LeafNode, Node, NodeKey, TreeReader};
use jmt::{JellyfishMerkleTree, KeyHash, RootHash};
use rusqlite::{Connection, params};

use crate::authenticated::codec;
use crate::authenticated::{Hasher, MAX_NODE_BYTES, MAX_STORED_VALUE, PHYSICAL_UPDATES, Root};

pub(super) struct Reader<'a> {
    conn: &'a Connection,
    root: Root,
    cache: RefCell<BTreeMap<NodeKey, Node>>,
    reads_left: Cell<usize>,
    bytes_left: Cell<usize>,
}

impl<'a> Reader<'a> {
    /// Creates bounded per-operation state, authenticating the root before it can be traversed.
    /// Error cases:
    /// - the root is absent, malformed, or differs from the caller's trusted hash
    pub(super) fn new(conn: &'a Connection, root: &Root, writing: bool) -> Result<Self> {
        root.validate()?;
        let keys = if writing { PHYSICAL_UPDATES } else { 1 };
        let nodes = 1 + keys * 64 * 16;
        let reader = Self {
            conn,
            root: root.clone(),
            cache: RefCell::new(BTreeMap::new()),
            reads_left: Cell::new(nodes + keys * 8),
            bytes_left: Cell::new(nodes * MAX_NODE_BYTES + keys * MAX_STORED_VALUE * 4),
        };
        if let Some(version) = root.version {
            let key = codec::root_key(version)?;
            let node = reader.fetch(&key)?;
            ensure!(codec::hash(&node) == root.hash, "root node hash mismatch");
            reader.cache.borrow_mut().insert(key, node);
        }
        Ok(reader)
    }

    /// Returns exact value bytes or proved absence using JMT's point verifier.
    /// Error cases:
    /// - bounded storage traversal or upstream membership/nonmembership verification fails
    pub(super) fn read(&self, key: KeyHash) -> Result<Option<Vec<u8>>> {
        self.prepare(&[key])?;
        let tree = JellyfishMerkleTree::<_, Hasher>::new(self);
        let (value, proof) = tree.get_with_proof(key, self.root.version.unwrap_or(0))?;
        proof.verify(RootHash(self.root.hash), key, value.as_deref())?;
        Ok(value)
    }

    /// Fetches every requested old-tree path and its one-hop siblings before infallible JMT reads.
    /// Error cases:
    /// - an unavailable/malformed node, child inconsistency, or exhausted finite resource budget
    pub(super) fn prepare(&self, keys: &[KeyHash]) -> Result<()> {
        let Some(version) = self.root.version else { return Ok(()) };
        for query in keys {
            let mut key = codec::root_key(version)?;
            for depth in 0..=64 {
                let node =
                    self.cache.borrow().get(&key).cloned().ok_or_else(|| anyhow::anyhow!("missing prepared node"))?;
                let Node::Internal(internal) = &node else { break };
                ensure!(depth < 64, "terminal internal node");
                self.prepare_children(&key, &node)?;
                let nibble = query.0[depth / 2] >> if depth % 2 == 0 { 4 } else { 0 } & 15;
                let child = internal.children_sorted().find(|(n, _)| u8::from(*n) == nibble);
                let Some((_, child)) = child else { break };
                key = codec::child_key(&key, nibble, child.version)?;
            }
        }
        Ok(())
    }

    /// Looks up the physical value row superseded by an exact update, never as logical authority.
    /// Error cases:
    /// - a bounded point query fails or its returned version is outside the requested range
    pub(super) fn value_version(&self, key: KeyHash) -> Result<Option<u64>> {
        let Some(version) = self.root.version else { return Ok(None) };
        self.charge(0)?;
        let mut stmt = self.conn.prepare(
            "SELECT version FROM value_rows WHERE namespace=?1 AND key=?2 AND version<=?3
             ORDER BY version DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![&self.root.namespace[..], &key.0[..], version as i64])?;
        let Some(row) = rows.next()? else { return Ok(None) };
        let found: i64 = row.get(0)?;
        ensure!(found >= 0 && found as u64 <= version, "invalid value row version");
        Ok(Some(found as u64))
    }

    /// Returns the checked native root count for exact per-operation overflow validation.
    /// Error cases:
    /// - the checked root is absent
    pub(super) fn leaf_count(&self) -> Result<usize> {
        let Some(version) = self.root.version else { return Ok(0) };
        let key = codec::root_key(version)?;
        let cache = self.cache.borrow();
        let count = match cache.get(&key) {
            Some(Node::Internal(node)) => node.leaf_count(),
            Some(Node::Leaf(_)) => 1,
            Some(Node::Null) => 0,
            None => bail!("missing checked root"),
        };
        Ok(count)
    }

    /// Reports whether a proposed retirement was actually read from the authenticated old graph.
    pub(super) fn saw(&self, key: &NodeKey) -> bool {
        self.cache.borrow().contains_key(key)
    }

    /// Loads a node with an exact primary-key lookup and validates its fixed-size wire fields.
    /// Error cases:
    /// - budget exhaustion, missing row, oversized BLOB, or malformed node
    fn fetch(&self, key: &NodeKey) -> Result<Node> {
        self.charge(0)?;
        ensure!(key.version() <= self.root.version.unwrap_or(0), "unexpected future node");
        let mut stmt = self.conn.prepare("SELECT data FROM nodes WHERE namespace=?1 AND version=?2 AND path=?3")?;
        let mut rows = stmt.query(params![&self.root.namespace[..], key.version() as i64, codec::path(key)?])?;
        let row = rows.next()?.ok_or_else(|| anyhow::anyhow!("missing authenticated node"))?;
        let raw = row.get_ref(0)?.as_blob()?;
        ensure!(raw.len() <= MAX_NODE_BYTES, "oversized node BLOB");
        self.charge_bytes(raw.len())?;
        codec::decode(key, raw)
    }

    /// Materializes one-hop children before JMT can reach its infallible sibling reader.
    /// Error cases:
    /// - a direct child is unavailable, malformed, or inconsistent with its authenticated parent
    fn prepare_children(&self, key: &NodeKey, node: &Node) -> Result<()> {
        let Node::Internal(internal) = node else { return Ok(()) };
        for (nibble, child) in internal.children_sorted() {
            let child_key = codec::child_key(key, u8::from(nibble), child.version)?;
            let cached = self.cache.borrow().get(&child_key).cloned();
            let loaded = match cached {
                Some(node) => node,
                None => self.fetch(&child_key)?,
            };
            ensure!(codec::hash(&loaded) == child.hash, "child hash mismatch");
            let count = match &loaded {
                Node::Null => bail!("null child"),
                Node::Leaf(_) => {
                    ensure!(child.is_leaf(), "child type mismatch");
                    1
                }
                Node::Internal(node) => {
                    ensure!(!child.is_leaf(), "child type mismatch");
                    node.leaf_count()
                }
            };
            ensure!(count == child.leaf_count(), "child leaf count mismatch");
            self.cache.borrow_mut().insert(child_key, loaded);
        }
        Ok(())
    }

    /// Charges one indexed SQL operation before it executes.
    /// Error cases:
    /// - the operation or byte budget is exhausted
    fn charge(&self, bytes: usize) -> Result<()> {
        self.reads_left.set(
            self.reads_left
                .get()
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("authenticated read operation budget exhausted"))?,
        );
        self.charge_bytes(bytes)
    }

    /// Charges host bytes before decoding or copying them.
    /// Error cases:
    /// - the byte budget is exhausted
    fn charge_bytes(&self, bytes: usize) -> Result<()> {
        self.bytes_left.set(
            self.bytes_left
                .get()
                .checked_sub(bytes)
                .ok_or_else(|| anyhow::anyhow!("authenticated read byte budget exhausted"))?,
        );
        Ok(())
    }
}

impl TreeReader for Reader<'_> {
    /// Returns only prepared nodes; upstream sibling helpers perform no further disk reads.
    /// Error cases:
    /// - upstream requests a node outside the prepared graph
    fn get_node_option(&self, key: &NodeKey) -> Result<Option<Node>> {
        if self.root.version.is_none() {
            if key == &codec::root_key(u64::MAX)? {
                return Ok(None);
            }
            ensure!(key == &codec::root_key(0)?, "unexpected pre-version node");
            return Ok(Some(Node::Null));
        }
        let node = self
            .cache
            .borrow()
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("node was not reached from a checked parent"))?;
        Ok(Some(node))
    }

    /// Reads a versioned value by primary-key prefix; the caller always verifies its proof.
    /// Error cases:
    /// - invalid version, exhausted budget, non-BLOB value, or oversized value
    fn get_value_option(&self, max_version: u64, key: KeyHash) -> Result<Option<Vec<u8>>> {
        ensure!(self.root.version == Some(max_version), "unexpected value version");
        self.charge(0)?;
        let mut stmt = self.conn.prepare(
            "SELECT version, data FROM value_rows WHERE namespace=?1 AND key=?2 AND version<=?3
             ORDER BY version DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![&self.root.namespace[..], &key.0[..], max_version as i64])?;
        let Some(row) = rows.next()? else { return Ok(None) };
        let version: i64 = row.get(0)?;
        ensure!(version >= 0 && version as u64 <= max_version, "invalid value version");
        let raw = row.get_ref(1)?;
        if raw == rusqlite::types::ValueRef::Null {
            return Ok(None);
        }
        let raw = raw.as_blob()?;
        ensure!(raw.len() <= MAX_STORED_VALUE, "value exceeds storage format");
        self.charge_bytes(raw.len())?;
        Ok(Some(raw.to_vec()))
    }

    /// Refuses restore/iteration: the utility exposes point operations only.
    /// Error cases:
    /// - always; tree restoration is outside this adapter
    fn get_rightmost_leaf(&self) -> Result<Option<(NodeKey, LeafNode)>> {
        bail!("rightmost-leaf scans are unsupported")
    }
}
