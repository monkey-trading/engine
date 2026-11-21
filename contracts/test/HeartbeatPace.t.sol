// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// The operator's heads at the node's pace, against the schedule's count: a head every half interval,
/// rounded up to the second, and each landing up to just under the head tolerance after it was signed,
/// with every slot witnessed. The first comes anywhere in the half interval after the boundary, as a
/// member that also held the turn before posts it, or, if later, within the poster's 30 s turn read and
/// a read tick, a sixth of an interval, as the first turn after none named the operator finds it. With
/// any one head failing, or one a read tick late, with the rest following it, at every second of that
/// phase, no promotion or skip comes due. Once a head of the signer epoch has landed, the promotion also stays further off than the
/// operator's stop margin. Before one has, as at the first turn after rotation turns on, the boundary's
/// slot and the one before it start the count, and under the tightest rule a failed first head can bring
/// the operator's stop, though not the promotion, before its next head lands.
abstract contract HeartbeatPaceTest is LivenessTest {
    /// The operator's node stops signing this long before a promotion could pass (`liveness::DELTA`).
    uint64 internal constant STOP_MARGIN = 15;
    uint256 internal constant NONE = type(uint256).max;
    /// How often the poster reads whether a turn names the operator while none does (`TURN_READ_EVERY`).
    uint64 internal constant TURN_READ = 30;

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

    function test_no_single_failed_or_late_head_brings_a_promotion_or_skip_due() public {
        vm.pauseGasMetering();
        (uint64 interval, uint64 tolerance,) = _rule();
        uint64[2] memory delays = [uint64(0), tolerance - 1];
        for (uint256 d = 0; d < 2; ++d) {
            for (uint64 offset = 0; offset < _firstWithin(interval); ++offset) {
                // The first head, which latches the turn, then two steady ones.
                for (uint256 j = 0; j < 3; ++j) {
                    uint256 snapshot = vm.snapshotState();
                    _beatAt(delays[d], offset, j, 0, NONE);
                    vm.revertToState(snapshot);
                    snapshot = vm.snapshotState();
                    _beatAt(delays[d], offset, NONE, 0, j);
                    vm.revertToState(snapshot);
                }
            }
        }
    }

    /// Without heads for the misses the rule counts, the same run brings the promotion due: the checks
    /// above can see one.
    function test_failed_heads_spanning_the_misses_bring_the_promotion_due() public {
        vm.pauseGasMetering();
        (,, uint16 misses) = _rule();
        vm.expectRevert(bytes("promotion due"));
        this.beatAt(0, 0, 1, 2 * uint256(misses) + 1, NONE);
    }

    /// With only heads that latch nothing landing, as the incumbent's while the turn's member never takes
    /// it, the same run brings the skip due: the checks above can see one.
    function test_heads_that_latch_nothing_bring_the_skip_due() public {
        vm.pauseGasMetering();
        bindsOther = true;
        vm.expectRevert(bytes("skip due"));
        this.beatAt(0, 0, NONE, 0, NONE);
    }

    /// `_beatAt`, called externally so an expected revert is caught at this call.
    function beatAt(uint64 delay, uint64 offset, uint256 dropped, uint256 alsoDropped, uint256 late) external {
        _beatAt(delay, offset, dropped, alsoDropped, late);
    }

    /// Signs a head every half interval from `offset` after the boundary, each landing `delay` after it was
    /// signed: the `dropped`th fails, with the `alsoDropped` after it, and from the `late`th each is signed
    /// a read tick late. Witnesses each slot a second in, through the misses plus two intervals past the
    /// last failure. Before each landing, requires no skip or promotion due, and once a head has landed, the
    /// promotion further off than the stop margin.
    function _beatAt(uint64 delay, uint64 offset, uint256 dropped, uint256 alsoDropped, uint256 late) internal {
        (uint64 interval,, uint16 misses) = _rule();
        uint64 gap = _gap(interval);
        uint64 tick = (interval + 5) / 6;
        nextWitness = start / interval + 1;
        uint256 attempts = 4 + alsoDropped + (uint256(misses) + 2) * interval / gap;
        bool landed;
        for (uint256 j = 0; j < attempts; ++j) {
            uint64 signAt = start + offset + uint64(j) * gap + (j >= late ? tick : 0);
            uint64 landAt = signAt + delay;
            _witnessThrough(landAt, interval);
            vm.warp(landAt);
            uint64 due = schedule.promotionDueAt();
            require(due > landAt && (!landed || due > landAt + STOP_MARGIN), "promotion due");
            require(schedule.skipDueAt() > landAt, "skip due");
            if (j >= dropped && j - dropped <= alsoDropped) continue;
            landed = true;
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

    /// Half an interval, rounded up to the second.
    function _gap(uint64 interval) internal pure returns (uint64) {
        return (interval + 1) / 2;
    }

    /// How long after the boundary the first head may come: half an interval, or the poster's 30 s turn
    /// read and a read tick when that is later.
    function _firstWithin(uint64 interval) internal pure virtual returns (uint64) {
        uint64 found = TURN_READ + (interval + 5) / 6;
        return found > _gap(interval) ? found : _gap(interval);
    }

    /// The heartbeat rule the schedule was deployed with: interval, tolerance and misses.
    function _rule() internal view returns (uint64 interval, uint64 tolerance, uint16 misses) {
        (interval, tolerance, misses,) = schedule.liveness();
    }

    /// A schedule with `rule` as its value and both its bounds, a two-slot take window and `budget`.
    function _built(OperatorSchedule.Liveness memory rule, uint64 budget) internal returns (OperatorSchedule) {
        OperatorSchedule.TakeWindow memory window = OperatorSchedule.TakeWindow(2, 2, 2);
        return
            new OperatorSchedule(
                registry, receiver, new LogHeads(vault), rule, rule, rule, window, budget, BLOCK_MILLIS
            );
    }
}

/// The forge default rule: 60 s slots, 30 s tolerance, five misses.
contract HeartbeatPaceDefaultTest is HeartbeatPaceTest {}

/// Three misses, the fewest a rule with a take window can have: 140 s slots and 40 s tolerance, under the
/// 20 s test switch budget.
contract HeartbeatPaceThreeMissesTest is HeartbeatPaceTest {
    function _newSchedule() internal override returns (OperatorSchedule) {
        return _built(OperatorSchedule.Liveness(140, 40, 3, 200), SWITCH_BUDGET);
    }
}

/// The tightest rule the schedule accepts at 49 s slots: three misses, the longest tolerance and no
/// switch budget, so the take window spans the least time it can. The turn's boundary falls on its slot's
/// last second, which leaves the least time before a skip. Its first head comes within half an interval,
/// as a member continuing into the turn posts it: at the first turn after none named the operator, a
/// first head found 30 s and a read tick after the boundary that then fails leaves its member a take only
/// through the members' relays, under any interval shorter than 90 s.
contract HeartbeatPaceTightestTest is HeartbeatPaceTest {
    function _newSchedule() internal override returns (OperatorSchedule) {
        return _built(OperatorSchedule.Liveness(49, 48, 3, 10), 0);
    }

    function _firstWithin(uint64 interval) internal pure override returns (uint64) {
        return _gap(interval);
    }

    function test_the_boundary_falls_on_its_slots_last_second() public view {
        assertEq(start % 49, 48);
    }
}
