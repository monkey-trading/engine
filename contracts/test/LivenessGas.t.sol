// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {console} from "forge-std/Test.sol";

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// Reports, before refunds and with cold storage as in their own transactions, a live and a held
/// heartbeat in steady state (the epoch already holding a head and the turn latched), a witness of a
/// new slot after an earlier one, a promotion past one member of four, and a standby key
/// registration.
contract LivenessGasTest is LivenessTest {
    function setUp() public {
        _deployLiveness();
        _startTurn();
    }

    function _cool() internal {
        vm.cool(address(logHeads));
        vm.cool(address(schedule));
        vm.cool(address(registry));
        vm.cool(address(vault));
        vm.cool(address(ARB));
    }

    function test_GasOfAHeartbeatAWitnessAPromotionAndARegistration() public {
        _beat();
        vm.warp(block.timestamp + 30);
        LogHeads.HeadV2 memory live = _nextHead();
        bytes memory liveSignature = _sign(live, _signerKey());
        _cool();
        logHeads.postV2(live, liveSignature);
        uint256 liveGas = vm.lastCallGas().gasTotalUsed;

        vm.warp(block.timestamp + 30);
        ARB.setBlockNumber(ARB.arbBlockNumber() + 120);
        LogHeads.HeadV2 memory held = _held(_nextHead(), 1, uint64(ARB.arbBlockNumber() - 1));
        bytes memory heldSignature = _sign(held, _signerKey());
        _cool();
        logHeads.postV2(held, heldSignature);
        uint256 heldGas = vm.lastCallGas().gasTotalUsed;

        _nextSlot(10, 240);
        schedule.witness();
        _nextSlot(10, 240);
        _cool();
        schedule.witness();
        uint256 witnessGas = vm.lastCallGas().gasTotalUsed;

        _nextSlot(10, 240);
        _witnessFromNow(MISSES - 2);
        _nextSlot(5, 240);
        _cool();
        schedule.promote();
        uint256 promoteGas = vm.lastCallGas().gasTotalUsed;

        address key = _freshStandbyKey();
        uint64 expiry = uint64(block.timestamp + 1 days);
        bytes memory vouch = _vouch(_signerKey(), alice, key, registry.candidateOf(alice).release, 1, expiry);
        _cool();
        vm.prank(alice);
        schedule.registerStandby(key, 1, expiry, vouch);
        uint256 registerGas = vm.lastCallGas().gasTotalUsed;

        console.log("live heartbeat, gas:", liveGas);
        console.log("held heartbeat, gas:", heldGas);
        console.log("witness, gas:", witnessGas);
        console.log("promotion, gas:", promoteGas);
        console.log("standby registration, gas:", registerGas);
        assertLt(liveGas, 110_000);
        assertLt(heldGas, 125_000);
        assertLt(witnessGas, 95_000);
        assertLt(promoteGas, 550_000);
        assertLt(registerGas, 115_000);
    }

    /// A member's own moves to the release it holds add revisions to its assignment. `assignmentHeadOf`
    /// answers the same assignment, count and latest revision as `assignmentOf` at a cost that
    /// does not grow with them, where `assignmentOf` pays about 5,000 gas for each.
    function test_AnAssignmentsHeadReadsAtACostThatDoesNotGrowWithItsRevisions() public {
        _beat();
        uint16 position = schedule.turnOf(turn).position;
        uint256[2] memory counts = [uint256(20), 220];
        uint256[2] memory headCosts;
        uint256[2] memory listCosts;
        uint256 made = 0;
        for (uint256 k = 0; k < 2; ++k) {
            for (; made < counts[k]; ++made) {
                vm.prank(alice);
                registry.setRelease(_digest(1), CONFIG);
            }
            (OperatorSchedule.Assignment memory assignment, OperatorSchedule.Revision[] memory list) =
                schedule.assignmentOf(turn, position);
            vm.cool(address(schedule));
            (OperatorSchedule.Assignment memory read, uint256 count, OperatorSchedule.Revision memory latest) =
                schedule.assignmentHeadOf(turn, position);
            headCosts[k] = vm.lastCallGas().gasTotalUsed;
            assertEq(abi.encode(read), abi.encode(assignment), "the same assignment");
            assertEq(count, list.length, "every revision counted");
            assertGt(count, counts[k], "the moves added revisions");
            assertEq(abi.encode(latest), abi.encode(list[count - 1]), "the latest revision");
            vm.cool(address(schedule));
            schedule.assignmentOf(turn, position);
            listCosts[k] = vm.lastCallGas().gasTotalUsed;
        }
        assertEq(headCosts[1], headCosts[0], "the head's cost does not grow");
        assertGt(listCosts[1], listCosts[0] + 200 * 4_000, "the list's cost does");
    }
}
