// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

/// @notice A stand-in for Arbitrum's `ArbSys` precompile on a local chain, etched at 0x64 by the dev
/// scenarios: the chain's own blocks are its L2 blocks, so a node binds a held head to a block it
/// read, and `arbBlockHash` reverts outside the 256 blocks before the current one, as the precompile
/// does.
contract ChainArbSys {
    function arbBlockNumber() external view returns (uint256) {
        return block.number;
    }

    function arbBlockHash(uint256 blockNumber) external view returns (bytes32) {
        require(blockNumber < block.number && block.number - blockNumber <= 256, "invalid block number");
        return blockhash(blockNumber);
    }
}
