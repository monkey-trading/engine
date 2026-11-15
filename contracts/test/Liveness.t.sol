// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// The schedule's heartbeat rule and its bounds, witnessed slots, and the count of missed slots that
/// `promote` and `promotionDueAt` read, with block time and the Arbitrum block number moved apart.
contract LivenessRuleTest is LivenessTest {
    function setUp() public {
        _deployLiveness();
    }

    function _rule(uint64 interval, uint64 tolerance, uint16 misses, uint16 blocks)
        internal
        pure
        returns (OperatorSchedule.Liveness memory)
    {
        return OperatorSchedule.Liveness(interval, tolerance, misses, blocks);
    }

    function _build(
        OperatorSchedule.Liveness memory rule,
        OperatorSchedule.Liveness memory lo,
        OperatorSchedule.Liveness memory hi,
        uint64 budget
    ) internal returns (OperatorSchedule) {
        return new OperatorSchedule(registry, receiver, logHeads, rule, lo, hi, budget, BLOCK_MILLIS);
    }

    /// @dev Expects a schedule with `rule` to be refused, and `accepted` to deploy, both under
    /// `lo`, `hi` and `budget`.
    function _refusedThenAccepted(
        OperatorSchedule.Liveness memory rule,
        OperatorSchedule.Liveness memory accepted,
        OperatorSchedule.Liveness memory lo,
        OperatorSchedule.Liveness memory hi,
        uint64 budget
    ) internal {
        vm.expectRevert(OperatorSchedule.InvalidLiveness.selector);
        _build(rule, lo, hi, budget);
        OperatorSchedule built = _build(accepted, lo, hi, budget);
        (uint64 interval, uint64 tolerance, uint16 misses, uint16 blocks) = built.liveness();
        assertEq(interval, accepted.heartbeatInterval);
        assertEq(tolerance, accepted.headTolerance);
        assertEq(misses, accepted.heartbeatMisses);
        assertEq(blocks, accepted.headBlocks);
    }

    function test_EachFieldStaysWithinItsDeploymentBounds() public {
        OperatorSchedule.Liveness memory lo = _rule(30, 5, 3, 10);
        OperatorSchedule.Liveness memory hi = _rule(120, 60, 9, 200);
        // Each refused rule breaks one bound and keeps every invariant.
        _refusedThenAccepted(_rule(29, 5, 5, 10), _rule(30, 5, 5, 10), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(121, 30, 3, 200), _rule(120, 30, 3, 200), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(60, 4, 5, 200), _rule(60, 5, 5, 200), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(120, 61, 3, 200), _rule(120, 60, 3, 200), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(90, 30, 2, 200), _rule(90, 30, 3, 200), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(30, 5, 10, 10), _rule(30, 5, 9, 10), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(60, 30, 5, 9), _rule(60, 30, 5, 10), lo, hi, SWITCH_BUDGET);
        _refusedThenAccepted(_rule(60, 30, 5, 201), _rule(60, 30, 5, 200), lo, hi, SWITCH_BUDGET);
    }

    function test_TheRuleKeepsItsInvariants() public {
        // Bounds wide enough that only the invariant under test refuses.
        OperatorSchedule.Liveness memory lo = _rule(1, 0, 1, 1);
        OperatorSchedule.Liveness memory hi = _rule(600, 600, 100, 300);
        // At least two misses.
        _refusedThenAccepted(_rule(60, 30, 1, 200), _rule(60, 30, 2, 200), lo, hi, 0);
        // Detection, (misses + 1) * interval + tolerance, within ten minutes.
        _refusedThenAccepted(_rule(60, 1, 9, 200), _rule(60, 0, 9, 200), lo, hi, 0);
        // The tolerance under one interval.
        _refusedThenAccepted(_rule(60, 60, 5, 200), _rule(60, 59, 5, 200), lo, hi, 0);
        // The held-head window within `arbBlockHash`'s 256 blocks.
        _refusedThenAccepted(_rule(120, 30, 2, 257), _rule(120, 30, 2, 256), lo, hi, 0);
        // The held-head window under one interval at the pinned block time: 240 blocks of 250 ms is 60 s.
        _refusedThenAccepted(_rule(60, 30, 5, 240), _rule(60, 30, 5, 239), lo, hi, 0);
        // The miss window above the switch budget: 2 * 60 s is not above 120 s.
        _refusedThenAccepted(_rule(60, 30, 2, 200), _rule(61, 30, 2, 200), lo, hi, SWITCH_BUDGET);
    }

    function test_TheTimelockChangesTheRuleWhileNoTurnNamesTheOperator() public {
        OperatorSchedule.Liveness memory slower = _rule(90, 30, 4, 200);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.LivenessChanged(slower);
        vm.prank(address(governance));
        schedule.setLiveness(slower);
        (uint64 interval, uint64 tolerance, uint16 misses, uint16 blocks) = schedule.liveness();
        assertEq(interval, 90);
        assertEq(tolerance, 30);
        assertEq(misses, 4);
        assertEq(blocks, 200);

        // A rule breaking an invariant is refused: the tolerance is not under the interval.
        vm.expectRevert(OperatorSchedule.InvalidLiveness.selector);
        vm.prank(address(governance));
        schedule.setLiveness(_rule(60, 60, 5, 200));
    }

    function test_OnlyTheTimelockChangesTheRule() public {
        vm.expectRevert(OperatorSchedule.UnauthorizedGovernance.selector);
        schedule.setLiveness(_liveness());
        vm.expectRevert(OperatorSchedule.UnauthorizedGovernance.selector);
        vm.prank(address(registry));
        schedule.setLiveness(_liveness());
        vm.expectRevert(OperatorSchedule.UnauthorizedGovernance.selector);
        vm.prank(multisig);
        schedule.setLiveness(_liveness());
    }

    function test_TheRuleCannotChangeWhileATurnNamesTheOperator() public {
        _startTurn();
        vm.expectRevert(OperatorSchedule.TurnNamesTheOperator.selector);
        vm.prank(address(governance));
        schedule.setLiveness(_rule(90, 30, 4, 200));
        // Turned off, the drawn turns still name the operator until an installation past them.
        _setRotation(false);
        vm.expectRevert(OperatorSchedule.TurnNamesTheOperator.selector);
        vm.prank(address(governance));
        schedule.setLiveness(_rule(90, 30, 4, 200));
    }

    function test_NoSlotIsWitnessedWhileNoTurnNamesTheOperator() public {
        _startTurn();
        vm.warp(schedule.boundary(turn) - 1);
        vm.expectRevert(OperatorSchedule.NoTurn.selector);
        schedule.witness();
        assertEq(schedule.promotionDueAt(), type(uint64).max);
        vm.warp(schedule.boundary(turn));
        assertTrue(schedule.witness());
    }

    function test_AWitnessMarksTheCurrentSlotOnceWithItsArbitrumBlock() public {
        _startTurn();
        vm.warp(block.timestamp + 17);
        // `block.number` and the Arbitrum block number move apart; the mark records Arbitrum's.
        vm.roll(42);
        ARB.setBlockNumber(250_000_000);
        uint64 slot = _slot();
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.SlotWitnessed(slot, 250_000_000);
        assertTrue(schedule.witness());
        (uint64 at, uint64 blockNumber) = schedule.marks(slot);
        assertEq(at, block.timestamp);
        assertEq(blockNumber, 250_000_000);
        (uint64 latest, uint64 latestBlock) = schedule.latestMark();
        assertEq(latest, slot);
        assertEq(latestBlock, 250_000_000);
        // Neither slot beside it is marked.
        (at,) = schedule.marks(slot - 1);
        assertEq(at, 0);
        (at,) = schedule.marks(slot + 1);
        assertEq(at, 0);
    }

    function test_DuplicateWitnessesChangeNothing() public {
        _startTurn();
        vm.warp(block.timestamp + 5);
        assertTrue(schedule.witness());
        (uint64 at, uint64 blockNumber) = schedule.marks(_slot());
        vm.recordLogs();
        for (uint256 i = 0; i < 5; ++i) {
            vm.warp(block.timestamp + 10);
            ARB.setBlockNumber(ARB.arbBlockNumber() + 40);
            vm.prank(makeAddr(string(abi.encode("witness", i))));
            assertFalse(schedule.witness());
        }
        assertEq(vm.getRecordedLogs().length, 0);
        (uint64 again, uint64 againBlock) = schedule.marks(_slot());
        assertEq(again, at);
        assertEq(againBlock, blockNumber);
    }

    /// A head and a witness in one slot, in either order: the slot is the head's, so its mark does not
    /// count, and the count passes `MISSES` slots after it, not before.
    function test_AWitnessAndAHeadInOneSlotInBothOrders() public {
        _startTurn();
        _nextSlot(10, 240);
        uint256 snapshot = vm.snapshotState();
        for (uint256 order = 0; order < 2; ++order) {
            vm.revertToState(snapshot);
            if (order == 0) {
                _beat();
                schedule.witness();
            } else {
                schedule.witness();
                _beat();
            }
            uint64 headSlot = _slot();
            assertEq(schedule.promotionDueAt(), (headSlot + MISSES + 1) * INTERVAL);
            _nextSlot(10, 240);
            _witnessFromNow(MISSES);
            vm.warp((headSlot + MISSES + 1) * INTERVAL - 1);
            vm.expectRevert(
                abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, (headSlot + MISSES + 1) * INTERVAL)
            );
            schedule.promote();
            vm.warp((headSlot + MISSES + 1) * INTERVAL);
            schedule.promote();
        }
    }

    function test_AHeadBeforeAPromotionInTheSameBlockStopsIt() public {
        _startTurn();
        _lapse();
        assertLe(schedule.promotionDueAt(), block.timestamp);
        _beat();
        uint64 dueAt = (_slot() + MISSES + 1) * INTERVAL;
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
    }

    function test_AHeadAfterAPromotionInTheSameBlockFailsAsTheOldEpochs() public {
        _startTurn();
        _lapse();
        LogHeads.HeadV2 memory late = _nextHead();
        bytes memory signature = _sign(late, _signerKey());
        schedule.promote();
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        logHeads.postV2(late, signature);
    }

    function test_ATimestampJumpMarksOnlyTheSlotItLandsIn() public {
        _startTurn();
        _witnessFromNow(2);
        // Ten slots pass in one block-time jump; only the slot it lands in can be witnessed.
        vm.warp(block.timestamp + 10 * INTERVAL);
        ARB.setBlockNumber(ARB.arbBlockNumber() + 1);
        schedule.witness();
        uint64 landed = _slot();
        (uint64 at,) = schedule.marks(landed - 1);
        assertEq(at, 0);
        // The skipped slots restart the count: it passes `MISSES` marked slots after the last one.
        assertEq(schedule.promotionDueAt(), (landed - 1 + MISSES + 1) * INTERVAL);
        _nextSlot(10, 240);
        _witnessFromNow(MISSES - 1);
        _nextSlot(5, 240);
        assertEq(_slot(), landed + MISSES);
        assertLe(schedule.promotionDueAt(), block.timestamp, "due: at or before now");
        schedule.promote();
    }

    function test_AMissingWitnessRestartsTheCount() public {
        _startTurn();
        _witnessFromNow(2);
        _nextSlot(10, 240);
        uint64 missed = _slot();
        _nextSlot(10, 240);
        _witnessFromNow(MISSES - 1);
        _nextSlot(5, 240);
        uint64 dueAt = (missed + MISSES + 1) * INTERVAL;
        assertEq(schedule.promotionDueAt(), dueAt);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
        schedule.witness();
        _nextSlot(5, 240);
        assertEq(block.timestamp, dueAt + 5);
        schedule.promote();
    }

    /// The chain stops for three slots, so no one can witness them, then resumes: the count restarts
    /// after the outage instead of carrying the slots before it.
    function test_AnOutageThatResumesRestartsTheCount() public {
        _startTurn();
        _beat();
        _nextSlot(10, 240);
        _witnessFromNow(MISSES - 1);
        // Blocks stop: three slots pass with no block, then one block lands.
        vm.warp(block.timestamp + 3 * INTERVAL);
        ARB.setBlockNumber(ARB.arbBlockNumber() + 1);
        uint64 resumed = _slot();
        schedule.witness();
        assertEq(schedule.promotionDueAt(), resumed * INTERVAL + MISSES * INTERVAL);
        _nextSlot(10, 240);
        _witnessFromNow(MISSES - 1);
        _nextSlot(5, 240);
        assertLe(schedule.promotionDueAt(), block.timestamp, "due: at or before now");
        schedule.promote();
    }

    function test_ASecondPromotionInTheSameOrTheNextSlotReverts() public {
        _startTurn();
        _lapse();
        schedule.promote();
        uint64 dueAt = (_slot() + MISSES + 1) * INTERVAL;
        assertEq(schedule.promotionDueAt(), dueAt);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
        schedule.witness();
        _nextSlot(5, 240);
        schedule.witness();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
    }

    /// The count reads block time only: rolling `block.number` and moving the Arbitrum block number
    /// apart change neither when a promotion is due nor whether it passes.
    function test_BlockTimeAloneDrivesTheCount() public {
        _startTurn();
        _beat();
        uint64 dueAt = schedule.promotionDueAt();
        for (uint256 i = 0; i < MISSES; ++i) {
            _nextSlot(1, i % 2 == 0 ? 1 : 10_000);
            vm.roll(i % 2 == 0 ? 7 : 7_000_000);
            schedule.witness();
            assertEq(schedule.promotionDueAt(), dueAt);
        }
        vm.warp(dueAt - 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
        vm.warp(dueAt);
        vm.roll(1);
        schedule.promote();
    }

    /// Marks made under a 60-second interval sit at slot numbers a 61-second interval reaches about
    /// 340 days later. After the change they must not count as that interval's slots.
    function test_AMarkUnderAnOldIntervalDoesNotCountAfterAChange() public {
        _startTurn();
        _nextSlot(10, 240);
        uint64 first = _slot();
        _witnessFromNow(MISSES);

        // The turns end with an installation; then the timelock lengthens the interval.
        _setRotation(false);
        assertGe(block.timestamp, schedule.turnsRunUntil());
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (deployerKey, deployerKey, _digest(0), CONFIG))
        );
        assertEq(registry.operatorGeneration(), 0);
        _govern(address(schedule), abi.encodeCall(OperatorSchedule.setLiveness, (_rule(61, 30, 5, 200))));

        // Rotation resumes in time for a turn that names the operator when the 61-second slot
        // `first + MISSES` starts, so the five marks sit at the five slot numbers before it.
        uint64 target = (first + MISSES) * 61 + 1;
        vm.warp(target - 12 days);
        _setRotation(true);
        _closeNext();
        uint64 later = uint64((target - schedule.BOUNDARY_OFFSET()) / 1 days);
        _draw(later);
        vm.warp(target);
        assertTrue(registry.operatorGeneration() != 0);
        for (uint64 slot = first; slot < first + MISSES; ++slot) {
            (uint64 at,) = schedule.marks(slot);
            assertTrue(at != 0, "the old marks are still stored");
        }
        uint64 dueAt = (first + MISSES - 1 + MISSES + 1) * 61;
        assertEq(schedule.promotionDueAt(), dueAt);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
    }
}
