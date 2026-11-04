// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";

/// Relays seeds for a span of turns, from the relay and from strangers, at random times, and reads
/// every turn's final seed whenever it may.
contract SeedReceiverHandler is Test {
    uint64 internal constant BASE = 20763;
    uint64 internal constant SPAN = 12;

    SeedReceiver internal immutable receiver;
    /// Each turn's seed the first time `seedOf` answered it.
    mapping(uint64 turn => bytes32) public observed;
    uint64[] public observedTurns;
    bool public changed;
    bool public strangerRecorded;
    bool public lateRecorded;

    constructor(SeedReceiver receiver_) {
        receiver = receiver_;
        vm.warp(receiver.drawDeadline(BASE) - 3 days);
    }

    function relay(uint64 offset, bytes32 seed) external {
        uint64 turn = BASE + uint64(bound(offset, 0, SPAN - 1));
        if (seed == 0) seed = bytes32(uint256(1));
        bool late = block.timestamp >= receiver.drawDeadline(turn);
        bool fresh = receiver.relayedSeed(turn) == 0;
        uint64 epoch = receiver.seedEpoch(turn);
        vm.prank(receiver.relayAlias());
        try receiver.receiveSeed(turn, epoch, seed) {
            if (late) lateRecorded = true;
            if (!fresh) changed = true;
        } catch {}
    }

    function relayAsStranger(uint64 offset, bytes32 seed, address stranger) external {
        if (stranger == receiver.relayAlias()) return;
        uint64 turn = BASE + uint64(bound(offset, 0, SPAN - 1));
        uint64 epoch = receiver.seedEpoch(turn);
        vm.prank(stranger);
        try receiver.receiveSeed(turn, epoch, seed) {
            strangerRecorded = true;
        } catch {}
    }

    function warp(uint32 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 0, 2 days));
    }

    function observe(uint64 offset) external {
        uint64 turn = BASE + uint64(bound(offset, 0, SPAN - 1));
        try receiver.seedOf(turn) returns (bytes32 seed) {
            if (observed[turn] == 0) {
                observed[turn] = seed;
                observedTurns.push(turn);
            } else if (observed[turn] != seed) {
                changed = true;
            }
        } catch {}
    }

    function observedCount() external view returns (uint256) {
        return observedTurns.length;
    }
}

contract SeedReceiverInvariantTest is Test {
    SeedReceiver internal receiver;
    SeedReceiverHandler internal handler;

    function setUp() public {
        receiver = new SeedReceiver(makeAddr("relay"), 1606824023);
        handler = new SeedReceiverHandler(receiver);
        targetContract(address(handler));
    }

    /// Once `seedOf` answers a turn, it answers the same seed for good.
    function invariant_AFinalSeedNeverChanges() public view {
        assertFalse(handler.changed());
        for (uint256 i = 0; i < handler.observedCount(); ++i) {
            uint64 turn = handler.observedTurns(i);
            assertEq(receiver.seedOf(turn), handler.observed(turn));
        }
    }

    function invariant_OnlyTheAliasedRelayRecordsBeforeTheDrawDeadline() public view {
        assertFalse(handler.strangerRecorded());
        assertFalse(handler.lateRecorded());
    }

    function invariant_TheFirstRelayedTurnIsTheEarliestWithASeed() public view {
        uint64 first;
        for (uint64 turn = 20763; turn < 20763 + 12; ++turn) {
            if (receiver.relayedSeed(turn) != 0) {
                first = turn;
                break;
            }
        }
        assertEq(receiver.firstRelayedTurn(), first);
    }
}
