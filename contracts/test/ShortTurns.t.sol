// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";

/// The schedule on five-minute turns, as a development stack deploys it: a turn starts every five
/// minutes and names its member for five, and the take window spans at most a turn.
contract ShortTurnsTest is ScheduleTest {
    /// @dev A switch budget and heartbeat rule that fit a five-minute turn: test values only.
    uint64 internal constant BUDGET = 30;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");

    function setUp() public {
        _deploySchedule();
    }

    function _turnLength() internal pure override returns (uint64) {
        return 5 minutes;
    }

    function _newSchedule() internal override returns (OperatorSchedule) {
        return _scheduleWith(receiver, new LogHeads(vault), _fitting(_shortMinimums(), _shortMaximums(), BUDGET));
    }

    function test_TheTurnViewsFollowFiveMinuteBoundaries() public {
        uint32 setId = _rotateTwo(alice, bob);
        uint64 turn = _nextTurn() + 1;
        _relayPicking(turn, setId, alice);
        _relayPicking(turn + 1, setId, bob);
        _draw(turn);
        uint64 since = schedule.boundary(turn);
        assertEq(since % 5 minutes, 0, "a boundary every five minutes");
        assertEq(schedule.drawDeadline(turn), since - 5 minutes);

        vm.warp(since);
        (address candidate,, uint64 from, uint64 until) = schedule.namedAt(since);
        assertEq(candidate, alice);
        assertEq(from, since);
        assertEq(until, since + 5 minutes);
        (,,, uint64 stateUntil) = registry.rotationState();
        assertEq(stateUntil, since + 5 minutes, "the vault reads the turn's end a turn on");
        assertEq(schedule.turnsRunUntil(), since + 5 minutes);

        _draw(turn + 1);
        assertEq(schedule.turnsRunUntil(), since + 10 minutes);
        (uint64 started,) = schedule.turnAt(since + 5 minutes - 1);
        assertEq(started, turn);
        (started,) = schedule.turnAt(since + 5 minutes);
        assertEq(started, turn + 1);
        vm.warp(since + 5 minutes);
        (candidate,, from, until) = schedule.namedAt(uint64(block.timestamp));
        assertEq(candidate, bob);
        assertEq(from, since + 5 minutes);
        assertEq(until, since + 10 minutes);
        assertEq(registry.operatorInEffect().candidate, bob);
        assertTrue(registry.holdsTurn(alice), "bob takes the keys from alice");
        assertTrue(registry.holdsTurn(bob));
    }

    function test_RevertWhen_TheTakeWindowSpansMoreThanATurnAtTheShortestInterval() public {
        // Sixteen 20 s slots are 320 s: more than a five-minute turn, well within a day.
        OperatorSchedule.TakeWindow memory window = OperatorSchedule.TakeWindow(4, 4, 16);
        LogHeads heads = new LogHeads(vault);
        try this.scheduleWith(receiver, heads, window) {
            fail("a window of 320 s built a schedule of five-minute turns");
        } catch (bytes memory reason) {
            assertEq(bytes4(reason), OperatorSchedule.InvalidTakeWindow.selector);
        }
        SeedReceiver daily = new SeedReceiver(relay, GENESIS, 1 days);
        assertEq(_scheduleWith(daily, heads, window).turnLength(), 1 days);
        window.maximum = 15;
        assertEq(_scheduleWith(receiver, heads, window).turnLength(), 5 minutes, "300 s is a turn");
    }

    /// @dev `_scheduleWith` as an external call, so a test can catch its revert.
    function scheduleWith(SeedReceiver receiver_, LogHeads heads, OperatorSchedule.TakeWindow memory window)
        external
        returns (OperatorSchedule)
    {
        return _scheduleWith(receiver_, heads, window);
    }

    function _scheduleWith(SeedReceiver receiver_, LogHeads heads, OperatorSchedule.TakeWindow memory window)
        internal
        returns (OperatorSchedule)
    {
        return new OperatorSchedule(
            registry,
            receiver_,
            heads,
            OperatorSchedule.Liveness(20, 5, 3, 40),
            _shortMinimums(),
            _shortMaximums(),
            window,
            BUDGET,
            BLOCK_MILLIS
        );
    }

    function _shortMinimums() internal pure returns (OperatorSchedule.Liveness memory) {
        return OperatorSchedule.Liveness(20, 5, 2, 10);
    }

    function _shortMaximums() internal pure returns (OperatorSchedule.Liveness memory) {
        return OperatorSchedule.Liveness(30, 10, 5, 40);
    }
}
