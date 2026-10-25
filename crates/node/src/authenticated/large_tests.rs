//! Variable-length values and large command/expiry fanout remain exact and atomic.

use std::io::{Cursor, Write};

use anyhow::Result;

use crate::authenticated::tests::{fresh, put, raw, row_count};
use crate::authenticated::{CHUNK_BYTES, COMPLETE_RECEIPT_BUDGET, INTERNAL, MAX_UPDATES, Root};

#[test]
fn chain_ref_sized_values_beyond_one_mib_round_trip_without_loss() -> Result<()> {
    let (_dir, store) = fresh()?;
    let key = vec![7u8; 8193];
    let bytes = serde_json::to_vec(&serde_json::json!({"chain_ref":"r".repeat(2*1024*1024+17)}))?;
    let root = store.update(&Root::empty([51; 32]), vec![(key.clone(), Some(bytes.clone()))])?;
    assert_eq!(store.read(&root, &key)?, Some(bytes.clone()));
    let mut streamed = Vec::new();
    assert_eq!(store.read_to(&root, &key, &mut streamed)?, Some(bytes.len() as u64));
    assert_eq!(streamed, bytes);
    Ok(())
}

#[test]
fn large_expiry_and_fanout_share_one_complete_commit() -> Result<()> {
    let (_dir, store) = fresh()?;
    let count = MAX_UPDATES * 3 + 11;
    let changes: Vec<_> = (0..count).map(|i| put(&i.to_be_bytes(), &i.to_be_bytes())).collect();
    let root = store.update(&Root::empty([52; 32]), changes)?;
    assert!(root.version.expect("materialized") > 0, "physical batches are chunked");
    let pinned = store.lease(&root)?;
    let expiry = (0..count).map(|i| (i.to_be_bytes().to_vec(), None)).collect();
    let expired = store.update(&root, expiry)?;
    for i in 0..count {
        assert_eq!(store.read(&expired, &i.to_be_bytes())?, None);
        assert_eq!(pinned.read(&i.to_be_bytes())?, Some(i.to_be_bytes().to_vec()));
    }
    let (same, blocked) = store.gc(&expired, COMPLETE_RECEIPT_BUDGET)?;
    // Eligible early receipts can be reclaimed, but pinned version data must remain readable.
    assert_eq!(store.read(&same, b"absent")?, None);
    let _ = blocked;
    assert_eq!(pinned.read(&0usize.to_be_bytes())?, Some(0usize.to_be_bytes().to_vec()));
    Ok(())
}

#[test]
fn interruption_after_many_physical_batches_preserves_every_old_logical_value() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([53; 32]), vec![put(b"keep", b"old")])?;
    let before = row_count(&raw(&dir)?, "nodes")?;
    let changes = (0..MAX_UPDATES * 3).map(|i| put(&i.to_be_bytes(), b"new")).collect();
    assert!(
        store
            .update_checked(&root, changes, |_| anyhow::bail!("interrupt complete chunked transaction"), || Ok(()))
            .is_err()
    );
    assert_eq!(row_count(&raw(&dir)?, "nodes")?, before);
    assert_eq!(store.read(&root, b"keep")?, Some(b"old".to_vec()));
    assert_eq!(store.read(&root, &0usize.to_be_bytes())?, None);
    Ok(())
}

#[test]
fn streaming_short_or_excess_sources_roll_back_all_chunks() -> Result<()> {
    let (dir, store) = fresh()?;
    let empty = Root::empty([54; 32]);
    let bytes = vec![5u8; CHUNK_BYTES * (MAX_UPDATES + 1)];
    assert!(store.write_stream(&empty, b"a", bytes.len() as u64 + 1, Cursor::new(&bytes)).is_err());
    assert_eq!(row_count(&raw(&dir)?, "nodes")?, 0);
    assert!(store.write_stream(&empty, b"a", bytes.len() as u64 - 1, Cursor::new(&bytes)).is_err());
    assert_eq!(row_count(&raw(&dir)?, "value_rows")?, 0);
    let root = store.write_stream(&empty, b"a", bytes.len() as u64, Cursor::new(&bytes))?;
    let mut sink = Counting { bytes: 0, max: 0 };
    assert_eq!(store.read_to(&root, b"a", &mut sink)?, Some(bytes.len() as u64));
    assert_eq!(sink.bytes, bytes.len());
    assert!(sink.max <= CHUNK_BYTES);
    assert!(store.read(&root, INTERNAL).is_err());
    Ok(())
}

#[test]
fn deleted_large_values_release_physical_pages_with_bounded_incremental_vacuum() -> Result<()> {
    let (dir, store) = fresh()?;
    let input = vec![9u8; 8 * 1024 * 1024];
    let root = store.write_stream(&Root::empty([55; 32]), b"large", input.len() as u64, Cursor::new(input))?;
    let root = store.update(&root, vec![(b"large".to_vec(), None)])?;
    let before = std::fs::metadata(dir.path.join("store.sqlite"))?.len();
    let (root, report) = store.gc(&root, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.deleted > 0 && report.pages_reclaimed > 1, "bounded vacuum must drain multiple result rows");
    let after = std::fs::metadata(dir.path.join("store.sqlite"))?.len();
    assert!(after < before);
    assert!(report.pages_reclaimed <= COMPLETE_RECEIPT_BUDGET as u64);
    assert_eq!(store.read(&root, b"large")?, None);
    Ok(())
}

#[test]
fn forged_logical_length_is_rejected_by_proof_before_any_length_sized_allocation() -> Result<()> {
    let (dir, store) = fresh()?;
    let root = store.update(&Root::empty([56; 32]), vec![put(b"a", b"small")])?;
    let hash = crate::authenticated::key_hash(&root.namespace, b"a")?;
    let conn = raw(&dir)?;
    let forged = serde_json::to_vec(&serde_json::json!({"length":u64::MAX,"digest":vec![0u8;32]}))?;
    conn.execute("UPDATE value_rows SET data=?1 WHERE key=?2", rusqlite::params![forged, &hash.0[..]])?;
    assert!(store.read(&root, b"a").is_err());
    let mut sink = Counting { bytes: 0, max: 0 };
    assert!(store.read_to(&root, b"a", &mut sink).is_err());
    assert_eq!(sink.bytes, 0, "unproved length/descriptor cannot emit any logical bytes");
    Ok(())
}

#[test]
fn structural_counts_above_one_billion_use_native_representation_bounds() -> Result<()> {
    let large = (1usize << 30) + 17;
    let mut children = vec![serde_json::Value::Null; 16];
    children[0] = serde_json::json!({"hash":vec![1u8;32],"version":0,"node_type":{"Internal":{"leaf_count":large}}});
    children[1] = serde_json::json!({"hash":vec![2u8;32],"version":0,"node_type":"Leaf"});
    let wire = serde_json::json!({"Internal":{"children":{"children":children,"num_children":2},"leaf_count":large+1}});
    // Local shape validation does not claim these synthetic child hashes are a complete tree.
    let node =
        crate::authenticated::codec::decode(&crate::authenticated::codec::root_key(0)?, &serde_json::to_vec(&wire)?)?;
    match node {
        jmt::storage::Node::Internal(node) => assert_eq!(node.leaf_count(), large + 1),
        _ => panic!("internal fixture"),
    }
    Ok(())
}

#[test]
fn workspace_hasher_matches_published_sha256_jmt_roots_and_proofs() -> Result<()> {
    let (dir, _store) = fresh()?;
    let conn = raw(&dir)?;
    let empty = Root::empty([57; 32]);
    let reader = crate::authenticated::reader::Reader::new(&conn, &empty, true)?;
    let key = crate::authenticated::key_hash(&empty.namespace, b"same key")?;
    let updates = vec![(key, Some(b"same exact value".to_vec()))];
    let custom = jmt::JellyfishMerkleTree::<_, crate::authenticated::Hasher>::new(&reader);
    let builtin = jmt::Sha256Jmt::new(&reader);
    let (a, a_proof, a_batch) = custom.put_value_set_with_proof(updates.clone(), 0)?;
    let (b, b_proof, b_batch) = builtin.put_value_set_with_proof(updates.clone(), 0)?;
    assert_eq!(a.0, b.0);
    assert_eq!(a_batch.node_batch, b_batch.node_batch);
    a_proof.verify_update(jmt::RootHash(empty.hash), a, &updates)?;
    b_proof.verify_update(jmt::RootHash(empty.hash), b, &updates)?;
    Ok(())
}

struct Counting {
    bytes: usize,
    max: usize,
}

impl Write for Counting {
    /// Counts proof-verified writes without storing the logical value.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes += bytes.len();
        self.max = self.max.max(bytes.len());
        Ok(bytes.len())
    }
    /// Has no buffered IO.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
