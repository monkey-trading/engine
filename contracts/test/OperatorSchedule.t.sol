// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vm, console, stdError} from "forge-std/Test.sol";

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {Vault} from "../src/Vault.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";
import {MockSeedReceiver} from "./mocks/MockSeedReceiver.sol";

/// Rotation sets adopted at closes, turns drawn from them, and how the registry and the vault
/// follow the turns.
contract OperatorScheduleTest is ScheduleTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal dave = makeAddr("dave");
    address internal voter = makeAddr("voter");

    function setUp() public {
        _deploySchedule();
    }

    function test_AQuorumCloseAdoptsTheEligibleCandidatesByVotes() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        uint64 epoch = registry.openEpoch() + uint64((block.timestamp - registry.openEpochStart()) / EPOCH) - 1;
        uint64 effectiveAt = uint64(block.timestamp + 7 days + FINALITY_MARGIN);
        (address[] memory members, uint256[] memory weights) = _two(alice, bob);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(1, epoch, effectiveAt, true, members, _keys(members), weights);
        registry.closeEpoch();
        assertEq(registry.pendingElection().effectiveAt, 0, "a registry with a schedule elects no one");

        assertEq(schedule.pendingSetId(), 1);
        assertEq(schedule.currentSetId(), 0);
        vm.warp(effectiveAt - 1);
        assertEq(schedule.currentSetId(), 0);
        vm.warp(effectiveAt);
        assertEq(schedule.pendingSetId(), 0);
        assertEq(schedule.currentSetId(), 1);
        (uint64 setEpoch, uint64 setEffectiveAt, address[] memory candidates, uint256[] memory votes) =
            schedule.setOf(1);
        assertEq(setEpoch, epoch);
        assertEq(setEffectiveAt, effectiveAt);
        assertEq(candidates, members);
        assertEq(votes, weights);
    }

    function test_AQuorumCloseReplacesTheSetInEffect() public {
        _rotateTwo(alice, bob);
        _candidate(carol, 25_000e18, 3);
        _delegate(carol, carol);
        _stake(alice, 5_000e18);
        _delegate(alice, alice);
        _closeNext();
        (,, address[] memory candidates, uint256[] memory weights) = schedule.setOf(2);
        assertEq(candidates.length, 3);
        assertEq(candidates[0], alice);
        assertEq(weights[0], 35_000e18, "a quorum close weighs by the votes now");
        assertEq(candidates[2], carol);
        assertEq(weights[2], 25_000e18);
    }

    function test_ACloseBelowQuorumKeepsTheSetInEffectLessMembersNoLongerEligible() public {
        address[] memory members = new address[](3);
        uint256[] memory stakes = new uint256[](3);
        (members[0], members[1], members[2]) = (alice, bob, carol);
        (stakes[0], stakes[1], stakes[2]) = (30_000e18, 20_000e18, 10_000e18);
        _rotate(members, stakes);
        // 300k delegated to no candidate: 65k of 365k is under the 20% quorum, and carol's 10k is
        // under the 5% eligibility share.
        _stake(voter, 300_000e18);
        _delegate(voter, dave);
        _stake(alice, 5_000e18);
        _delegate(alice, alice);
        (address[] memory kept, uint256[] memory weights) = _two(alice, bob);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(
            2, _closingEpoch(), uint64(block.timestamp + 7 days + FINALITY_MARGIN), false, kept, _keys(kept), weights
        );
        registry.closeEpoch();
    }

    function test_EveryCloseQueuesItsSetToTakeEffectAtItsOwnTime() public {
        address[3] memory members = [alice, bob, carol];
        uint256[3] memory stakes = [uint256(30_000e18), 20_000e18, 15_000e18];
        for (uint256 i = 0; i < 3; ++i) {
            _candidate(members[i], stakes[i], i + 1);
            _delegate(members[i], members[i]);
        }
        _setRotation(true);
        // A close with everyone unlisted adopts nothing and leaves the next closes a day apart.
        for (uint256 i = 0; i < 3; ++i) {
            _list(members[i], i + 1, false);
        }
        _closeNext();
        assertEq(schedule.setCount(), 0);
        uint64[3] memory effectiveAt;
        for (uint256 i = 0; i < 3; ++i) {
            // Each close lists one member alone, so its set names it.
            _list(members[i], i + 1, true);
            _closeNext();
            _list(members[i], i + 1, false);
            effectiveAt[i] = uint64(block.timestamp + 7 days + FINALITY_MARGIN);
            (, uint64 adoptedAt, address[] memory adopted,) = schedule.setOf(uint32(i + 1));
            assertEq(adoptedAt, effectiveAt[i]);
            assertEq(adopted.length, 1);
            assertEq(adopted[0], members[i]);
        }
        assertEq(effectiveAt[1] - effectiveAt[0], EPOCH);
        assertEq(schedule.setCount(), 3);
        assertEq(schedule.pendingSetId(), 1, "the earliest waiting set is next");
        for (uint256 i = 0; i < 3; ++i) {
            _list(members[i], i + 1, true);
        }

        _relay(_nextTurn() + 1, keccak256("seed"));
        for (uint256 i = 0; i < 3; ++i) {
            vm.warp(effectiveAt[i]);
            assertEq(schedule.currentSetId(), i + 1);
            assertEq(schedule.pendingSetId(), i < 2 ? i + 2 : 0);
            // The one turn whose deadline falls while this set is in effect draws its member.
            uint64 turn = uint64((effectiveAt[i] - schedule.BOUNDARY_OFFSET() - 1) / 1 days + 2);
            assertGe(schedule.drawDeadline(turn), effectiveAt[i]);
            assertEq(_draw(turn), members[i]);
            assertEq(schedule.turnOf(turn).setId, i + 1);
        }
    }

    function test_ALateCloseAndThePromptOneAfterItQueueBoth() public {
        _candidate(alice, 30_000e18, 1);
        _delegate(alice, alice);
        _setRotation(true);
        _closeNext();
        // Called a second before two epochs have ended, the close leaves the next one due at once.
        vm.warp(uint256(registry.openEpochStart()) + 2 * EPOCH - 1);
        registry.closeEpoch();
        vm.warp(block.timestamp + 1);
        registry.closeEpoch();
        assertEq(schedule.setCount(), 3);
        (, uint64 second,,) = schedule.setOf(2);
        (, uint64 third,,) = schedule.setOf(3);
        assertEq(third, second + 1);
        assertEq(schedule.pendingSetId(), 1);
    }

    function test_ALaterSetNeverTakesEffectBeforeAnEarlierOne() public {
        _candidate(alice, 30_000e18, 1);
        _delegate(alice, alice);
        _setRotation(true);
        _closeNext();
        // A late close with the finality margin, then a prompt one a second later without it.
        vm.warp(uint256(registry.openEpochStart()) + 2 * EPOCH - 1);
        registry.closeEpoch();
        (, uint64 late,,) = schedule.setOf(2);
        OperatorRegistry.Parameters memory quick = _parameters();
        quick.finalityMargin = 0;
        vm.prank(address(governance));
        registry.setParameters(quick);
        vm.warp(block.timestamp + 1);
        assertLt(block.timestamp + 7 days, late, "its own notice would end before the late close's set");
        (address[] memory kept, uint256[] memory weights) = _one(alice);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(3, _closingEpoch(), late, true, kept, _keys(kept), weights);
        registry.closeEpoch();
        assertEq(schedule.setAt(late), 3);
    }

    function test_ACloseBelowQuorumDerivesFromTheLatestSetEvenWhileItWaits() public {
        address[] memory pair = new address[](2);
        uint256[] memory stakes = new uint256[](2);
        (pair[0], pair[1], stakes[0], stakes[1]) = (alice, bob, 30_000e18, 25_000e18);
        _rotate(pair, stakes);
        _candidate(carol, 25_000e18, 3);
        _delegate(carol, carol);
        _closeNext();
        assertEq(schedule.pendingSetId(), 2);
        // 420k delegated to no candidate: 80k of 500k is under the quorum, and every member keeps
        // the eligibility share.
        _stake(voter, 420_000e18);
        _delegate(voter, dave);
        address[] memory three = new address[](3);
        uint256[] memory weights = new uint256[](3);
        (three[0], three[1], three[2]) = (alice, bob, carol);
        (weights[0], weights[1], weights[2]) = (30_000e18, 25_000e18, 25_000e18);
        vm.warp(uint256(registry.openEpochStart()) + EPOCH);
        uint64 effectiveAt = uint64(block.timestamp + 7 days + FINALITY_MARGIN);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(3, _closingEpoch(), effectiveAt, false, three, _keys(three), weights);
        registry.closeEpoch();
    }

    function test_WhileAScheduleIsNamedEpochsLastADayAtLeast() public {
        OperatorRegistry.Parameters memory hourly = _parameters();
        hourly.epochLength = 1 hours;
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EpochTooShort.selector, 1 hours, 1 days));
        vm.prank(address(governance));
        registry.setParameters(hourly);

        _deployRegistry();
        OperatorSchedule own = _newSchedule();
        vm.prank(address(governance));
        registry.setParameters(hourly);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EpochTooShort.selector, 1 hours, 1 days));
        vm.prank(address(governance));
        registry.setSchedule(own);
        vm.prank(address(governance));
        registry.setParameters(_parameters());
        vm.prank(address(governance));
        registry.setSchedule(own);
    }

    function test_AnEmptySetIsNotAdoptedWithoutMembersInEffect() public {
        _setRotation(true);
        vm.recordLogs();
        _closeNext();
        assertEq(vm.getRecordedLogs().length, 1, "no eligible candidate: only EpochClosed");
        // Below quorum with no set in effect, nothing is kept either.
        _candidate(alice, 10_000e18, 1);
        _delegate(alice, alice);
        _stake(voter, 100_000e18);
        assertTrue(registry.isEligible(alice));
        _closeNext();
        assertEq(schedule.setCount(), 0);
    }

    function test_WhileRotationIsOffACloseAdoptsNothing() public {
        _candidate(alice, 30_000e18, 1);
        _delegate(alice, alice);
        _closeNext();
        assertEq(schedule.setCount(), 0);
    }

    function test_ADrawOpensAtTheDeadlineAndClosesAtTheBoundary() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        uint64 opensAt = schedule.drawDeadline(turn);
        uint64 closesAt = schedule.boundary(turn);
        vm.warp(opensAt - 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NotDrawable.selector, turn, opensAt, closesAt));
        schedule.draw(turn);
        vm.warp(closesAt);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NotDrawable.selector, turn, opensAt, closesAt));
        schedule.draw(turn);
        vm.warp(opensAt);
        assertEq(schedule.draw(turn), alice);
        assertEq(schedule.lastDrawnTurn(), turn);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.AlreadyDrawn.selector, turn));
        schedule.draw(turn);
    }

    function test_ADrawNeedsTheSwitchOnAndASetInEffect() public {
        _candidate(alice, 30_000e18, 1);
        _delegate(alice, alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        vm.warp(schedule.drawDeadline(turn));
        vm.expectRevert(OperatorSchedule.RotationOff.selector);
        schedule.draw(turn);
        vm.prank(address(governance));
        registry.setRotation(true);
        vm.expectRevert(OperatorSchedule.NoSetInEffect.selector);
        schedule.draw(turn);
        // A set adopted but not yet in effect draws nothing either.
        registry.closeEpoch();
        assertEq(schedule.pendingSetId(), 1);
        vm.expectRevert(OperatorSchedule.NoSetInEffect.selector);
        schedule.draw(turn);
    }

    function test_ADrawTakesTheReceiversSeed() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        bytes32 seed = keccak256("seed");
        _relay(turn, seed);
        vm.warp(schedule.drawDeadline(turn));
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnAssigned(turn, 1, 0, 0, alice, _key(alice), _release(1), 1);
        schedule.draw(turn);
        OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
        assertEq(drawn.seed, seed);
        assertEq(drawn.candidate, alice);
        assertEq(drawn.setId, 1);
        assertEq(drawn.generation, 1);
        assertEq(drawn.prior, 0);
        assertEq(vault.generationCounter(), 1);
    }

    function test_AReceiverWithNoRelayedSeedDrawsFromTheScheduleLastSeed() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        // An Ethereum fork's new receiver has no relay yet.
        SeedReceiver forked = new SeedReceiver(makeAddr("forked relay"), GENESIS);
        vm.prank(address(governance));
        schedule.setSeedReceiver(forked);
        _draw(turn + 1);
        assertEq(schedule.turnOf(turn + 1).seed, keccak256(abi.encode(keccak256("seed"), turn + 1)));
        // The mock's refusal falls back the same way.
        MockSeedReceiver mock = new MockSeedReceiver();
        vm.prank(address(governance));
        schedule.setSeedReceiver(SeedReceiver(address(mock)));
        _draw(turn + 2);
        assertEq(schedule.turnOf(turn + 2).seed, keccak256(abi.encode(schedule.turnOf(turn + 1).seed, turn + 2)));
    }

    function test_TheFallbackSeedDoesNotDependOnLaterDraws() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        bytes32 seed = keccak256("seed");
        _relay(turn, seed);
        _draw(turn);
        SeedReceiver forked = new SeedReceiver(makeAddr("forked relay"), GENESIS);
        vm.prank(address(governance));
        schedule.setSeedReceiver(forked);
        assertEq(schedule.seedAnchor(), seed);
        uint256 before = vm.snapshotState();
        _draw(turn + 1);
        _draw(turn + 2);
        bytes32 afterADraw = schedule.turnOf(turn + 2).seed;
        vm.revertToState(before);
        _draw(turn + 2);
        assertEq(schedule.turnOf(turn + 2).seed, afterADraw, "drawing turn + 1 or not leaves turn + 2's seed");
        assertEq(afterADraw, keccak256(abi.encode(seed, turn + 2)));
    }

    function test_AReceiverReplacedBeforeAnyDrawLeavesNoFallback() public {
        _rotateOne(alice);
        SeedReceiver forked = new SeedReceiver(makeAddr("forked relay"), GENESIS);
        vm.prank(address(governance));
        schedule.setSeedReceiver(forked);
        assertEq(schedule.seedAnchor(), bytes32(0));
        uint64 turn = _nextTurn() + 1;
        vm.warp(schedule.drawDeadline(turn));
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoSeed.selector, turn));
        schedule.draw(turn);
    }

    function test_TheFallbackNeedsADrawnTurn() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        vm.warp(schedule.drawDeadline(turn));
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoSeed.selector, turn));
        schedule.draw(turn);
    }

    function test_AnyOtherReceiverFailureRevertsTheDraw() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        MockSeedReceiver mock = new MockSeedReceiver();
        vm.prank(address(governance));
        schedule.setSeedReceiver(SeedReceiver(address(mock)));
        vm.warp(schedule.drawDeadline(turn + 1));

        mock.fail(MockSeedReceiver.Failure.OtherError);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.SeedNotFinal.selector, turn + 1, 0));
        schedule.draw(turn + 1);
        mock.fail(MockSeedReceiver.Failure.Panic);
        vm.expectRevert(stdError.assertionError);
        schedule.draw(turn + 1);
        // A caller who leaves the read too little gas gets a revert, never the fallback.
        mock.fail(MockSeedReceiver.Failure.OutOfGas);
        vm.expectRevert();
        schedule.draw{gas: 1_000_000}(turn + 1);
        assertEq(schedule.lastDrawnTurn(), turn);
    }

    function test_OnlyTheTimelockReplacesTheReceiver() public {
        SeedReceiver forked = new SeedReceiver(makeAddr("forked relay"), GENESIS);
        vm.expectRevert(OperatorSchedule.UnauthorizedGovernance.selector);
        schedule.setSeedReceiver(forked);
        address noCode = makeAddr("no code");
        vm.expectRevert(OperatorSchedule.ZeroAddress.selector);
        vm.prank(address(governance));
        schedule.setSeedReceiver(SeedReceiver(noCode));
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.SeedReceiverChanged(address(forked));
        vm.prank(address(governance));
        schedule.setSeedReceiver(forked);
        assertEq(address(schedule.seedReceiver()), address(forked));
    }

    function test_ADrawWalksPastUnlistedMembers() public {
        _rotateTwo(alice, bob);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (bob, _digest(2), CONFIG, false))
        );
        uint64 turn = _nextTurn() + 1;
        bytes32 seed = _relayPicking(turn, 1, bob);
        uint16 position = 1;
        while (schedule.memberAt(1, seed, turn, position) != alice) {
            ++position;
        }
        vm.warp(schedule.drawDeadline(turn));
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnAssigned(turn, 1, position, 0, alice, _key(alice), _release(1), 1);
        assertEq(schedule.draw(turn), alice);
        assertEq(schedule.turnOf(turn).position, position);

        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, false);
        vm.warp(schedule.drawDeadline(turn + 1));
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoAvailableMember.selector, turn + 1));
        schedule.draw(turn + 1);
    }

    function test_AScanDrawsTheOnlyAvailableMember() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 30_000e18, 2);
        vm.prank(address(governance));
        registry.setRotation(true);
        address[] memory pair = new address[](2);
        uint256[] memory weights = new uint256[](2);
        (pair[0], pair[1], weights[0], weights[1]) = (alice, bob, 99, 1);
        vm.prank(address(registry));
        schedule.adopt(1, uint64(block.timestamp), pair, _keys(pair), weights, true);
        MockSeedReceiver mock = new MockSeedReceiver();
        vm.prank(address(governance));
        schedule.setSeedReceiver(SeedReceiver(address(mock)));
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, false);

        uint64 turn = _nextTurn() + 1;
        vm.warp(schedule.drawDeadline(turn));
        uint256 scanned = 0;
        for (uint256 i = 0; i < 40; ++i) {
            mock.answerWith(keccak256(abi.encode("seed", i)));
            uint256 before = vm.snapshotState();
            assertEq(schedule.draw(turn), bob);
            uint16 position = schedule.turnOf(turn).position;
            assertLt(position, schedule.MAX_DRAW_POSITIONS() + 2);
            assertEq(schedule.memberAt(1, mock.answer(), turn, position), bob);
            if (position >= schedule.MAX_DRAW_POSITIONS()) ++scanned;
            vm.revertToState(before);
        }
        assertGt(scanned, 0, "some seeds leave every weighted position to alice");
        assertLt(scanned, 40, "some find bob among them");

        vm.prank(address(governance));
        registry.setReleaseApproval(bob, _digest(2), CONFIG, false);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoAvailableMember.selector, turn));
        schedule.draw(turn);
    }

    function test_AMemberKeepsItsGenerationWhileItStaysNamed() public {
        uint32 setId = _rotateTwo(alice, bob);
        uint64 turn = _nextTurn() + 1;
        address[6] memory picks = [alice, alice, bob, alice, alice, alice];
        for (uint64 i = 0; i < picks.length; ++i) {
            _relayPicking(turn + i, setId, picks[i]);
        }
        _draw(turn);
        _draw(turn + 1);
        assertEq(schedule.turnOf(turn + 1).generation, 1, "alice's next turn keeps her generation");
        _draw(turn + 2);
        assertEq(schedule.turnOf(turn + 2).generation, 2, "bob takes a new one");
        _draw(turn + 3);
        assertEq(schedule.turnOf(turn + 3).generation, 3, "alice after bob takes a new one");
        // No draw for turn + 4: alice stays named across the gap, and her holding continues.
        _draw(turn + 5);
        assertEq(schedule.turnOf(turn + 5).prior, turn + 3);
        assertEq(schedule.turnOf(turn + 5).generation, 3);
        assertEq(vault.generationCounter(), 3);
    }

    function test_TheIncumbentsFirstTurnKeepsItsFoldedGeneration() public {
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_digest(0), CONFIG)));
        // The deployment's operator registers on the release it runs.
        _candidate(deployerKey, 30_000e18, 0);
        _delegate(deployerKey, deployerKey);
        _setRotation(true);
        _closeNext();
        (, uint64 effectiveAt,,) = schedule.setOf(1);
        vm.warp(effectiveAt);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        assertEq(_draw(turn), deployerKey);
        assertEq(schedule.turnOf(turn).generation, 1);
        assertEq(schedule.sourceOf(turn), deployerKey);
        assertEq(vault.generationCounter(), 1, "no generation reserved");
        vm.warp(schedule.boundary(turn));
        assertEq(vault.currentRelease(), _release(0));
        assertEq(vault.releaseGeneration(), 1);
        assertEq(vault.syncedRelease(), _release(0));
    }

    function test_TheIncumbentTakesANewGenerationWhenTheVaultFoldedAnotherRelease() public {
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_digest(9), CONFIG)));
        _candidate(deployerKey, 30_000e18, 0);
        _delegate(deployerKey, deployerKey);
        _setRotation(true);
        assertEq(vault.currentRelease(), _release(0));
        assertEq(vault.syncedRelease(), _release(9), "generation 1 was folded for another release");
        _closeNext();
        (, uint64 effectiveAt,,) = schedule.setOf(1);
        vm.warp(effectiveAt);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        assertEq(schedule.turnOf(turn).generation, 2);
    }

    function test_AnotherMemberOnTheSameReleaseTakesANewGeneration() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 1);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        _closeNext();
        (, uint64 effectiveAt,,) = schedule.setOf(1);
        vm.warp(effectiveAt);
        uint64 turn = _nextTurn() + 1;
        _relayPicking(turn, 1, alice);
        _relayPicking(turn + 1, 1, bob);
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        assertEq(vault.syncedRelease(), _release(1));
        _draw(turn + 1);
        assertEq(schedule.turnOf(turn + 1).generation, 2, "a generation never passes between members");
    }

    function test_AnInstallationBetweenTurnsTakesANewGeneration() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        vm.prank(address(governance));
        registry.setRotation(false);
        vm.warp(schedule.turnsRunUntil());
        vm.prank(address(governance));
        registry.installOperator(carol, _key(carol), _digest(7), CONFIG);
        vm.prank(address(governance));
        registry.setRotation(true);
        uint64 later = _nextTurn();
        vm.warp(schedule.drawDeadline(later));
        assertEq(schedule.draw(later), alice);
        assertEq(schedule.turnOf(later).generation, 2, "the installed operator held the keys in between");
        assertEq(schedule.sourceOf(later), carol);
    }

    function test_ATurnDrawsFromTheSetInEffectAtItsDeadline() public {
        _rotateOne(alice);
        _relay(_nextTurn() + 1, keccak256("seed"));
        _candidate(bob, 20_000e18, 2);
        _delegate(bob, bob);
        // A set of bob alone, adopted while alice is unlisted.
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, false);
        _closeNext();
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, true);
        (, uint64 effectiveAt, address[] memory members,) = schedule.setOf(2);
        assertEq(members.length, 1);
        assertEq(members[0], bob);

        // The turn whose window holds the set's effective time draws from the set before it,
        // whenever in its window it is drawn.
        uint64 turn = uint64((effectiveAt - schedule.BOUNDARY_OFFSET()) / 1 days + 1);
        assertLt(schedule.drawDeadline(turn), effectiveAt);
        assertEq(schedule.setAt(schedule.drawDeadline(turn)), 1);
        vm.warp(effectiveAt - 1);
        uint256 before = vm.snapshotState();
        assertEq(schedule.draw(turn), alice);
        vm.revertToState(before);
        vm.warp(effectiveAt);
        assertEq(schedule.currentSetId(), 2);
        assertEq(schedule.draw(turn), alice);
        assertEq(schedule.turnOf(turn).setId, 1);
        assertEq(_draw(turn + 1), bob);
        assertEq(schedule.turnOf(turn + 1).setId, 2);
    }

    function test_AnEmptyResultReplacesTheSetInEffect() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        // Alice unlisted: below quorum, and no member of the set in effect stays eligible.
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, false);
        uint64 effectiveAt = uint64(block.timestamp + 7 days + FINALITY_MARGIN);
        address[] memory none = new address[](0);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(2, _closingEpoch(), effectiveAt, false, none, none, new uint256[](0));
        registry.closeEpoch();
        vm.warp(effectiveAt);
        assertEq(schedule.currentSetId(), 2);
        uint64 later = _nextTurn() + 1;
        vm.warp(schedule.drawDeadline(later));
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.EmptySet.selector, 2));
        schedule.draw(later);
        assertEq(registry.namedOperator().candidate, alice, "the last member stays named");
        // Another empty result changes nothing.
        vm.recordLogs();
        _closeNext();
        assertEq(vm.getRecordedLogs().length, 1, "only EpochClosed");
        assertEq(schedule.setCount(), 2);
    }

    function test_AQuorumCloseWithNoEligibleCandidateAdoptsAnEmptySet() public {
        _rotateTwo(alice, bob);
        OperatorRegistry.Parameters memory strict = _parameters();
        strict.eligibilityBps = 10_000;
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (strict)));
        assertFalse(registry.isEligible(alice));
        address[] memory none = new address[](0);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(
            2, _closingEpoch(), uint64(block.timestamp + 7 days + FINALITY_MARGIN), true, none, none, new uint256[](0)
        );
        registry.closeEpoch();
    }

    function test_ADrawSkipsAMemberRegisteredUnderAnotherAdminKey() public {
        uint32 setId = _rotateTwo(alice, bob);
        vm.prank(bob);
        registry.deregister();
        vm.warp(block.timestamp + COOLDOWN);
        uint256 newKey = uint256(keccak256("bob's next admin key"));
        (uint8 v, bytes32 r, bytes32 sig) = vm.sign(newKey, _registrationDigest(registry, bob, vm.addr(newKey)));
        OperatorRegistry.Registration memory registration = _registration(vm.addr(newKey), MIN_BOND, 2);
        vm.prank(bob);
        registry.register(registration, abi.encodePacked(r, sig, v));
        assertTrue(registry.isListed(bob), "its release is still approved");

        uint64 turn = _nextTurn() + 1;
        bytes32 seed = _relayPicking(turn, setId, bob);
        uint16 position = 1;
        while (schedule.memberAt(setId, seed, turn, position) != alice) {
            ++position;
        }
        vm.warp(schedule.drawDeadline(turn));
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnAssigned(turn, setId, position, 0, alice, _key(alice), _release(1), 1);
        schedule.draw(turn);

        // Below quorum the set carried over drops bob, whose key is no longer the one adopted.
        _stake(voter, 300_000e18);
        _delegate(voter, dave);
        (address[] memory kept, uint256[] memory weights) = _one(alice);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(
            2, _closingEpoch(), uint64(block.timestamp + 7 days + FINALITY_MARGIN), false, kept, _keys(kept), weights
        );
        registry.closeEpoch();
    }

    function test_TheOperatorBeforeTheTurnsMayLeaveOnceTurnsNameOthers() public {
        // The deployment's operator registers without votes, so the set names alice alone.
        _candidate(deployerKey, MIN_BOND, 0);
        uint32 setId = _rotateOne(alice);
        (,, address[] memory members,) = schedule.setOf(setId);
        assertEq(members.length, 1);
        _assertCannotLeave(deployerKey, "named while no turn names anyone");
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _assertCannotLeave(deployerKey, "the source of the turn in effect");
        _draw(turn + 1);
        vm.warp(schedule.boundary(turn + 1));
        assertEq(registry.namedOperator().candidate, alice);
        vm.prank(deployerKey);
        registry.deregister();
        assertEq(registry.candidateOf(deployerKey).adminKey, address(0));
    }

    function test_AMemberMayLeaveOnceAnInstallationEndsTheTurns() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        vm.prank(address(governance));
        registry.setRotation(false);
        vm.warp(schedule.turnsRunUntil());
        _assertCannotLeave(alice, "named after its turn ends");
        vm.prank(address(governance));
        registry.installOperator(carol, _key(carol), _digest(7), CONFIG);
        assertTrue(schedule.holdsTurn(alice));
        vm.prank(alice);
        registry.deregister();
        assertEq(registry.candidateOf(alice).adminKey, address(0));
    }

    function test_AReleaseMoveGivesTheTurnsTheMemberHoldsANewGeneration() public {
        _approve(alice, 3);
        _approve(alice, 4);
        _approve(alice, 5);
        uint32 setId = _rotateTwo(alice, bob);
        uint64 turn = _nextTurn() + 1;
        _relayPicking(turn, setId, alice);
        _relayPicking(turn + 1, setId, alice);
        _relayPicking(turn + 2, setId, bob);
        _draw(turn);
        _draw(turn + 1);
        assertEq(block.timestamp, schedule.boundary(turn), "alice's turn is in effect, her next is drawn");
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnGenerationMoved(turn, 2);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnGenerationMoved(turn + 1, 2);
        vm.prank(alice);
        registry.setRelease(_digest(3), CONFIG);
        assertEq(vault.currentRelease(), _release(3));
        assertEq(vault.releaseGeneration(), 2);

        // Alice in effect, bob drawn next: only alice's turn moves.
        _draw(turn + 2);
        assertEq(schedule.turnOf(turn + 2).generation, 3);
        vm.recordLogs();
        vm.prank(alice);
        registry.setRelease(_digest(4), CONFIG);
        assertEq(_moves(vm.getRecordedLogs()), 1);
        assertEq(schedule.turnOf(turn + 1).generation, 4);
        assertEq(schedule.turnOf(turn + 2).generation, 3, "bob's drawn turn keeps the generation it reserved");

        // Bob in effect: alice's moves touch no turn.
        vm.warp(schedule.boundary(turn + 2));
        vm.recordLogs();
        vm.prank(alice);
        registry.setRelease(_digest(5), CONFIG);
        assertEq(_moves(vm.getRecordedLogs()), 0);
    }

    function test_TheTurnViewsFollowTheBoundaries() public {
        uint32 setId = _rotateTwo(alice, bob);
        uint64 turn = _nextTurn() + 1;
        _relayPicking(turn, setId, alice);
        _relayPicking(turn + 1, setId, bob);
        _draw(turn);
        uint64 since = schedule.boundary(turn);
        (uint64 none,) = schedule.turnAt(uint64(block.timestamp));
        assertEq(none, 0, "drawn, not started");
        (address named,,,) = schedule.namedAt(uint64(block.timestamp));
        assertEq(named, address(0));
        assertEq(registry.namedOperator().candidate, deployerKey);

        vm.warp(since);
        assertTrue(schedule.holdsTurn(deployerKey), "the source of the turn in effect");
        (uint64 started, OperatorSchedule.Turn memory inEffect) = schedule.turnAt(since);
        assertEq(started, turn);
        assertEq(inEffect.candidate, alice);
        (address candidate, uint64 generation, uint64 from, uint64 until) = schedule.namedAt(since);
        assertEq(candidate, alice);
        assertEq(generation, 1);
        assertEq(from, since);
        assertEq(until, since + 1 days);
        OperatorRegistry.Operator memory operator = registry.operatorInEffect();
        assertEq(operator.candidate, alice);
        assertEq(operator.adminKey, _key(alice));
        assertEq(operator.release, _release(1));
        assertEq(operator.since, since);

        _draw(turn + 1);
        (started,) = schedule.turnAt(uint64(block.timestamp));
        assertEq(started, turn, "bob's turn starts at its own boundary");
        (started,) = schedule.turnAt(since + 1 days);
        assertEq(started, turn + 1);
        (started,) = schedule.turnAt(since - 1);
        assertEq(started, 0);
        assertEq(schedule.turnsRunUntil(), since + 2 days);
        assertEq(schedule.sourceOf(turn + 1), alice);
        assertEq(schedule.sourceOf(turn), deployerKey, "the first turn takes the keys from the operator before it");
        assertTrue(schedule.holdsTurn(alice));
        assertTrue(schedule.holdsTurn(bob));
        assertFalse(schedule.holdsTurn(carol));
    }

    function test_TheVaultFollowsTheTurnAtItsBoundaryWithoutATransaction() public {
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_digest(0), CONFIG)));
        _rotateOne(alice);
        assertEq(vault.currentRelease(), _release(0));
        assertEq(vault.releaseGeneration(), 1);
        assertEq(vault.syncedRelease(), _release(0));
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        assertEq(schedule.turnOf(turn).generation, 2);
        // Until the boundary the running release keeps its release, generation and fold.
        vm.warp(schedule.boundary(turn) - 1);
        assertEq(vault.currentRelease(), _release(0));
        assertEq(vault.releaseGeneration(), 1);
        assertEq(vault.syncedRelease(), _release(0));
        vm.warp(schedule.boundary(turn));
        assertEq(vault.currentRelease(), _release(1));
        assertEq(vault.releaseGeneration(), 2);
        assertEq(vault.syncedRelease(), _release(1));
        assertEq(registry.operatorGeneration(), 2);
        assertEq(registry.operatorEnclave(), registry.candidateOf(alice).enclave);
        vault.syncRelease();
        assertEq(vault.generationCounter(), 2, "a sync under a named turn is a no-op");
    }

    function test_DrawnTurnsStandAfterRotationTurnsOffUntilAnInstallation() public {
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_digest(0), CONFIG)));
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _draw(turn + 1);
        vm.prank(address(governance));
        registry.setRotation(false);
        assertTrue(registry.rotationEnabled(), "a drawn turn names the operator");
        vm.warp(schedule.drawDeadline(turn + 2));
        vm.expectRevert(OperatorSchedule.RotationOff.selector);
        schedule.draw(turn + 2);

        uint64 until = schedule.turnsRunUntil();
        assertEq(until, schedule.boundary(turn + 1) + 1 days);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.TurnsPending.selector, until));
        vm.prank(address(governance));
        registry.installOperator(carol, _key(carol), _digest(7), CONFIG);

        // Past the last turn's end its member stays named, and the timelock cannot name a release.
        vm.warp(uint256(until) + 10 days);
        assertEq(registry.namedOperator().candidate, alice);
        assertTrue(registry.rotationEnabled());
        assertEq(vault.currentRelease(), _release(1));
        vm.expectRevert(Vault.RotationActive.selector);
        vm.prank(address(governance));
        vault.setCurrentRelease(_digest(8), CONFIG);

        vm.prank(address(governance));
        registry.installOperator(carol, _key(carol), _digest(7), CONFIG);
        assertEq(registry.namedOperator().candidate, carol);
        assertEq(registry.operatorGeneration(), 0);
        assertFalse(registry.rotationEnabled());
        assertEq(vault.currentRelease(), _release(0));
        // The fold before the turns no longer stands: a node waits for a new generation.
        assertEq(vault.syncedRelease(), bytes32(0));
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), 3);
        assertEq(vault.syncedRelease(), _release(0));
    }

    function test_AMemberHoldingATurnCannotLeave() public {
        uint32 setId = _rotateTwo(alice, bob);
        uint64 turn = _nextTurn() + 1;
        _relayPicking(turn, setId, alice);
        _relayPicking(turn + 1, setId, bob);
        _relayPicking(turn + 2, setId, bob);
        _draw(turn);
        _assertCannotLeave(alice, "drawn, not started");
        vm.warp(schedule.boundary(turn));
        _assertCannotLeave(alice, "in effect");
        _draw(turn + 1);
        vm.warp(schedule.boundary(turn + 1));
        _assertCannotLeave(alice, "the source of the turn in effect");
        _draw(turn + 2);
        vm.warp(schedule.boundary(turn + 2));
        _assertCannotLeave(bob, "in effect");
        vm.prank(alice);
        registry.deregister();
        assertEq(registry.candidateOf(alice).adminKey, address(0));
    }

    function test_PromotionRevertsWithASchedule() public {
        _rotateTwo(alice, bob);
        vm.warp(block.timestamp + LIVENESS);
        vm.expectRevert(OperatorRegistry.ScheduleNamesTurns.selector);
        registry.promote();
    }

    function test_TheTimelockNamesAScheduleOnceWhileRotationIsOff() public {
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ScheduleAlreadySet.selector, address(schedule)));
        vm.prank(address(governance));
        registry.setSchedule(schedule);

        OperatorSchedule other = schedule;
        _deployRegistry();
        OperatorSchedule own = _newSchedule();
        vm.expectRevert(OperatorRegistry.UnauthorizedGovernance.selector);
        registry.setSchedule(own);
        vm.expectRevert(OperatorRegistry.InvalidSchedule.selector);
        vm.prank(address(governance));
        registry.setSchedule(other);
        vm.prank(address(governance));
        registry.setRotation(true);
        vm.expectRevert(OperatorRegistry.RotationActive.selector);
        vm.prank(address(governance));
        registry.setSchedule(own);
        vm.prank(address(governance));
        registry.setRotation(false);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.ScheduleSet(address(own));
        vm.prank(address(governance));
        registry.setSchedule(own);
        assertEq(address(registry.schedule()), address(own));
    }

    function test_OnlyTheScheduleReservesAGeneration() public {
        vm.expectRevert(Vault.UnauthorizedSchedule.selector);
        vault.reserveGeneration();
        vm.prank(address(schedule));
        assertEq(vault.reserveGeneration(), 1);
        vm.expectRevert(OperatorSchedule.UnauthorizedRegistry.selector);
        schedule.releaseMoved(alice);
        vm.expectRevert(OperatorSchedule.UnauthorizedRegistry.selector);
        schedule.adopt(0, 0, new address[](0), new address[](0), new uint256[](0), true);
    }

    /// @notice Reports, before refunds and with cold storage as in their own transactions, a close
    /// adopting the most members a set can hold, the candidate cap's limit plus the operator's place
    /// beyond it, and a draw from them.
    function test_GasOfAnAdoptionAndADrawAtTheMemberLimit() public {
        OperatorRegistry.Parameters memory wide = _parameters();
        wide.maxCandidates = uint16(registry.CANDIDATE_LIMIT());
        wide.eligibilityBps = 0;
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (wide)));
        for (uint256 i = 0; i < 100; ++i) {
            address member = makeAddr(string(abi.encode("member", i)));
            _candidate(member, MIN_BOND + i * 1e18, i + 1);
            _delegate(member, member);
        }
        // The named operator takes the one place beyond the cap; no one else may.
        _candidate(deployerKey, MIN_BOND, 0);
        _delegate(deployerKey, deployerKey);
        _stake(dave, MIN_BOND);
        bytes memory authorization = _authorization(dave);
        vm.prank(dave);
        registry.register(_registration(_key(dave), MIN_BOND, 200), authorization);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, 100));
        vm.prank(address(governance));
        registry.setReleaseApproval(dave, _digest(200), CONFIG, true);

        _setRotation(true);
        _cool();
        registry.closeEpoch();
        uint256 close = vm.lastCallGas().gasTotalUsed;
        (, uint64 effectiveAt, address[] memory members,) = schedule.setOf(1);
        assertEq(members.length, registry.CANDIDATE_LIMIT() + 1);

        vm.warp(effectiveAt);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        vm.warp(schedule.drawDeadline(turn));
        _cool();
        schedule.draw(turn);
        uint256 drawn = vm.lastCallGas().gasTotalUsed;
        console.log("close adopting 101 members, gas:", close);
        console.log("draw from 101 members, gas:", drawn);
        assertLt(close, 12_000_000);
        assertLt(drawn, 450_000);
    }

    function _cool() internal {
        vm.cool(address(registry));
        vm.cool(address(staking));
        vm.cool(address(schedule));
        vm.cool(address(receiver));
        vm.cool(address(vault));
    }

    function _approve(address candidate, uint256 seed) internal {
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.setReleaseApproval, (candidate, _digest(seed), CONFIG, true))
        );
    }

    /// @dev Approves, or withdraws approval of, `candidate`'s release `seed` straight from the timelock.
    function _list(address candidate, uint256 seed, bool approved) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(candidate, _digest(seed), CONFIG, approved);
    }

    function _keys(address[] memory members) internal view returns (address[] memory keys) {
        keys = new address[](members.length);
        for (uint256 i = 0; i < members.length; ++i) {
            keys[i] = _key(members[i]);
        }
    }

    /// @dev The epoch a close now closes.
    function _closingEpoch() internal view returns (uint64) {
        return registry.openEpoch() + uint64((block.timestamp - registry.openEpochStart()) / EPOCH) - 1;
    }

    function _moves(Vm.Log[] memory logs) internal pure returns (uint256 count) {
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].topics[0] == OperatorSchedule.TurnGenerationMoved.selector) ++count;
        }
    }

    function _assertCannotLeave(address member, string memory why) internal {
        vm.prank(member);
        try registry.deregister() {
            revert(why);
        } catch (bytes memory reason) {
            assertEq(bytes4(reason), OperatorRegistry.OperatorCannotLeave.selector, why);
        }
    }
}

/// The weighted pick and the draw window under fuzzing.
contract OperatorScheduleFuzzTest is ScheduleTest {
    address internal alice = makeAddr("alice");

    function setUp() public {
        _deploySchedule();
        _rotateOne(alice);
    }

    function testFuzz_MemberAtIsTheFirstMemberPastTheSeedsPoint(
        uint256 size,
        bytes32 weightSeed,
        bytes32 seed,
        uint64 turn,
        uint16 position
    ) public {
        size = bound(size, 1, 100);
        address[] memory members = new address[](size);
        uint256[] memory weights = new uint256[](size);
        for (uint256 i = 0; i < size; ++i) {
            members[i] = address(uint160(i + 1));
            weights[i] = bound(uint256(keccak256(abi.encode(weightSeed, i))), 1, 1e30);
        }
        vm.prank(address(registry));
        schedule.adopt(1, uint64(block.timestamp), members, members, weights, true);
        uint32 setId = schedule.setCount();

        uint256 total = 0;
        for (uint256 i = 0; i < size; ++i) {
            total += weights[i];
        }
        address expected;
        if (position < schedule.MAX_DRAW_POSITIONS()) {
            uint256 point = uint256(keccak256(abi.encode(seed, turn, position))) % total;
            uint256 cumulative = 0;
            for (uint256 i = 0; i < size; ++i) {
                cumulative += weights[i];
                if (cumulative > point) {
                    expected = members[i];
                    break;
                }
            }
        } else {
            // Past the weighted positions, the scan walks the members in order from a seeded start.
            uint256 start = uint256(keccak256(abi.encode(seed, turn, schedule.MAX_DRAW_POSITIONS()))) % size;
            expected = members[(start + position - schedule.MAX_DRAW_POSITIONS()) % size];
        }
        assertEq(schedule.memberAt(setId, seed, turn, position), expected);
    }

    function testFuzz_ADrawRunsExactlyInItsWindow(uint256 at) public {
        uint64 turn = _nextTurn() + 3;
        _relay(turn, keccak256("seed"));
        uint64 opensAt = schedule.drawDeadline(turn);
        uint64 closesAt = schedule.boundary(turn);
        vm.warp(bound(at, opensAt - 2 days, closesAt + 2 days));
        if (block.timestamp < opensAt || block.timestamp >= closesAt) {
            vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NotDrawable.selector, turn, opensAt, closesAt));
        }
        schedule.draw(turn);
    }
}
