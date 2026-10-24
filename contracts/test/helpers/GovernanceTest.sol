// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

abstract contract GovernanceTest is Test {
    TimelockController internal governance;
    address internal multisig = makeAddr("deployment multisig");
    uint256 internal constant GOVERNANCE_DELAY = 2 days;
    uint256 private operationCount;

    function _deployGovernance() internal returns (TimelockController) {
        address[] memory proposers = new address[](1);
        proposers[0] = multisig;
        address[] memory executors = new address[](1);
        executors[0] = address(0);
        return new TimelockController(GOVERNANCE_DELAY, proposers, executors, address(0));
    }

    function _govern(address target, bytes memory data) internal {
        bytes32 salt = bytes32(operationCount++);
        uint256 delay = governance.getMinDelay();
        vm.prank(multisig);
        governance.schedule(target, 0, data, bytes32(0), salt, delay);
        vm.warp(block.timestamp + delay);
        governance.execute(target, 0, data, bytes32(0), salt);
    }
}
