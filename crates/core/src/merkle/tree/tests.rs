use std::collections::{BTreeMap, BTreeSet};

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};

use super::*;
use crate::Fixed;
use crate::emissions::TokenUnits;
use crate::merkle::tree::arena::Node;
use crate::merkle::{SparseMerkleTree, keccak};
use crate::types::AccountId;
use crate::withdrawal::QueuedWithdrawal;

#[test]
fn every_root_proof_and_absence_matches_the_oracle_through_random_batches() {
    let batch = vec((0..POOL, proptest::option::of(0..4u64)), 0..24);
    let result = runner(256, 0x925_0001).run(&vec(batch, 1..16), |batches| {
        let pool = key_pool();
        let (mut tree, mut oracle) = (StateTree::default(), SparseMerkleTree::default());
        for (seq, batch) in batches.into_iter().enumerate() {
            let changes: BTreeMap<B256, Option<B256>> =
                batch.into_iter().map(|(k, v)| (pool[k], v.map(value))).collect();
            oracle.update(changes.clone());
            tree.advance(changes.into_iter().map(|(key, hash)| LeafUpdate::hashed(key, hash)).collect());
            let (root, _) = tree.commit(seq as u64, ProtocolVersion::LATEST, None);
            prop_assert!(tree.release(seq as u64, root));
            tree.collect(usize::MAX);
            prop_assert_eq!(root, oracle.root());
            let version = tree.released().expect("released above");
            // The pool's keys, present or not, and keys it never holds.
            for key in pool.iter().copied().chain((1_000..1_016).map(key)) {
                let proof = tree.arena.proven(version.root, &key).map(|(_, siblings)| siblings);
                prop_assert_eq!(proof, oracle.proof(key), "{}", key);
                prop_assert_eq!(tree.arena.absence(version.root, &key), oracle.absence(key), "{}", key);
            }
            prop_assert_eq!(live(&tree), reachable(&tree));
            // Committing and releasing each root at once keeps one version, as an inline engine does.
            prop_assert_eq!(tree.versions.len(), 1);
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
}

#[test]
fn kept_versions_prove_their_own_leaves_and_free_exactly_what_they_alone_held() {
    let op = prop_oneof![
        4 => vec((0..POOL, proptest::option::of(0..4u64)), 0..12).prop_map(Op::Write),
        2 => Just(Op::Commit),
        2 => (0..3usize).prop_map(Op::Release),
        1 => Just(Op::Reset),
        1 => (0..3usize).prop_map(Op::Pin),
        1 => Just(Op::Unpin),
    ];
    let result = runner(256, 0x925_0002).run(&vec(op, 1..48), |ops| {
        let pool = key_pool();
        let mut tree = StateTree::default();
        let mut working: BTreeMap<B256, B256> = BTreeMap::new();
        // Each kept version's id with the leaves it holds; and the pins taken.
        let mut kept: Vec<(u64, BTreeMap<B256, B256>)> = Vec::new();
        let mut pins: Vec<u64> = Vec::new();
        let mut seq = 0;
        for op in ops {
            match op {
                Op::Write(batch) => {
                    let changes: BTreeMap<B256, Option<B256>> =
                        batch.into_iter().map(|(k, v)| (pool[k], v.map(value))).collect();
                    for (key, hash) in &changes {
                        match hash {
                            Some(hash) => working.insert(*key, *hash),
                            None => working.remove(key),
                        };
                    }
                    tree.advance(changes.into_iter().map(|(key, hash)| LeafUpdate::hashed(key, hash)).collect());
                }
                Op::Commit => {
                    seq += 1;
                    tree.commit(seq, ProtocolVersion::LATEST, None);
                    kept.push((tree.newest().expect("committed").id, working.clone()));
                }
                Op::Release(back) => {
                    let Some(version) = tree.versions.iter().rev().nth(back) else { continue };
                    let (id, seq, root) = (version.id, version.seq, version.hash);
                    prop_assert!(tree.release(seq, root));
                    let position = kept.iter().position(|(kept_id, _)| *kept_id == id).expect("kept");
                    let mut index = 0;
                    kept.retain(|(kept_id, _)| {
                        index += 1;
                        index > position || pins.contains(kept_id)
                    });
                }
                Op::Reset => {
                    tree.reset();
                    working = kept.last().map(|(_, leaves)| leaves.clone()).unwrap_or_default();
                }
                Op::Pin(back) => {
                    let Some((id, _)) = kept.iter().rev().nth(back) else { continue };
                    tree.pin(*id);
                    pins.push(*id);
                }
                Op::Unpin => {
                    let Some(id) = pins.pop() else { continue };
                    tree.unpin(id);
                    // The version goes if a newer one was released meanwhile and no other pin holds it.
                    let newest_released = tree.released().map(|v| v.id);
                    let dropped = tree.version(id).is_none();
                    prop_assert_eq!(dropped, newest_released.is_some_and(|newest| newest > id) && !pins.contains(&id));
                    if dropped {
                        kept.retain(|(kept_id, _)| *kept_id != id);
                    }
                }
            }
            let ids: Vec<u64> = tree.versions.iter().map(|v| v.id).collect();
            prop_assert_eq!(ids, kept.iter().map(|(id, _)| *id).collect::<Vec<_>>());
            for (id, leaves) in &kept {
                let version = tree.version(*id).expect("kept");
                let oracle = oracle_of(leaves);
                prop_assert_eq!(version.hash, oracle.root());
                for key in &pool {
                    let proof = tree.arena.proven(version.root, key).map(|(_, siblings)| siblings);
                    prop_assert_eq!(proof, oracle.proof(*key));
                    prop_assert_eq!(tree.arena.absence(version.root, key), oracle.absence(*key));
                }
            }
            tree.collect(usize::MAX);
            prop_assert_eq!(live(&tree), reachable(&tree));
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
}

#[test]
fn a_version_keeps_its_values_while_later_versions_change_them() {
    let (alice, bob) = (AccountId([1; 20]), AccountId([2; 20]));
    let entitled = |account, units| {
        let key = crate::merkle::entitlement_key(account);
        let value = LeafValue::Entitlement(TokenUnits(units));
        LeafUpdate::valued(key, Some((crate::merkle::entitlement_hash(TokenUnits(units)), value)))
    };
    let mut tree = StateTree::default();
    tree.advance(vec![entitled(alice, 5), entitled(bob, 7)]);
    let (first, _) = tree.commit(1, ProtocolVersion::LATEST, None);
    assert!(tree.release(1, first));
    tree.advance(vec![entitled(alice, 9)]);
    let (second, _) = tree.commit(2, ProtocolVersion::LATEST, None);
    let proven = |tree: &StateTree, seq: u64, account| {
        let version = tree.at_seq(seq).expect("kept");
        tree.entitlement_proof(version, account).map(|proof| (proof.root, proof.entitled.0))
    };
    // The second version is unreleased: the first stays, holding its own values, beside it.
    assert_eq!(proven(&tree, 1, alice), Some((first, 5)));
    assert_eq!(proven(&tree, 2, alice), Some((second, 9)));
    assert_eq!(proven(&tree, 2, bob), Some((second, 7)));
    assert!(tree.release(2, second));
    assert!(tree.at_seq(1).is_none(), "a released version drops the ones before it");
    tree.collect(usize::MAX);
    assert_eq!(live(&tree), reachable(&tree));
    let proof = tree.entitlement_proof(tree.released().expect("released"), alice).expect("held");
    assert!(proof.verify(second));
}

#[test]
fn a_reset_restores_the_newest_version_and_frees_what_the_working_version_made() {
    let pool = key_pool();
    let mut tree = StateTree::default();
    tree.advance(pool.iter().take(20).map(|key| LeafUpdate::hashed(*key, Some(value(1)))).collect());
    let (root, _) = tree.commit(1, ProtocolVersion::LATEST, None);
    assert!(tree.release(1, root));
    // Insert and remove the same keys within one working version, then change others, and reset.
    for round in 0..3 {
        tree.advance(pool[20..].iter().map(|key| LeafUpdate::hashed(*key, Some(value(round)))).collect());
        tree.advance(pool[20..].iter().map(|key| LeafUpdate::hashed(*key, None)).collect());
    }
    tree.advance(pool.iter().take(5).map(|key| LeafUpdate::hashed(*key, Some(value(9)))).collect());
    tree.reset();
    tree.collect(usize::MAX);
    assert_eq!(live(&tree), reachable(&tree));
    let (again, work) = tree.commit(2, ProtocolVersion::LATEST, None);
    assert_eq!(again, root, "the working version is the newest one again");
    assert_eq!(work.nodes, 0, "a reset leaves nothing to hash");
}

#[test]
fn an_unchanged_write_copies_and_hashes_nothing() {
    let pool = key_pool();
    let mut tree = StateTree::default();
    let writes = || pool.iter().map(|key| LeafUpdate::hashed(*key, Some(value(3)))).collect::<Vec<_>>();
    tree.advance(writes());
    let (root, _) = tree.commit(1, ProtocolVersion::LATEST, None);
    assert!(tree.release(1, root));
    let before = live(&tree);
    tree.advance(writes());
    tree.advance(vec![LeafUpdate::hashed(key(10_000), None)]);
    assert_eq!(live(&tree), before, "no path was copied");
    let (again, work) = tree.commit(2, ProtocolVersion::LATEST, None);
    assert_eq!((again, work.nodes), (root, 0));
}

#[test]
fn an_exit_entry_an_older_pinned_version_holds_is_pruned_once_that_version_goes() {
    let account = AccountId([3; 20]);
    let queued = QueuedWithdrawal {
        account,
        amount: Fixed::from_int(5),
        remaining: Fixed::from_int(5),
        requested_seq: 7,
        ticket: 1,
    };
    let queue_key = crate::merkle::withdrawal_queue_key(7);
    let hash = crate::merkle::withdrawal_queue_hash(&queued, ProtocolVersion::LATEST);
    let mut tree = StateTree::default();
    tree.advance(vec![LeafUpdate::valued(queue_key, Some((hash, LeafValue::Queued(queued))))]);
    let (first, _) = tree.commit(1, ProtocolVersion::LATEST, None);
    assert!(tree.release(1, first));
    // A read pins the first version; the second holds the entry too.
    let pinned = tree.released().expect("released").id();
    tree.pin(pinned);
    tree.advance(vec![LeafUpdate::hashed(key(1), Some(value(1)))]);
    let (second, _) = tree.commit(2, ProtocolVersion::LATEST, None);
    assert!(tree.release(2, second));
    // The third lets the entry go, and its release drops the second while the pinned first holds it.
    tree.advance(vec![LeafUpdate::valued(queue_key, None)]);
    let (third, _) = tree.commit(3, ProtocolVersion::LATEST, None);
    assert!(tree.release(3, third));
    assert!(tree.index.queued.contains_key(&account), "the pinned version still finds it");
    tree.unpin(pinned);
    assert!(tree.version(pinned).is_none(), "the first version goes with its pin");
    assert!(!tree.index.queued.contains_key(&account), "no version holds the entry: the index lets it go");
}

// ── Helpers ──

/// How many keys `key_pool` holds.
const POOL: usize = 56;

/// One step of the versions test.
#[derive(Clone, Debug)]
enum Op {
    Write(Vec<(usize, Option<u64>)>),
    Commit,
    /// Release the version this many from the newest.
    Release(usize),
    Reset,
    /// Pin the kept version this many from the newest.
    Pin(usize),
    Unpin,
}

fn runner(cases: u32, seed: u64) -> TestRunner {
    TestRunner::new(Config { cases, rng_seed: RngSeed::Fixed(seed), failure_persistence: None, ..Config::default() })
}

/// A key spread over the key space, like the hashed keys the engine uses.
fn key(n: u64) -> B256 {
    keccak(&[b"key", &n.to_be_bytes()])
}

/// A value hash.
fn value(n: u64) -> B256 {
    keccak(&[b"value", &n.to_be_bytes()])
}

/// Spread keys; keys sharing 248 bits, so writes make and collapse long branch chains; and keys
/// sharing all but their last bit.
fn key_pool() -> Vec<B256> {
    let spread = (0..32).map(key);
    let clustered = (0..20u8).map(|n| {
        let mut key = B256::repeat_byte(0x5a);
        key[31] = n;
        key
    });
    let twins = (0..4u8).map(|n| {
        let mut key = B256::repeat_byte(0xc3);
        key[31] = 0xf0 | (n & 1);
        key[30] = n >> 1;
        key
    });
    let pool: Vec<B256> = spread.chain(clustered).chain(twins).collect();
    assert_eq!(pool.len(), POOL);
    pool
}

/// The oracle tree holding exactly `leaves`.
fn oracle_of(leaves: &BTreeMap<B256, B256>) -> SparseMerkleTree {
    let mut tree = SparseMerkleTree::default();
    tree.update(leaves.iter().map(|(k, v)| (*k, Some(*v))).collect());
    tree
}

/// The branches and leaves in use.
fn live(tree: &StateTree) -> (usize, usize) {
    tree.arena.live()
}

/// The distinct branches and leaves the kept versions and the working one reach.
fn reachable(tree: &StateTree) -> (usize, usize) {
    let (mut branches, mut leaves) = (BTreeSet::new(), BTreeSet::new());
    let mut stack: Vec<NodeRef> = tree.versions.iter().map(|v| v.root).chain([tree.working]).collect();
    while let Some(node) = stack.pop() {
        match node.node() {
            Node::Empty => {}
            Node::Leaf(index) => _ = leaves.insert(index),
            Node::Branch(index) => {
                if branches.insert(index) {
                    stack.extend(tree.arena.branch(index).children);
                }
            }
        }
    }
    (branches.len(), leaves.len())
}
