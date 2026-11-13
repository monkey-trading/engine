use super::*;
use alloy_primitives::U256;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use serde::Deserialize;

use crate::account::Position;
use crate::types::{AccountId, MarketId};

/// An account leaf, its two position leaves, and three market leaves, with their keys, value
/// hashes, ABI encodings, the root of the tree holding the six, and each leaf's proof; and an
/// account leaf scored under `V11`, with a fraction of a bp and history behind its score; all
/// computed with Foundry's `cast` from the layout in the module docs.
const FIXTURE: &str = include_str!("../../tests/fixtures/merkle.json");

/// `FIXTURE`'s contents.
#[derive(Deserialize)]
struct Fixture {
    account: AccountId,
    account_key: B256,
    account_leaf: AccountLeaf,
    account_value_hash: B256,
    account_siblings: Vec<B256>,
    scored_account_leaf: AccountLeaf,
    scored_account_value_hash: B256,
    positions: Vec<FixturePosition>,
    markets: Vec<FixtureMarket>,
    root: B256,
}

/// One position of `FIXTURE`.
#[derive(Deserialize)]
struct FixturePosition {
    market: MarketId,
    key: B256,
    position: Position,
    value_hash: B256,
    siblings: Vec<B256>,
}

/// One market of `FIXTURE`.
#[derive(Deserialize)]
struct FixtureMarket {
    market: MarketId,
    key: B256,
    leaf: MarketLeaf,
    value_hash: B256,
    siblings: Vec<B256>,
}

#[test]
fn keys_leaves_the_root_and_proofs_match_an_evm_toolchain() {
    let fixture: Fixture = serde_json::from_str(FIXTURE).unwrap();
    assert_eq!(account_key(fixture.account), fixture.account_key);
    assert_eq!(fixture.account_leaf.value_hash(), fixture.account_value_hash);
    assert_eq!(fixture.scored_account_leaf.value_hash(), fixture.scored_account_value_hash);
    let mut leaves = vec![(fixture.account_key, fixture.account_value_hash)];
    for p in &fixture.positions {
        assert_eq!(position_key(fixture.account, p.market), p.key, "position in {}", p.market);
        assert_eq!(position_hash(&p.position), p.value_hash, "position in {}", p.market);
        leaves.push((p.key, p.value_hash));
    }
    for m in &fixture.markets {
        assert_eq!(market_key(m.market), m.key, "market {}", m.market);
        assert_eq!(m.leaf.value_hash(), m.value_hash, "market {}", m.market);
        leaves.push((m.key, m.value_hash));
    }
    let tree = built(&leaves);
    assert_eq!(tree.root(), fixture.root);
    let expected = [(fixture.account_key, &fixture.account_siblings)]
        .into_iter()
        .chain(fixture.positions.iter().map(|p| (p.key, &p.siblings)))
        .chain(fixture.markets.iter().map(|m| (m.key, &m.siblings)));
    for (key, siblings) in expected {
        assert_eq!(tree.proof(key).as_ref(), Some(siblings), "{key}");
    }
    // The account's two positions are in markets 1 and 2.
    let proof = AccountProof {
        seq: 0,
        root: fixture.root,
        account: fixture.account,
        leaf: fixture.account_leaf,
        siblings: fixture.account_siblings,
        positions: fixture
            .positions
            .iter()
            .map(|p| PositionProof { market: p.market, position: p.position, siblings: p.siblings.clone() })
            .collect(),
        markets: fixture.markets[..2]
            .iter()
            .map(|m| MarketProof { market: m.market, leaf: m.leaf.clone(), siblings: m.siblings.clone() })
            .collect(),
    };
    assert!(proof.verify(fixture.root));
}

#[test]
fn an_empty_tree_and_a_single_leaf_hash_as_the_layout_says() {
    let mut tree = SparseMerkleTree::default();
    assert_eq!(tree.root(), EMPTY);
    let (key, value) = (key(1), value(1));
    tree.update(BTreeMap::from([(key, Some(value))]));
    assert_eq!(tree.root(), leaf_hash(key, value));
    assert_eq!(tree.proof(key), Some(vec![]));
}

#[test]
fn the_root_is_the_layout_written_out_recursively() {
    let cases = (0..64u64).map(|n| (0..n).map(|i| (key(i), value(i))).collect::<Vec<_>>());
    // Keys that part only at the last bit, and at the first, put leaves at both extremes of depth.
    let deep = vec![(B256::ZERO, value(1)), (B256::with_last_byte(1), value(2)), (B256::repeat_byte(0xff), value(3))];
    for leaves in cases.chain([deep]) {
        let tree = built(&leaves);
        assert_eq!(tree.root(), reference_root(&leaves, 0), "{} leaves", leaves.len());
    }
}

#[test]
fn every_leaf_proves_against_the_root_and_nothing_else_does() {
    let mut leaves: Vec<(B256, B256)> = (0..300).map(|i| (key(i), value(i))).collect();
    leaves.push((B256::ZERO, value(1)));
    leaves.push((B256::with_last_byte(1), value(2)));
    let tree = built(&leaves);
    let root = tree.root();
    for (key, value) in &leaves {
        let siblings = tree.proof(*key).expect("a leaf has a proof");
        assert!(verify(root, *key, *value, &siblings));
        assert!(!verify(root, *key, *value ^ B256::with_last_byte(1), &siblings));
        assert!(!verify(root ^ B256::with_last_byte(1), *key, *value, &siblings));
        assert!(!verify(root, *key ^ B256::with_last_byte(1), *value, &siblings));
        let mut tampered = siblings.clone();
        tampered[0] ^= B256::with_last_byte(1);
        assert!(!verify(root, *key, *value, &tampered));
        assert!(!verify(root, *key, *value, &siblings[1..]));
        assert!(!verify(root, *key, *value, &[siblings.clone(), vec![EMPTY]].concat()));
    }
    // The two keys that part at the last bit sit at the bottom of the tree.
    assert_eq!(tree.proof(B256::ZERO).map(|siblings| siblings.len()), Some(256));
    assert_eq!(tree.proof(key(1_000)), None);
    assert!(!verify(root, B256::ZERO, value(1), &vec![EMPTY; 257]));
}

#[test]
fn incremental_updates_give_the_tree_a_fresh_build_gives() {
    let batch = vec((0..48usize, proptest::option::of(0..4u64)), 0..12);
    let mut runner = TestRunner::new(Config {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x5eed_0025),
        failure_persistence: None,
        ..Config::default()
    });
    let result = runner.run(&vec(batch, 1..12), |batches| {
        let pool = key_pool();
        let mut tree = SparseMerkleTree::default();
        let mut expected: BTreeMap<B256, B256> = BTreeMap::new();
        for batch in batches {
            let changes: BTreeMap<B256, Option<B256>> =
                batch.into_iter().map(|(k, v)| (pool[k], v.map(value))).collect();
            for (key, value) in &changes {
                match value {
                    Some(value) => expected.insert(*key, *value),
                    None => expected.remove(key),
                };
            }
            tree.update(changes);
            let fresh = built(&expected.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>());
            prop_assert_eq!(tree.root(), fresh.root());
            // Removals leave no node behind that a fresh build would not have.
            prop_assert_eq!(&tree.nodes, &fresh.nodes);
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
}

#[test]
fn an_update_hashes_only_the_nodes_above_leaves_that_changed() {
    let changed =
        |n: u64| -> BTreeMap<B256, Option<B256>> { (0..10).map(|i| (key(i * n / 10), Some(value(n + i)))).collect() };
    let mut small = built(&(0..1_000).map(|i| (key(i), value(i))).collect::<Vec<_>>());
    let mut large = built(&(0..100_000).map(|i| (key(i), value(i))).collect::<Vec<_>>());
    let (small_work, large_work) = (small.update(changed(1_000)), large.update(changed(100_000)));
    // Ten paths of about log2(n) nodes each, sharing their top: work grows with depth, not size.
    assert!(small_work <= 10 * 20 && large_work <= 10 * 26, "{small_work} / {large_work}");
    assert!(large_work < 2 * small_work, "{small_work} / {large_work}");
    // Setting leaves to the values they hold hashes nothing.
    let unchanged: BTreeMap<B256, Option<B256>> = (1..10).map(|i| (key(i), Some(value(i)))).collect();
    assert_eq!(small.update(unchanged), 0);
}

#[test]
fn an_open_time_leaf_is_keyed_as_its_position_is_and_hashes_its_int64_abi_word() {
    let (account, market): (AccountId, MarketId) = (AccountId([7; 20]), 3);
    let key = [&[27u8][..], &account.0, &market.to_be_bytes()].concat();
    assert_eq!(open_time_key(account, market), keccak(&[&key]));
    // `abi.encode(int64)`: the value sign-extended to a 32-byte big-endian word.
    let mut word = [0xff; 32];
    word[24..].copy_from_slice(&(-5i64).to_be_bytes());
    assert_eq!(open_time_hash(-5), keccak(&[&word]));
    let mut word = [0; 32];
    word[24..].copy_from_slice(&1_792_000_000_000i64.to_be_bytes());
    assert_eq!(open_time_hash(1_792_000_000_000), keccak(&[&word]));
}

// ── Helpers ──

/// A key spread over the key space, like the hashed keys the engine uses.
fn key(n: u64) -> B256 {
    keccak(&[b"key", &n.to_be_bytes()])
}

/// A value hash.
fn value(n: u64) -> B256 {
    keccak(&[b"value", &n.to_be_bytes()])
}

/// Spread keys, plus keys that share long prefixes, so updates make and remove deep nodes.
fn key_pool() -> Vec<B256> {
    let spread = (0..32).map(key);
    let clustered = (0..16u8).map(|n| {
        let mut key = B256::repeat_byte(0x5a);
        key[31] = n;
        key
    });
    spread.chain(clustered).collect()
}

/// A tree holding exactly `leaves`.
fn built(leaves: &[(B256, B256)]) -> SparseMerkleTree {
    let mut tree = SparseMerkleTree::default();
    tree.update(leaves.iter().map(|(k, v)| (*k, Some(*v))).collect());
    tree
}

/// The root by the layout's recursive definition, over leaves with distinct keys that share their
/// first `depth` bits: empty is zero, one leaf is its leaf hash, and more split on bit `depth`.
fn reference_root(leaves: &[(B256, B256)], depth: usize) -> B256 {
    match leaves {
        [] => B256::ZERO,
        [(key, value)] => keccak(&[&[0x00], key.as_slice(), value.as_slice()]),
        _ => {
            let right = |key: &B256| (U256::from_be_bytes(key.0) >> (255 - depth)) & U256::from(1) == U256::from(1);
            let (r, l): (Vec<_>, Vec<_>) = leaves.iter().partition(|(key, _)| right(key));
            let (left, right) = (reference_root(&l, depth + 1), reference_root(&r, depth + 1));
            keccak(&[&[0x01], left.as_slice(), right.as_slice()])
        }
    }
}
