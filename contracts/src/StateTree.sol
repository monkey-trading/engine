// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

/// @title StateTree
/// @notice Proof checks against the engine's state root, the keccak-256 sparse Merkle tree that
/// `crates/core/src/merkle/` lays out.
library StateTree {
    /// @dev Whether `siblings`, from the root down, lead from the leaf at `key` with `valueHash` to
    /// `root` in the engine's sparse Merkle tree: a leaf hashes as `0x00 ‖ key ‖ valueHash`, a node as
    /// `0x01 ‖ left ‖ right`, and bit `d` of the key, most significant first, is the leaf's side at
    /// depth `d + 1`.
    function proves(bytes32 root, bytes32 key, bytes32 valueHash, bytes32[] calldata siblings)
        internal
        pure
        returns (bool)
    {
        if (siblings.length > 256) return false;
        bytes32 hash = keccak256(abi.encodePacked(bytes1(0x00), key, valueHash));
        for (uint256 depth = siblings.length; depth > 0;) {
            unchecked {
                --depth;
            }
            bool right = (uint256(key) >> (255 - depth)) & 1 == 1;
            hash = right
                ? keccak256(abi.encodePacked(bytes1(0x01), siblings[depth], hash))
                : keccak256(abi.encodePacked(bytes1(0x01), hash, siblings[depth]));
        }
        return hash == root;
    }
}
