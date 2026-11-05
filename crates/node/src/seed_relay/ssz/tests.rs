use super::*;

#[test]
fn zero_subtrees_hash_as_the_specification_lists_them() {
    // The consensus specification's `zerohashes[1]` and `zerohashes[2]`.
    assert_eq!(hex::encode(ZERO_HASHES[1]), "f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b");
    assert_eq!(hex::encode(ZERO_HASHES[2]), "db56114e00fdd4c1f85c892bf35ac9a89289aaecb1ebd0a96cde606a748b5d71");
    assert_eq!(merkleize(Vec::new(), 1 << 40).unwrap(), ZERO_HASHES[40]);
}

#[test]
fn chunks_are_padded_to_the_type_limit_and_lists_mix_in_their_length() {
    let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
    assert_eq!(merkleize(vec![a, b, c], 4).unwrap(), hash(&hash(&a, &b), &hash(&c, &[0; 32])));
    // The same chunks under a larger limit sit in a deeper tree.
    let deeper = hash(&hash(&hash(&a, &b), &hash(&c, &[0; 32])), &ZERO_HASHES[2]);
    assert_eq!(merkleize(vec![a, b, c], 8).unwrap(), deeper);
    assert!(merkleize(vec![a, b, c], 2).is_err());
    let mut length = [0u8; 32];
    length[0] = 3;
    assert_eq!(mix_in_length(deeper, 3), hash(&deeper, &length));
    // Five uint64s pack into two chunks; a list of them mixes in five.
    let bytes: Vec<u8> = (1u64..=5).flat_map(u64::to_le_bytes).collect();
    let packed = pack(&bytes);
    assert_eq!(packed.len(), 2);
    assert_eq!(packed[1][..8], 5u64.to_le_bytes());
    assert_eq!(packed[1][8..], [0u8; 24]);
    let expected = mix_in_length(merkleize(packed, 4).unwrap(), 5);
    assert_eq!(packed_list_root(&bytes, 8, 16).unwrap(), expected);
    assert!(packed_list_root(&bytes, 8, 4).is_err());
    assert!(packed_list_root(&bytes[..39], 8, 16).is_err());
}

#[test]
fn a_container_hashes_long_byte_vectors_as_their_own_trees() {
    // A 48-byte key is two chunks; the container's three fields fill a 4-leaf tree.
    let bytes: Vec<u8> = (0u8..48).chain([7u8; 8]).chain([9u8; 1]).collect();
    let key = merkleize(pack(&bytes[..48]), 2).unwrap();
    let expected = hash(&hash(&key, &pack(&bytes[48..56])[0]), &hash(&pack(&bytes[56..])[0], &[0; 32]));
    assert_eq!(container_root(&bytes, &[48, 8, 1]).unwrap(), expected);
    assert!(container_root(&bytes[1..], &[48, 8, 1]).is_err());
}

#[test]
fn every_leaf_has_a_branch_to_the_root_and_no_other_leaf_or_position_does() {
    let leaves: Vec<[u8; 32]> = (0u8..37).map(|i| [i; 32]).collect();
    let root = merkleize(leaves.clone(), 64).unwrap();
    for index in [0usize, 1, 13, 36, 37, 63] {
        let (branch_root, siblings) = branch(leaves.clone(), 6, index).unwrap();
        assert_eq!(branch_root, root);
        let leaf = leaves.get(index).copied().unwrap_or_default();
        assert!(is_valid_merkle_branch(leaf, &siblings, index, root), "leaf {index}");
        assert!(!is_valid_merkle_branch([0xee; 32], &siblings, index, root), "forged leaf {index}");
        // Two zero padding subtrees are the same hash, so only a real leaf has a position of its own.
        if index < leaves.len() {
            assert!(!is_valid_merkle_branch(leaf, &siblings, index ^ 4, root), "moved leaf {index}");
        }
        let mut tampered = siblings.clone();
        tampered[5].0[0] ^= 1;
        assert!(!is_valid_merkle_branch(leaf, &tampered, index, root), "tampered branch {index}");
    }
    assert!(branch(leaves.clone(), 5, 0).is_err());
    assert!(branch(leaves, 6, 64).is_err());
}
