//! Two-pass receipt construction; only the final exact JMT proof authorizes persistence.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use jmt::storage::TreeUpdateBatch;
use jmt::{JellyfishMerkleTree, RootHash};
use rusqlite::{Connection, params};

use crate::authenticated::reader::Reader;
use crate::authenticated::receipts::{Queue, Receipt, RetiredRow, queue_key, receipt_key};
use crate::authenticated::{
    Changes, Hasher, MAX_RECEIPT_BYTES, MAX_RECEIPT_ROWS, MAX_STORED_VALUE, MAX_UPDATES, PHYSICAL_UPDATES, Root, codec,
};

pub(super) struct Prepared {
    pub root: Root,
    pub batch: TreeUpdateBatch,
}

/// Proves placeholder and final transitions from the same old root and requires exact retirements.
/// Error cases:
/// - wrong queue/key bounds, failed old/new proofs, overflow or different two-pass retirement sets
pub(super) fn prepare(
    conn: &Connection,
    old: &Root,
    changes: Changes,
    mut queue: Queue,
    maintenance: bool,
) -> Result<Prepared> {
    ensure!(changes.len() <= MAX_UPDATES, "physical batch must be chunked");
    let version = old
        .version
        .map_or(Some(0), |v| v.checked_add(1))
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or_else(|| anyhow::anyhow!("version exhausted"))?;
    ensure!(queue.hi == version && queue.lo <= queue.hi, "queue version mismatch");
    queue.hi = version + 1;
    let count = changes.len();
    let mut exact: BTreeMap<_, _> = changes.into_iter().collect();
    ensure!(exact.len() == count, "duplicate physical keys");
    ensure!(
        exact.insert(queue_key(&old.namespace)?, Some(serde_json::to_vec(&queue)?)).is_none(),
        "reserved queue collision"
    );
    let receipt_key = receipt_key(&old.namespace, version)?;
    ensure!(!exact.contains_key(&receipt_key), "reserved receipt collision");
    let mut receipt = Receipt { old: old.clone(), version, maintenance, rows: Vec::new() };
    exact.insert(receipt_key, Some(serde_json::to_vec(&receipt)?));
    // Both passes read the same immutable old root, so they share one checked node cache.
    let reader = Reader::new(conn, old, true)?;
    let preliminary = generate(&reader, old, exact.iter().map(|(k, v)| (*k, v.clone())).collect())?;
    receipt.rows = retirements(&reader, old, &preliminary.batch, &preliminary.exact, version)?;
    ensure!(receipt.rows.len() <= MAX_RECEIPT_ROWS, "receipt exceeds derived physical-batch envelope");
    let raw = serde_json::to_vec(&receipt)?;
    ensure!(
        raw.len() <= MAX_RECEIPT_BYTES && raw.len() <= MAX_STORED_VALUE,
        "receipt encoding exceeds derived envelope"
    );
    exact.insert(receipt_key, Some(raw));
    let final_batch = generate(&reader, old, exact.into_iter().collect())?;
    ensure!(
        preliminary.batch.stale_node_index_batch == final_batch.batch.stale_node_index_batch,
        "two-pass stale-node sets differ"
    );
    ensure!(
        preliminary.batch.node_batch.nodes().keys().eq(final_batch.batch.node_batch.nodes().keys()),
        "two-pass new-node keys differ"
    );
    let final_rows = retirements(&reader, old, &final_batch.batch, &final_batch.exact, version)?;
    ensure!(receipt.rows == final_rows, "two-pass retired rows differ");
    receipt.validate(&final_batch.root, version)?;
    Ok(Prepared { root: final_batch.root, batch: final_batch.batch })
}

struct Generated {
    root: Root,
    batch: TreeUpdateBatch,
    exact: Changes,
}

/// Generates and consumes the upstream proof for the exact effective sorted key/value set.
/// Error cases:
/// - resource/representation failure, malformed disk graph, failed proof or unexpected batch data
fn generate(reader: &Reader<'_>, old: &Root, exact: Changes) -> Result<Generated> {
    ensure!(exact.len() <= PHYSICAL_UPDATES, "excessive physical changes");
    reader.prepare(&exact.iter().map(|(key, _)| *key).collect::<Vec<_>>())?;
    let mut effective = Vec::new();
    let mut count = reader.leaf_count()?;
    for (key, value) in exact {
        if let Some(raw) = &value {
            ensure!(raw.len() <= MAX_STORED_VALUE, "oversized physical value");
        }
        let previous = reader.read(key)?;
        if previous.is_none() && value.is_none() {
            continue;
        }
        count = match (previous.is_some(), value.is_some()) {
            (false, true) => count.checked_add(1).ok_or_else(|| anyhow::anyhow!("JMT native leaf count overflow"))?,
            (true, false) => count.checked_sub(1).ok_or_else(|| anyhow::anyhow!("JMT native leaf count underflow"))?,
            _ => count,
        };
        effective.push((key, value));
    }
    let version = old.version.map_or(0, |v| v + 1);
    let tree = JellyfishMerkleTree::<_, Hasher>::new(reader);
    let (hash, proof, batch) = tree.put_value_set_with_proof(effective.clone(), version)?;
    proof.verify_update(RootHash(old.hash), hash, &effective)?;
    let expected: BTreeMap<_, _> = effective.iter().map(|(key, value)| ((version, *key), value.clone())).collect();
    ensure!(batch.node_batch.values() == &expected, "upstream value batch differs from exact changes");
    for stale in &batch.stale_node_index_batch {
        ensure!(
            stale.stale_since_version == version && stale.node_key.version() < version && reader.saw(&stale.node_key),
            "stale node lacks old-graph provenance"
        );
    }
    ensure!(batch.node_batch.nodes().len() <= PHYSICAL_UPDATES * 66 + 1, "generated nodes exceed path envelope");
    Ok(Generated {
        root: Root { namespace: old.namespace, version: Some(version), hash: hash.0 },
        batch,
        exact: effective,
    })
}

/// Derives each retirement from the verified upstream batch and proved superseded values.
/// Error cases:
/// - a required old value/version is unavailable or a generated retirement is invalid
fn retirements(
    reader: &Reader<'_>,
    old: &Root,
    batch: &TreeUpdateBatch,
    changes: &Changes,
    version: u64,
) -> Result<Vec<RetiredRow>> {
    let mut rows = BTreeSet::new();
    for stale in &batch.stale_node_index_batch {
        rows.insert(RetiredRow::Node(stale.node_key.clone()));
    }
    if let Some(previous) = old.version {
        rows.insert(RetiredRow::Node(codec::root_key(previous)?));
    }
    for (key, _) in changes {
        if reader.read(*key)?.is_some() {
            let previous =
                reader.value_version(*key)?.ok_or_else(|| anyhow::anyhow!("missing superseded value row"))?;
            ensure!(previous < version, "future superseded value");
            rows.insert(RetiredRow::Value { key: *key, version: previous });
        }
    }
    Ok(rows.into_iter().collect())
}

/// Inserts immutable nodes and live values, then proves every exact written result in this transaction.
/// Error cases:
/// - version collision, malformed generated node or mismatch in actual persisted bytes
pub(super) fn persist(conn: &Connection, prepared: &Prepared) -> Result<()> {
    let root = &prepared.root;
    let version = root.version.ok_or_else(|| anyhow::anyhow!("unversioned generated root"))?;
    for (key, node) in prepared.batch.node_batch.nodes() {
        ensure!(key.version() == version, "generated node version mismatch");
        let raw = serde_json::to_vec(node)?;
        codec::decode(key, &raw)?;
        conn.execute(
            "INSERT INTO nodes(namespace,version,path,data) VALUES(?1,?2,?3,?4)",
            params![&root.namespace[..], version as i64, codec::path(key)?, raw],
        )?;
    }
    for ((row_version, key), value) in prepared.batch.node_batch.values() {
        // Missing leaves never read value rows; omitting tombstones avoids permanent per-delete garbage.
        if let Some(value) = value {
            conn.execute(
                "INSERT INTO value_rows(namespace,key,version,data) VALUES(?1,?2,?3,?4)",
                params![&root.namespace[..], &key.0[..], *row_version as i64, value],
            )?;
        }
    }
    let reader = Reader::new(conn, root, true)?;
    for ((_, key), expected) in prepared.batch.node_batch.values() {
        ensure!(reader.read(*key)?.as_deref() == expected.as_deref(), "persisted exact physical value mismatch");
    }
    Ok(())
}
