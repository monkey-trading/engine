// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {console} from "forge-std/Test.sol";

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// A schedule whose take window is two slots: bounds of 60 to 120 s slots and at most 30 s of
/// tolerance, under a 10 s switch budget, so a window of two 60 s slots exceeds the budget, one
/// interval and a head's inclusion.
abstract contract SkipTest is LivenessTest {
    uint16 internal constant TAKE = 2;
    uint64 internal constant BUDGET = 10;

    function _newSchedule() internal override returns (OperatorSchedule) {
        return new OperatorSchedule(
            registry,
            receiver,
            new LogHeads(vault),
            _liveness(),
            OperatorSchedule.Liveness(60, 5, 2, 10),
            OperatorSchedule.Liveness(120, 30, 9, 240),
            OperatorSchedule.TakeWindow(TAKE, 2, 8),
            BUDGET,
            BLOCK_MILLIS
        );
    }

    /// @dev Adopts the four members and starts `turn` with them in the order alice, bob, carol, dave.
    function _startInOrder() internal {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (alice, bob, carol, dave);
        _startTurn(order);
    }

    /// @dev Adopts the four members, starts `turn` for alice, draws `turn + 1` while alice is named, so
    /// alice is its source, with its first positions picking `order`, and warps to its boundary.
    function _startSourced(address[] memory order) internal {
        address[] memory members = new address[](4);
        uint256[] memory stakes = new uint256[](4);
        (members[0], members[1], members[2], members[3]) = (alice, bob, carol, dave);
        (stakes[0], stakes[1], stakes[2], stakes[3]) = (40_000e18, 30_000e18, 20_000e18, 10_000e18);
        setId = _rotate(members, stakes);
        turn = _nextTurn() + 1;
        address[] memory first = new address[](1);
        first[0] = alice;
        seed = _relayOrder(turn, first);
        bytes32 next = _relayOrder(turn + 1, order);
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), alice);
        (turn, seed) = (turn + 1, next);
        vm.warp(schedule.boundary(turn));
    }

    /// @dev Witnesses `count` slots in a row from the next one and moves five seconds into the slot
    /// after them.
    function _marked(uint256 count) internal {
        _nextSlot(10, 240);
        _witnessFromNow(count);
        _nextSlot(5, 240);
    }

    /// @dev The slot `turn`'s boundary falls in.
    function _boundarySlot() internal view returns (uint64) {
        return schedule.boundary(turn) / INTERVAL;
    }

    /// @dev A live head that binds no assignment of the turn in effect, as the incumbent's do while it
    /// still holds the keys of an earlier turn.
    function _incumbentHead() internal view returns (LogHeads.HeadV2 memory head) {
        head = _nextHead();
        (head.turn, head.position, head.release, head.generation) = (turn - 1, 0, keccak256("earlier"), 1);
    }
}

/// The schedule's `skip`: a turn whose current assignment no head latches within the take window
/// moves to the next listed member in its order, in a new generation, with no signer change; and the
/// take window's bounds.
contract SkipScheduleTest is SkipTest {
    function setUp() public {
        _deployLiveness();
    }

    function test_ASkipNamesTheNextListedMemberInANewGenerationWithNoSignerChange() public {
        _startInOrder();
        OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
        assertEq(schedule.skipDueAt(), (_boundarySlot() + TAKE + 1) * INTERVAL);
        _marked(TAKE);
        uint64 generation = vault.generationCounter() + 1;
        bytes32 release = registry.candidateOf(bob).release;
        address enclave = vault.checkpointSigner();

        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnAssigned(turn, setId, 1, 1, bob, _key(bob), release, generation);
        (uint64 skipped, uint16 position) = schedule.skip();
        assertEq(skipped, turn);
        assertEq(position, 1);

        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        assertEq(current.candidate, bob);
        assertEq(current.position, 1);
        assertEq(current.reason, 1);
        assertEq(current.generation, generation);
        assertEq(current.setId, drawn.setId);
        assertEq(current.seed, drawn.seed);
        assertEq(current.source, drawn.source, "the draw's source stays");
        (OperatorSchedule.Assignment memory assignment, OperatorSchedule.Revision[] memory revisions) =
            schedule.assignmentOf(turn, 1);
        assertEq(assignment.candidate, bob);
        assertEq(assignment.adminKey, _key(bob));
        assertEq(assignment.setId, setId);
        assertEq(assignment.reason, 1);
        assertEq(assignment.standbyKey, address(0));
        assertEq(assignment.signerEpoch, 0);
        assertEq(assignment.at, block.timestamp);
        assertEq(revisions.length, 1);
        assertEq(revisions[0].release, release);
        assertEq(revisions[0].generation, generation);
        (OperatorSchedule.Assignment memory first,) = schedule.assignmentOf(turn, 0);
        assertEq(first.candidate, alice, "the drawn assignment stays");
        assertTrue(schedule.heldTurn(turn, alice));
        assertTrue(schedule.heldTurn(turn, bob));

        // The keys move by a handover: the signer, its epoch and the members' standby keys stay.
        assertEq(vault.checkpointSigner(), enclave);
        assertEq(vault.signerEpoch(), 1);
        assertTrue(schedule.promotable(bob) != address(0));
        assertEq(registry.namedOperator().candidate, bob);
        assertEq(registry.operatorGeneration(), generation);
        assertEq(vault.releaseGeneration(), generation);
        assertEq(vault.currentRelease(), release);
        // The next window counts from the skip's own slot.
        assertEq(schedule.skipDueAt(), (_slot() + TAKE + 1) * INTERVAL);
    }

    function test_SkipsWalkTheOrderToExhaustion() public {
        _startInOrder();
        address[3] memory next = [bob, carol, dave];
        for (uint256 i = 0; i < next.length; ++i) {
            _marked(TAKE);
            (, uint16 position) = schedule.skip();
            assertEq(position, i + 1);
            assertEq(schedule.turnOf(turn).candidate, next[i]);
        }
        _marked(TAKE);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoQualifiedPosition.selector, turn));
        schedule.skip();
    }

    function test_ASkipWaitsForTheTakeWindowAfterTheBoundary() public {
        _startInOrder();
        uint64 dueAt = (_boundarySlot() + TAKE + 1) * INTERVAL;
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, dueAt));
        schedule.skip();
        _nextSlot(10, 240);
        _witnessFromNow(TAKE);
        // The last slot of the window is not complete yet.
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, dueAt));
        schedule.skip();
        _nextSlot(0, 240);
        assertEq(block.timestamp, dueAt);
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, bob);
    }

    function test_AnUnmarkedSlotRestartsTheTakeWindow() public {
        _startInOrder();
        _nextSlot(10, 240);
        schedule.witness();
        // A slot goes unmarked, as in a chain outage: the window restarts after it.
        _nextSlot(10, 240);
        _nextSlot(10, 240);
        schedule.witness();
        uint64 dueAt = (_slot() - 1 + TAKE + 1) * INTERVAL;
        _nextSlot(10, 240);
        schedule.witness();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, dueAt));
        schedule.skip();
        _nextSlot(0, 240);
        schedule.skip();
    }

    function test_ABoundaryDuringChainDowntimeGivesTheMemberAFreshWindow() public {
        _startInOrder();
        // No block from before the boundary until ten minutes past it: the first resumed block marks
        // its slot, and no skip or promotion lands then.
        vm.warp(schedule.boundary(turn) + 600);
        ARB.setBlockNumber(ARB.arbBlockNumber() + 2_400);
        schedule.witness();
        uint64 resumed = _slot();
        vm.expectRevert(
            abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, (resumed - 1 + TAKE + 1) * INTERVAL)
        );
        schedule.skip();
        vm.expectRevert(
            abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, (resumed - 1 + MISSES + 1) * INTERVAL)
        );
        schedule.promote();
        // The member takes the turn in the resumed window: its first head latches the assignment.
        vm.warp(block.timestamp + 20);
        _beat();
        _marked(TAKE);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, type(uint64).max));
        schedule.skip();
        assertEq(schedule.skipDueAt(), type(uint64).max);
        (uint64 at,, address member,,) = logHeads.latches(turn, 0);
        assertGt(at, 0);
        assertEq(member, alice);
    }

    function test_AHeadBeforeASkipInTheSameBlockStopsIt() public {
        _startInOrder();
        _marked(TAKE);
        _beat();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, type(uint64).max));
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, alice);
    }

    function test_AHeadAfterASkipInTheSameBlockLatchesNothing() public {
        _startInOrder();
        _marked(TAKE);
        LogHeads.HeadV2 memory head = _nextHead();
        schedule.skip();
        _post(head);
        (uint64 seq,,,,) = logHeads.heads(1);
        assertEq(seq, head.seq, "the head posts");
        (uint64 drawnAt,,,,) = logHeads.latches(turn, 0);
        (uint64 skippedAt,,,,) = logHeads.latches(turn, 1);
        assertEq(drawnAt, 0, "the superseded assignment is not latched");
        assertEq(skippedAt, 0, "nor is the skip's");
    }

    function test_ADelayedRelayOfASupersededAssignmentsFirstHeadLatchesNothing() public {
        _startInOrder();
        _marked(TAKE);
        // Alice's enclave signs its first head under the drawn assignment; the relay lands after the
        // skip, within the head's tolerance.
        LogHeads.HeadV2 memory head = _nextHead();
        bytes memory signature = _sign(head, _signerKey());
        schedule.skip();
        vm.warp(block.timestamp + 20);
        logHeads.postV2(head, signature);
        (uint64 drawnAt,,,,) = logHeads.latches(turn, 0);
        assertEq(drawnAt, 0);
        assertEq(schedule.skipDueAt(), (_slot() + TAKE + 1) * INTERVAL, "bob's window runs on");
    }

    function test_ASkippedMembersLateHeadLatchesNothingAndItsSuccessorsDoes() public {
        _startInOrder();
        OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
        _marked(TAKE);
        schedule.skip();
        // Alice takes the keys late, under the assignment it was drawn for: its heads latch nothing.
        vm.warp(block.timestamp + 20);
        LogHeads.HeadV2 memory late = _nextHead();
        (late.position, late.generation, late.release) = (0, drawn.generation, registry.candidateOf(alice).release);
        _post(late);
        (uint64 lateAt,,,,) = logHeads.latches(turn, 0);
        assertEq(lateAt, 0);
        assertEq(logHeads.headRelease(1), late.release, "the newest head names the enclave holding the keys");
        // It hands over to bob, whose first head latches the skip's assignment.
        vm.warp(block.timestamp + 20);
        LogHeads.HeadV2 memory taken = _beat();
        assertEq(taken.position, 1);
        (uint64 at, uint64 epoch, address member, bytes32 release, uint64 generation) = logHeads.latches(turn, 1);
        assertEq(at, block.timestamp);
        assertEq(epoch, 1);
        assertEq(member, bob);
        assertEq(release, registry.candidateOf(bob).release);
        assertEq(generation, schedule.turnOf(turn).generation);
        assertEq(logHeads.headRelease(1), release);
    }

    function test_ASkipIsRefusedWhileNoTurnNamesTheOperator() public {
        vm.expectRevert(OperatorSchedule.NoTurn.selector);
        schedule.skip();
        assertEq(schedule.skipDueAt(), type(uint64).max);
    }

    function test_ATurnDrawnBeforeASkipForTheSkippedMemberTakesANewGeneration() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, alice);
        _draw(turn + 1);
        OperatorSchedule.Turn memory next = schedule.turnOf(turn + 1);
        assertEq(next.candidate, alice);
        assertEq(next.generation, schedule.turnOf(turn).generation, "a repeat turn continues its holder's");
        _marked(TAKE);
        schedule.skip();
        OperatorSchedule.Turn memory moved = schedule.turnOf(turn + 1);
        assertGt(moved.generation, schedule.turnOf(turn).generation, "alice holds no keys to continue");
        (, OperatorSchedule.Revision[] memory revisions) = schedule.assignmentOf(turn + 1, next.position);
        assertEq(revisions.length, 2, "recorded as a revision, erasing none");
        assertEq(revisions[0].generation, next.generation);
        assertEq(revisions[1].generation, moved.generation);
    }

    function test_TheSkippedToMembersNextTurnContinuesTheSkipsGeneration() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, bob);
        _draw(turn + 1);
        OperatorSchedule.Turn memory next = schedule.turnOf(turn + 1);
        assertEq(next.candidate, bob);
        _marked(TAKE);
        schedule.skip();
        uint64 skipped = schedule.turnOf(turn).generation;
        assertEq(schedule.turnOf(turn + 1).generation, skipped, "bob's next turn continues as a repeat turn");
        (, OperatorSchedule.Revision[] memory revisions) = schedule.assignmentOf(turn + 1, next.position);
        assertEq(revisions[revisions.length - 1].generation, skipped);
        assertEq(revisions[revisions.length - 1].release, registry.candidateOf(bob).release);
    }

    /// Carol's turn is drawn while alice is named, so alice is its source; the skip then names bob, who
    /// takes the keys carol takes at her boundary.
    function test_ASkippedToHolderCannotLeaveBeforeTheNextTurnsMemberTakesTheKeys() public {
        _skipAfterTheNextDraw();
        vm.warp(schedule.boundary(turn + 1));
        assertEq(schedule.sourceOf(turn + 1), alice, "the draw's source stays");
        assertTrue(registry.holdsTurn(bob), "bob holds the keys carol takes");
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
    }

    /// Bob stays through carol's turn even once she has taken his keys and her first head names her
    /// release, kept only by the latch of his assignment of the turn before it, and may leave once the
    /// turn after carol's starts.
    function test_ASkippedToHolderMayLeaveOnceTheTurnAfterTheNextStarts() public {
        _skipAfterTheNextDraw();
        _relayPicking(turn + 2, setId, dave);
        vm.warp(schedule.boundary(turn + 1));
        // Carol's first head, signed with the keys she took from bob, latches her assignment.
        _beat();
        (uint64 latchedAt,, address latched,,) = logHeads.latches(turn + 1, schedule.turnOf(turn + 1).position);
        assertEq(latchedAt, block.timestamp);
        assertEq(latched, carol);
        assertEq(logHeads.headRelease(vault.signerEpoch()), registry.candidateOf(carol).release);
        assertTrue(registry.holdsTurn(bob), "bob's head latched the turn before carol's");
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
        _draw(turn + 2);
        vm.warp(schedule.boundary(turn + 2));
        assertFalse(registry.holdsTurn(bob));
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateOf(bob).adminKey, address(0));
    }

    /// Alice is the source of carol's turn, but she never took her own: bob, skipped to, took its keys
    /// from its source and latched it, so at carol's boundary alice holds none of the keys carol takes.
    function test_TheNextTurnsSourceASkipPassedMayLeaveAtItsBoundary() public {
        _skipAfterTheNextDraw();
        vm.warp(schedule.boundary(turn + 1));
        assertEq(schedule.sourceOf(turn + 1), alice);
        assertFalse(registry.holdsTurn(alice));
        vm.prank(alice);
        registry.deregister();
        assertEq(registry.candidateOf(alice).adminKey, address(0));
    }

    /// Bob takes the turn before `turn` and is drawn for `turn` again, so he is the source of dave's turn
    /// after it; a skip names carol, whose head latches `turn` with the keys she took from him. Dave never
    /// takes his turn, so at the boundary of the turn after dave's the keys are carol's, and bob, the
    /// source of an earlier turn the walk passes, may leave.
    function test_AnEarlierTurnsSourceMayLeaveOnceALaterMemberTookTheKeys() public {
        address[] memory members = new address[](4);
        uint256[] memory stakes = new uint256[](4);
        (members[0], members[1], members[2], members[3]) = (alice, bob, carol, dave);
        (stakes[0], stakes[1], stakes[2], stakes[3]) = (40_000e18, 30_000e18, 20_000e18, 10_000e18);
        setId = _rotate(members, stakes);
        uint64 first = _nextTurn() + 1;
        address[] memory one = new address[](1);
        one[0] = bob;
        _relayOrder(first, one);
        turn = first + 1;
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, carol);
        seed = _relayOrder(turn, order);
        _relayPicking(turn + 1, setId, dave);
        _relayPicking(turn + 2, setId, alice);
        _draw(first);
        vm.warp(schedule.boundary(first));
        _beat();
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), bob);
        _marked(TAKE);
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, carol);
        vm.warp(block.timestamp + 20);
        _beat();
        (,, address latched,,) = logHeads.latches(turn, schedule.turnOf(turn).position);
        assertEq(latched, carol);
        _draw(turn + 2);
        assertEq(schedule.sourceOf(turn + 2), dave);
        vm.warp(schedule.boundary(turn + 2));
        assertFalse(registry.holdsTurn(bob));
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateOf(bob).adminKey, address(0));
    }

    /// Dave's turn is drawn while alice is named; the skip names bob, who takes the keys but posts no
    /// head before a skip past him to carol, who never takes them, so at dave's boundary bob holds them.
    function test_ASupersededHolderCannotLeaveBeforeTheNextTurnsMemberTakesTheKeys() public {
        address[] memory order = new address[](3);
        (order[0], order[1], order[2]) = (alice, bob, carol);
        _startTurn(order, dave);
        _draw(turn + 1);
        _marked(TAKE);
        schedule.skip();
        _marked(TAKE);
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, carol);
        vm.warp(schedule.boundary(turn + 1));
        assertTrue(registry.holdsTurn(bob), "bob holds the keys dave takes");
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
    }

    /// Alice holds the keys at the end of her turn; bob's turn after it is drawn while bob is named, and
    /// dave's while bob is; neither bob nor carol, skipped to, takes them, so at dave's boundary alice,
    /// the source of the turn before it, still holds them.
    function test_TheSourceOfAnUntakenSkippedTurnCannotLeave() public {
        address[] memory members = new address[](4);
        uint256[] memory stakes = new uint256[](4);
        (members[0], members[1], members[2], members[3]) = (alice, bob, carol, dave);
        (stakes[0], stakes[1], stakes[2], stakes[3]) = (40_000e18, 30_000e18, 20_000e18, 10_000e18);
        setId = _rotate(members, stakes);
        uint64 first = _nextTurn() + 1;
        address[] memory one = new address[](1);
        one[0] = alice;
        _relayOrder(first, one);
        turn = first + 1;
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, carol);
        seed = _relayOrder(turn, order);
        _relayPicking(turn + 1, setId, dave);
        _draw(first);
        vm.warp(schedule.boundary(first));
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), bob);
        _marked(TAKE);
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, carol);
        vm.warp(schedule.boundary(turn + 1));
        assertTrue(registry.holdsTurn(alice), "no member of bob's turn took alice's keys");
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(alice);
        registry.deregister();
    }

    function test_TheTimelockSetsTheTakeWindowWithinItsBoundsWhileNoTurnNamesTheOperator() public {
        (uint16 slots, uint16 minimum, uint16 maximum) = schedule.takeWindow();
        assertEq(slots, TAKE);
        assertEq(minimum, 2);
        assertEq(maximum, 8);
        vm.expectRevert(OperatorSchedule.UnauthorizedGovernance.selector);
        schedule.setTakeWindow(3);
        vm.prank(address(governance));
        vm.expectRevert(OperatorSchedule.InvalidTakeWindow.selector);
        schedule.setTakeWindow(1);
        vm.prank(address(governance));
        vm.expectRevert(OperatorSchedule.InvalidTakeWindow.selector);
        schedule.setTakeWindow(9);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TakeWindowChanged(8);
        vm.prank(address(governance));
        schedule.setTakeWindow(8);
        (slots,,) = schedule.takeWindow();
        assertEq(slots, 8);

        _startInOrder();
        vm.prank(address(governance));
        vm.expectRevert(OperatorSchedule.TurnNamesTheOperator.selector);
        schedule.setTakeWindow(3);
    }

    function test_TheTakeWindowsMinimumExceedsTheSwitchBudgetAnIntervalAndInclusion() public {
        OperatorSchedule.Liveness memory lo = OperatorSchedule.Liveness(60, 5, 2, 10);
        OperatorSchedule.Liveness memory hi = OperatorSchedule.Liveness(120, 30, 9, 240);
        // At the shortest interval, 60 s, and the longest tolerance, 30 s: one slot past the minimum's
        // first must exceed the budget plus the tolerance.
        _refusedWindow(lo, hi, OperatorSchedule.TakeWindow(2, 2, 8), 30);
        _build(lo, hi, OperatorSchedule.TakeWindow(3, 3, 8), 30);
        _build(lo, hi, OperatorSchedule.TakeWindow(2, 2, 8), 29);
        _refusedWindow(lo, hi, OperatorSchedule.TakeWindow(1, 1, 8), 0);
        _refusedWindow(lo, hi, OperatorSchedule.TakeWindow(0, 0, 8), 0);
        _refusedWindow(lo, hi, OperatorSchedule.TakeWindow(2, 3, 8), 10);
        _refusedWindow(lo, hi, OperatorSchedule.TakeWindow(9, 2, 8), 10);
        // A window whose maximum spans more than a day at the shortest interval.
        _refusedWindow(lo, hi, OperatorSchedule.TakeWindow(2, 2, 1441), 10);
        _build(lo, hi, OperatorSchedule.TakeWindow(2, 2, 1440), 10);
    }

    function test_GasOfASkip() public {
        _startInOrder();
        _marked(TAKE);
        vm.cool(address(logHeads));
        vm.cool(address(schedule));
        vm.cool(address(registry));
        vm.cool(address(vault));
        schedule.skip();
        uint256 skipGas = vm.lastCallGas().gasTotalUsed;
        console.log("skip, gas:", skipGas);
        assertLt(skipGas, 350_000);
    }

    function test_TheScheduleFitsEip170() public view {
        console.log("OperatorSchedule, bytes:", address(schedule).code.length);
        assertLe(address(schedule).code.length, 24_576, "EIP-170");
    }

    /// @dev Starts `turn` for alice, draws `turn + 1` for carol while alice is named, so alice is its
    /// source, skips `turn` to bob, and posts bob's first head, which latches the skip's assignment.
    function _skipAfterTheNextDraw() internal {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, carol);
        _draw(turn + 1);
        assertEq(schedule.turnOf(turn + 1).candidate, carol);
        _marked(TAKE);
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, bob);
        vm.warp(block.timestamp + 20);
        _beat();
        (, uint64 epoch, address latched,,) = logHeads.latches(turn, 1);
        assertEq(epoch, 1);
        assertEq(latched, bob);
    }

    function _build(
        OperatorSchedule.Liveness memory lo,
        OperatorSchedule.Liveness memory hi,
        OperatorSchedule.TakeWindow memory window,
        uint64 budget
    ) internal returns (OperatorSchedule) {
        return new OperatorSchedule(registry, receiver, logHeads, _liveness(), lo, hi, window, budget, BLOCK_MILLIS);
    }

    function _refusedWindow(
        OperatorSchedule.Liveness memory lo,
        OperatorSchedule.Liveness memory hi,
        OperatorSchedule.TakeWindow memory window,
        uint64 budget
    ) internal {
        vm.expectRevert(OperatorSchedule.InvalidTakeWindow.selector);
        _build(lo, hi, window, budget);
    }
}

/// The schedule's `promote` past an untaken turn: once `heartbeatMisses + 1` slots after the later of
/// the boundary's and the signer change's are marked with the turn's assignment unlatched, it passes
/// even while the incumbent beats.
contract UntakenTurnTest is SkipTest {
    function setUp() public {
        _deployLiveness();
    }

    function test_ASkipPassesOverTheMemberWhoseReleaseSignedTheNewestHead() public {
        address[] memory order = new address[](3);
        (order[0], order[1], order[2]) = (bob, alice, carol);
        _startSourced(order);
        // Alice, the turn's source, still holds the keys: her release signed the epoch's newest head.
        LogHeads.HeadV2 memory head = _incumbentHead();
        head.release = registry.candidateOf(alice).release;
        _post(head);
        _marked(TAKE);
        (, uint16 position) = schedule.skip();
        assertEq(position, 2, "alice, at position 1, is passed over");
        assertEq(schedule.turnOf(turn).candidate, carol);
    }

    function test_ASkipNamesTheSourceWhenAnotherReleaseHoldsTheKeys() public {
        address[] memory order = new address[](3);
        (order[0], order[1], order[2]) = (bob, alice, carol);
        _startSourced(order);
        _post(_incumbentHead());
        _marked(TAKE);
        (, uint16 position) = schedule.skip();
        assertEq(position, 1, "the source, whose release signed no head, is named");
        assertEq(schedule.turnOf(turn).candidate, alice);
    }

    function test_ASkipWaitsWhileTheSignerIsRetiredAndGivesAFreshWindowAfterAnotherIsInstalled() public {
        _startInOrder();
        vm.prank(multisig);
        vault.retireSigner();
        _marked(TAKE + 1);
        assertEq(schedule.skipDueAt(), type(uint64).max, "a skip while no head can latch");
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, type(uint64).max));
        schedule.skip();
        // The timelock installs the operator's registered enclave: the member gets a whole window from it.
        address installed = registry.operatorEnclave();
        vm.prank(address(governance));
        vault.setSigner(installed);
        assertEq(vault.checkpointSigner(), installed);
        assertEq(schedule.skipDueAt(), (block.timestamp / INTERVAL + TAKE + 1) * INTERVAL, "a window from the change");
    }

    function test_AnUntakenTurnsPromotionNamesTheNextQualifiedMemberTheSourceIncluded() public {
        address[] memory order = new address[](3);
        (order[0], order[1], order[2]) = (bob, alice, carol);
        _startSourced(order);
        address key = schedule.promotable(alice);
        _beatingThrough(MISSES + 1);
        (, uint16 position) = schedule.promote();
        assertEq(position, 1, "alice, the source, qualifies as #595 qualifies a member");
        assertEq(vault.checkpointSigner(), key, "the incumbent's own standby key: its old key is fenced out");
    }

    function test_AnUntakenTurnWithNoMemberToPromoteRefusesThePromotionAndItsTimeHolds() public {
        address[] memory order = new address[](1);
        order[0] = bob;
        _startSourced(order);
        uint64 dueAt = (_boundarySlot() + MISSES + 2) * INTERVAL;
        uint64 before = schedule.promotionDueAt();
        // Carol and dave lose their listings and alice's standby key is voided: past bob, no one qualifies,
        // which leaves the count where it was, so the incumbent's fence holds.
        vm.prank(carol);
        registry.deregister();
        vm.prank(dave);
        registry.deregister();
        _voidStandby(alice);
        assertEq(schedule.promotionDueAt(), before, "who qualifies moved the count");
        _beatingThrough(MISSES + 3);
        assertEq(schedule.promotionDueAt(), dueAt, "the untaken count from the boundary");
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoQualifiedPosition.selector, turn));
        schedule.promote();
        // A standby key registered now qualifies alice at once: the time the incumbent stopped by stands.
        _registerStandby(alice);
        assertEq(schedule.promotionDueAt(), dueAt, "a registration moved the count");
        schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, alice);
    }

    /// @dev Moves `member` to another approved release, which voids its standby key.
    function _voidStandby(address member) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(member, _digest(77), CONFIG, true);
        vm.prank(member);
        registry.setRelease(_digest(77), CONFIG);
        assertEq(schedule.promotable(member), address(0));
    }

    function test_APromotedAssignmentIsNeverSkipped() public {
        _startInOrder();
        _lapse();
        schedule.promote();
        assertEq(schedule.turnOf(turn).reason, 2);
        assertEq(schedule.skipDueAt(), type(uint64).max);
        _marked(TAKE + 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.SkipNotDue.selector, type(uint64).max));
        schedule.skip();
    }

    /// @dev The incumbent beats in each of the next `count` slots, each witnessed, and the clock moves
    /// five seconds into the slot after them.
    function _beatingThrough(uint256 count) internal {
        for (uint256 i = 0; i < count; ++i) {
            _nextSlot(10, 240);
            schedule.witness();
            _post(_incumbentHead());
        }
        _nextSlot(5, 240);
    }

    function test_AnUntakenTurnIsPromotedPastTheBeatingIncumbent() public {
        _startInOrder();
        uint64 dueAt = (_boundarySlot() + MISSES + 2) * INTERVAL;
        _beatingThrough(MISSES);
        // The sixth slot after the boundary's is not complete yet.
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
        schedule.promote();
        schedule.witness();
        _post(_incumbentHead());
        _nextSlot(0, 240);
        assertEq(block.timestamp, dueAt);
        assertEq(schedule.promotionDueAt(), dueAt, "the incumbent's heads do not move it");
        (uint16 position, address member) = _expectedPromotion();
        (, uint16 promoted) = schedule.promote();
        assertEq(promoted, position);
        assertEq(schedule.turnOf(turn).candidate, member);
        assertEq(vault.signerEpoch(), 2);
        // The incumbent's next head fails as the old epoch's.
        LogHeads.HeadV2 memory stale = _incumbentHead();
        bytes memory signature = _sign(stale, privateKeyOf[vault.signerAtEpoch(1)]);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        logHeads.postV2(stale, signature);
    }

    function test_TheMembersFirstHeadEndsTheUntakenPath() public {
        _startInOrder();
        _beatingThrough(MISSES);
        // Alice's first head latches its assignment before the count passes: only missed heads promote.
        _beat();
        assertEq(schedule.promotionDueAt(), (_slot() + MISSES + 1) * INTERVAL);
        _nextSlot(10, 240);
        schedule.witness();
        _nextSlot(5, 240);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, schedule.promotionDueAt()));
        schedule.promote();
    }

    function test_APromotionAfterChainDowntimeWaitsForTheResumedSlotsAndTheFirstResumedHeadStopsIt() public {
        _startInOrder();
        vm.warp(schedule.boundary(turn) + 600);
        ARB.setBlockNumber(ARB.arbBlockNumber() + 2_400);
        schedule.witness();
        uint64 resumed = _slot();
        _post(_incumbentHead());
        // With the incumbent beating, the untaken path counts from the first resumed slot.
        assertEq(schedule.promotionDueAt(), (resumed - 1 + MISSES + 2) * INTERVAL);
        _beatingThrough(MISSES);
        assertEq(block.timestamp, (resumed - 1 + MISSES + 2) * INTERVAL + 5);
        uint256 snapshot = vm.snapshotState();
        // Before the member's first resumed head: the promotion passes.
        schedule.promote();
        assertEq(vault.signerEpoch(), 2);
        vm.revertToState(snapshot);
        // After it: the head latches the turn, and the promotion is refused.
        _beat();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, schedule.promotionDueAt()));
        schedule.promote();
        assertEq(vault.signerEpoch(), 1);
    }

    function test_TwoPromotionsThroughTheUntakenPathNeedAFreshCount() public {
        _startInOrder();
        _beatingThrough(MISSES + 1);
        schedule.promote();
        uint64 changed = _slot();
        // No head under the new signer: both counts restart at the change, and the missed-head path,
        // one slot shorter, passes first.
        assertEq(schedule.promotionDueAt(), (changed + MISSES + 1) * INTERVAL);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, schedule.promotionDueAt()));
        schedule.promote();
        _nextSlot(10, 240);
        schedule.witness();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, schedule.promotionDueAt()));
        schedule.promote();
        _witnessFromNow(MISSES);
        _nextSlot(5, 240);
        schedule.promote();
        assertEq(vault.signerEpoch(), 3);
    }

    function test_ATurnOfAOneMemberSetIsExemptFromTheUntakenPath() public {
        setId = _rotateOne(alice);
        turn = _nextTurn() + 1;
        seed = _relayPicking(turn, setId, alice);
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _beatingThrough(MISSES + 3);
        assertEq(schedule.promotionDueAt(), (_slot() - 1 + MISSES + 1) * INTERVAL, "only missed heads count");
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, schedule.promotionDueAt()));
        schedule.promote();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoQualifiedPosition.selector, turn));
        schedule.skip();
    }
}
