// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// The operator's heads at the node's pace, against the schedule's count: a head every half interval, or
/// every two thirds of one after a slow chain read skips the node's read tick, the first from anywhere in
/// the interval after the boundary and each landing up to just under the head tolerance after it was
/// signed, with every slot witnessed. With any one head failing, at every phase of a slot to the second,
/// no promotion or skip comes due, and the promotion stays further off than the operator's stop margin.
abstract contract HeartbeatPaceTest is LivenessTest {
    /// The operator's node stops signing this long before a promotion could pass (`liveness::DELTA`).
    uint64 internal constant STOP_MARGIN = 15;

    uint64 internal start;
    uint64 internal nextWitness;
    /// Whether each head binds another position of the turn, as the incumbent's heads bind its own earlier
    /// assignment: it counts as a heartbeat but latches nothing.
    bool internal bindsOther;

    function setUp() public {
        _deployLiveness();
        _startTurn();
        start = uint64(block.timestamp);
        schedule.witness();
    }

    /// The heartbeat rule the schedule was deployed with: interval, tolerance and misses.
    function _rule() internal view returns (uint64 interval, uint64 tolerance, uint16 misses) {
        (interval, tolerance, misses,) = schedule.liveness();
    }

    function test_no_single_failed_head_brings_a_promotion_or_skip_due() public {
        vm.pauseGasMetering();
        (uint64 interval, uint64 tolerance,) = _rule();
        uint64[2] memory gaps = [interval / 2, interval * 2 / 3];
        uint64[2] memory delays = [uint64(0), tolerance - 1];
        for (uint256 g = 0; g < 2; ++g) {
            for (uint256 d = 0; d < 2; ++d) {
                for (uint64 offset = 0; offset < interval; ++offset) {
                    // The first head, which latches the turn, then two steady ones.
                    for (uint256 dropped = 0; dropped < 3; ++dropped) {
                        uint256 snapshot = vm.snapshotState();
                        _beatAt(gaps[g], delays[d], offset, dropped, 1);
                        vm.revertToState(snapshot);
                    }
                }
            }
        }
    }

    /// Without heads for the misses the rule counts, the same run brings the promotion due: the checks
    /// above can see one.
    function test_failed_heads_spanning_the_misses_bring_the_promotion_due() public {
        vm.pauseGasMetering();
        (uint64 interval,, uint16 misses) = _rule();
        vm.expectRevert(bytes("promotion due"));
        this.beatAt(interval / 2, 0, 0, 1, 2 * uint256(misses) + 2);
    }

    /// With only heads that latch nothing landing, as the incumbent's while the turn's member never takes
    /// it, the same run brings the skip due: the checks above can see one.
    function test_heads_that_latch_nothing_bring_the_skip_due() public {
        vm.pauseGasMetering();
        bindsOther = true;
        (uint64 interval,,) = _rule();
        vm.expectRevert(bytes("skip due"));
        this.beatAt(interval / 2, 0, 0, 0, 0);
    }

    /// `_beatAt`, called externally so an expected revert is caught at this call.
    function beatAt(uint64 gap, uint64 delay, uint64 offset, uint256 dropFrom, uint256 dropCount) external {
        _beatAt(gap, delay, offset, dropFrom, dropCount);
    }

    /// Signs a head every `gap` seconds from `offset` after the boundary, each landing `delay` after it was
    /// signed, with `dropCount` from the `dropFrom`th failing, witnessing each slot a second in, through the
    /// misses plus two intervals past the last failure. Before each landing, requires no skip due and the
    /// promotion further off than the stop margin.
    function _beatAt(uint64 gap, uint64 delay, uint64 offset, uint256 dropFrom, uint256 dropCount) internal {
        (uint64 interval,, uint16 misses) = _rule();
        nextWitness = start / interval + 1;
        uint256 attempts = dropFrom + dropCount + (uint256(misses) + 2) * interval / gap + 1;
        for (uint256 j = 0; j < attempts; ++j) {
            uint64 signAt = start + offset + uint64(j) * gap;
            uint64 landAt = signAt + delay;
            _witnessThrough(landAt, interval);
            vm.warp(landAt);
            require(schedule.promotionDueAt() > landAt + STOP_MARGIN, "promotion due");
            require(schedule.skipDueAt() > landAt, "skip due");
            if (j >= dropFrom && j < dropFrom + dropCount) continue;
            LogHeads.HeadV2 memory head = _nextHead();
            (head.stamp, head.signedAt) = (signAt * 1000, signAt * 1000);
            if (bindsOther) ++head.position;
            _post(head);
        }
    }

    /// Witnesses every slot that starts at or before `until`, a second into each.
    function _witnessThrough(uint64 until, uint64 interval) internal {
        while (nextWitness * interval + 1 <= until) {
            vm.warp(nextWitness * interval + 1);
            ARB.setBlockNumber(ARB.arbBlockNumber() + 100);
            schedule.witness();
            ++nextWitness;
        }
    }
}

/// The forge default rule: 60 s slots, 30 s tolerance, five misses.
contract HeartbeatPaceDefaultTest is HeartbeatPaceTest {}

/// Three misses, the fewest a rule with a take window can have: 140 s slots, 40 s tolerance, a two-slot
/// window, under the 20 s test switch budget.
contract HeartbeatPaceThreeMissesTest is HeartbeatPaceTest {
    function _newSchedule() internal override returns (OperatorSchedule) {
        return new OperatorSchedule(
            registry,
            receiver,
            new LogHeads(vault),
            OperatorSchedule.Liveness(140, 40, 3, 200),
            OperatorSchedule.Liveness(90, 5, 3, 10),
            OperatorSchedule.Liveness(140, 60, 3, 256),
            OperatorSchedule.TakeWindow(2, 2, 4),
            SWITCH_BUDGET,
            BLOCK_MILLIS
        );
    }
}

/// The shortest slots with the largest share of tolerance the liveness scenario runs: 30 s slots, 20 s
/// tolerance, three misses and a two-slot window, under a 5 s switch budget.
contract HeartbeatPaceShortSlotsTest is HeartbeatPaceTest {
    function _newSchedule() internal override returns (OperatorSchedule) {
        return new OperatorSchedule(
            registry,
            receiver,
            new LogHeads(vault),
            OperatorSchedule.Liveness(30, 20, 3, 10),
            OperatorSchedule.Liveness(30, 5, 3, 10),
            OperatorSchedule.Liveness(30, 20, 3, 10),
            OperatorSchedule.TakeWindow(2, 2, 4),
            5,
            BLOCK_MILLIS
        );
    }
}
