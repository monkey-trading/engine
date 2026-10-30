// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

/// @notice Builds the engine's sparse Merkle tree over a few leaves in memory, for tests: the root
/// and each leaf's root-to-leaf siblings, under the layout `crates/core/src/merkle/mod.rs` documents.
library TreeBuilder {
    /// @dev The hash of the leaf at `key` with `valueHash`.
    function leaf(bytes32 key, bytes32 valueHash) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(bytes1(0x00), key, valueHash));
    }

    /// @dev The root of the tree holding `keys` with `values`, which must be distinct keys.
    function root(bytes32[] memory keys, bytes32[] memory values) internal pure returns (bytes32) {
        return _hash(keys, values, _all(keys.length), 0);
    }

    /// @dev The siblings proving `keys[index]` against `root(keys, values)`.
    function proof(bytes32[] memory keys, bytes32[] memory values, uint256 index)
        internal
        pure
        returns (bytes32[] memory siblings)
    {
        bytes32[] memory path = new bytes32[](256);
        uint256[] memory members = _all(keys.length);
        uint256 depth;
        while (members.length > 1) {
            bool right = _bit(keys[index], depth);
            (uint256[] memory left, uint256[] memory rightSide) = _split(keys, members, depth);
            path[depth] = _hash(keys, values, right ? left : rightSide, depth + 1);
            members = right ? rightSide : left;
            depth++;
        }
        siblings = new bytes32[](depth);
        for (uint256 i; i < depth; ++i) {
            siblings[i] = path[i];
        }
    }

    /// @dev The hash of the subtree at `depth` holding `members`, indexes into `keys`.
    function _hash(bytes32[] memory keys, bytes32[] memory values, uint256[] memory members, uint256 depth)
        private
        pure
        returns (bytes32)
    {
        if (members.length == 0) return bytes32(0);
        if (members.length == 1) return leaf(keys[members[0]], values[members[0]]);
        (uint256[] memory left, uint256[] memory right) = _split(keys, members, depth);
        return keccak256(
            abi.encodePacked(bytes1(0x01), _hash(keys, values, left, depth + 1), _hash(keys, values, right, depth + 1))
        );
    }

    /// @dev `members` split by bit `depth` of each key: 0 to the left, 1 to the right.
    function _split(bytes32[] memory keys, uint256[] memory members, uint256 depth)
        private
        pure
        returns (uint256[] memory left, uint256[] memory right)
    {
        uint256 rights;
        for (uint256 i; i < members.length; ++i) {
            if (_bit(keys[members[i]], depth)) rights++;
        }
        left = new uint256[](members.length - rights);
        right = new uint256[](rights);
        (uint256 l, uint256 r) = (0, 0);
        for (uint256 i; i < members.length; ++i) {
            if (_bit(keys[members[i]], depth)) right[r++] = members[i];
            else left[l++] = members[i];
        }
    }

    function _bit(bytes32 key, uint256 depth) private pure returns (bool) {
        return (uint256(key) >> (255 - depth)) & 1 == 1;
    }

    function _all(uint256 count) private pure returns (uint256[] memory members) {
        members = new uint256[](count);
        for (uint256 i; i < count; ++i) {
            members[i] = i;
        }
    }
}
