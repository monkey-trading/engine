// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.26;

/// @dev Arbitrum's system precompile: the L2 block number, and the hashes of the 256 L2 blocks
/// before it (`arbBlockHash` reverts for any other).
interface ArbSys {
    function arbBlockNumber() external view returns (uint256);
    function arbBlockHash(uint256 arbBlockNum) external view returns (bytes32);
}

/// @dev Where Arbitrum serves `ArbSys`.
ArbSys constant ARB_SYS = ArbSys(address(0x64));
