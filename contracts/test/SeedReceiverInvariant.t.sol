// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {MockArbSys} from "./mocks/MockArbSys.sol";

/// Relays seeds for a span of turns, from the relay and from strangers, at random times, fixes
/// fallbacks over Arbitrum blocks, and reads every turn's final seed whenever it may.
contract SeedReceiverHandler is Test {
    uint64 internal constant BASE = 20763;
    uint64 internal constant SPAN = 12;
    MockArbSys internal constant ARB = MockArbSys(address(0x64));

    SeedReceiver internal immutable receiver;
    /// Each turn's seed the first time `seedOf` answered it.
    mapping(uint64 turn => bytes32) public observed;
    uint64[] public observedTurns;
    bool public changed;
    bool public strangerRecorded;
    bool public lateRecorded;
    /// Each turn's fallback block when it was first recorded, and whether a block was recorded before
    /// the turn's draw deadline or recorded again.
    mapping(uint64 turn => uint256) public committed;
    bool public committedEarly;
    bool public recommitted;

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

    /// Moves Arbitrum on, at times past the reach of a pending fallback's block.
    function mine(uint16 blocks) external {
        ARB.setBlockNumber(ARB.arbBlockNumber() + bound(blocks, 1, 300));
    }

    function fix(uint64 offset) external {
        uint64 turn = BASE + uint64(bound(offset, 0, SPAN - 1));
        try receiver.fixFallback(turn) {} catch {}
        uint256 recorded = receiver.fallbackBlock(turn);
        if (recorded == committed[turn]) return;
        if (committed[turn] != 0) recommitted = true;
        if (block.timestamp < receiver.drawDeadline(turn)) committedEarly = true;
        committed[turn] = recorded;
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
        vm.etch(address(0x64), address(new MockArbSys()).code);
        MockArbSys(address(0x64)).setBlockNumber(100_000_000);
        receiver = new SeedReceiver(makeAddr("relay"), 1606824023, 1 days);
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

    /// A fallback folds in the hash of the block its first fix landed in, from the turn's draw deadline
    /// on, and no later fix replaces that block.
    function invariant_AFallbackIsTheHashOfTheBlockFirstFixedFromItsDeadline() public view {
        assertFalse(handler.committedEarly());
        assertFalse(handler.recommitted());
        for (uint64 turn = 20763; turn < 20763 + 12; ++turn) {
            bytes32 blockHash = receiver.fallbackHash(turn);
            if (blockHash == 0) continue;
            assertEq(receiver.relayedSeed(turn), 0, "a relayed turn has no fallback");
            assertEq(blockHash, MockArbSys(address(0x64)).hashOf(receiver.fallbackBlock(turn)));
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
