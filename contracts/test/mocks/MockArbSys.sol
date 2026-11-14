// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

/// @notice A stand-in for Arbitrum's `ArbSys` precompile, etched at 0x64: the test sets the L2
/// block number apart from `block.number`, and block `n`'s hash is `hashOf(n)`. Like the precompile,
/// `arbBlockHash` reverts outside the 256 blocks before the current one.
contract MockArbSys {
    uint256 public arbBlockNumber;

    function setBlockNumber(uint256 blockNumber) external {
        arbBlockNumber = blockNumber;
    }

    function arbBlockHash(uint256 blockNumber) external view returns (bytes32) {
        require(blockNumber < arbBlockNumber && arbBlockNumber - blockNumber <= 256, "invalid block number");
        return hashOf(blockNumber);
    }

    function hashOf(uint256 blockNumber) public pure returns (bytes32) {
        return keccak256(abi.encode("arbitrum block", blockNumber));
    }
}
