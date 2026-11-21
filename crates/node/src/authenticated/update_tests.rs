//! An update loads only the children upstream's update can read, and upstream's update finds every one.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use anyhow::Result;
use jmt::{JellyfishMerkleTree, KeyHash, RootHash};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::authenticated::blobs::chunk_key;
use crate::authenticated::reader::{Checked, Reader, update_children};
use crate::authenticated::tests::{fresh, put, raw};
use crate::authenticated::{Hasher, Root, key_hash};

/// On trees of 2 to 400 keys, for batches of inserts, rewrites and deletes of present and absent keys,
/// upstream's own update with proof, over a reader prepared only with the children `update_children`
/// names, finds every node it asks for, its proof verifies the transition, and every node it retires was
/// read. Each batch's previous values are read through another reader, so that no proof's loads stand in
/// for the update's.
#[test]
fn an_update_loads_every_child_upstreams_update_reads() -> Result<()> {
    let mut rng = StdRng::seed_from_u64(1114);
    for size in [2u32, 3, 5, 9, 17, 40, 120, 400] {
        let (dir, store) = fresh()?;
        let namespace = [51; 32];
        let callers: Vec<_> = (0..size).map(|n| n.to_be_bytes().to_vec()).collect();
        let root = store.update(&Root::empty(namespace), callers.iter().map(|key| put(key, b"value")).collect())?;
        let mut present = Vec::new();
        for caller in &callers {
            let descriptor = key_hash(&namespace, caller)?;
            present.extend([descriptor, chunk_key(&namespace, descriptor, 0)?]);
        }
        let conn = raw(&dir)?;
        for _ in 0..300 {
            let mut batch = BTreeMap::new();
            for _ in 0..rng.gen_range(1..=12) {
                let key =
                    if rng.gen_bool(0.6) { present[rng.gen_range(0..present.len())] } else { KeyHash(rng.r#gen()) };
                batch.insert(key, rng.gen_bool(0.5).then(|| vec![rng.r#gen::<u8>()]));
            }
            let (previous, checked) = (Checked::default(), Checked::default());
            let before = Reader::new(&conn, &root, true, &previous)?;
            let mut changes = Vec::new();
            for (key, value) in batch {
                if before.read(key)?.is_some() || value.is_some() {
                    changes.push((key, value));
                }
            }
            let reader = Reader::new(&conn, &root, true, &checked)?;
            reader.prepare(&changes.iter().map(|(key, _)| *key).collect::<Vec<_>>())?;
            let version = root.version.expect("a version") + 1;
            let update = catch_unwind(AssertUnwindSafe(|| {
                JellyfishMerkleTree::<_, Hasher>::new(&reader).put_value_set_with_proof(changes.clone(), version)
            }));
            let (hash, proof, written) =
                update.map_err(|_| anyhow::anyhow!("upstream's update panicked on {size} keys and {changes:?}"))??;
            proof.verify_update(RootHash(root.hash), hash, &changes)?;
            for stale in &written.stale_node_index_batch {
                assert!(reader.saw(&stale.node_key), "unread retirement on {size} keys and {changes:?}");
            }
        }
    }
    Ok(())
}

/// An update loads the children its keys pass through and the untouched ones alone in a range of theirs,
/// and no others: through one nibble of a full node it loads that child and its one-wide sibling, two of
/// sixteen.
#[test]
fn an_update_loads_only_the_children_upstreams_update_can_read() {
    assert_eq!(update_children(u16::MAX, 1), 0b11, "a full node through nibble 0");
    assert_eq!(update_children(0b1111, 0b0011), 0b0011, "the untouched pair beside two touched children");
    let sparse = (1 << 0) | (1 << 5) | (1 << 9);
    assert_eq!(update_children(sparse, 1), sparse, "each untouched child alone in its range");
    assert_eq!(update_children(0b0110, 1 << 8), 0, "a key through an empty half, beside a pair");
}
