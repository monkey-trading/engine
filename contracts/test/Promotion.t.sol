// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {LogHeads} from "../src/LogHeads.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {Vault} from "../src/Vault.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// The schedule's `promote`: the vault's signer moving to the next qualified member's standby key in
/// the promotion transaction, the assignment it records, the order it walks, and its refusals.
contract PromotionTest is LivenessTest {
    function setUp() public {
        _deployLiveness();
    }

    function _postCheckpoint(uint64 seq, bytes32 root) internal {
        bool hasParent = vault.hasCheckpoint();
        (uint64 parentSeq, bytes32 parentRoot) = (vault.latestSeq(), vault.latestRoot());
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(vault),
                seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(_signerKey(), digest);
        vault.postCheckpoint(seq, root, hasParent, parentSeq, parentRoot, abi.encodePacked(r, s, v));
    }

    function _assignment(uint64 at, uint16 position)
        internal
        view
        returns (OperatorSchedule.Assignment memory assignment, OperatorSchedule.Revision[] memory revisions)
    {
        return schedule.assignmentOf(at, position);
    }

    function _moveRelease(address candidate, uint256 seed_) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(candidate, _digest(seed_), CONFIG, true);
        vm.prank(candidate);
        registry.setRelease(_digest(seed_), CONFIG);
    }

    function test_APromotionMovesTheSignerToTheStandbyKeyInANewEpoch() public {
        _startTurn();
        _postCheckpoint(5, keccak256("root 5"));
        LogHeads.HeadV2 memory head = _beat();
        _lapse();
        (uint16 position, address member) = _expectedPromotion();
        address key = schedule.promotable(member);
        assertTrue(key != address(0));
        bytes32 release = registry.candidateOf(member).release;
        uint64 generation = vault.generationCounter() + 1;
        address enclave = vault.checkpointSigner();

        vm.expectEmit(address(schedule));
        emit OperatorSchedule.TurnAssigned(turn, setId, position, 2, member, _key(member), release, generation);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.StandbyConsumed(member, key, 2);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.Promoted(turn, position, member, key, 2, head.seq, head.link, 5, keccak256("root 5"));
        vm.expectEmit(address(vault));
        emit Vault.SignerChanged(enclave, key, 2);
        (uint64 promotedTurn, uint16 promotedPosition) = schedule.promote();
        assertEq(promotedTurn, turn);
        assertEq(promotedPosition, position);

        assertEq(vault.checkpointSigner(), key);
        assertEq(vault.signerEpoch(), 2);
        assertEq(vault.signerAtEpoch(2), key);
        assertEq(vault.signerSince(), block.timestamp);
        assertTrue(vault.wasSigner(key));

        (OperatorSchedule.Assignment memory promoted, OperatorSchedule.Revision[] memory revisions) =
            _assignment(turn, position);
        assertEq(promoted.candidate, member);
        assertEq(promoted.adminKey, _key(member));
        assertEq(promoted.setId, setId);
        assertEq(promoted.reason, 2);
        assertEq(promoted.standbyKey, key);
        assertEq(promoted.signerEpoch, 2);
        assertEq(promoted.headSeq, head.seq);
        assertEq(promoted.headLink, head.link);
        assertEq(promoted.checkpointSeq, 5);
        assertEq(promoted.checkpointRoot, keccak256("root 5"));
        assertEq(promoted.at, block.timestamp);
        assertEq(revisions.length, 1);
        assertEq(revisions[0].release, release);
        assertEq(revisions[0].generation, generation);

        // The record is consumed; the key stays registered to the member, so it never registers again.
        (address recorded,,) = schedule.standbyOf(member);
        assertEq(recorded, address(0));
        assertEq(schedule.promotable(member), address(0));
        assertEq(schedule.standbyHolder(key), member);
        assertTrue(schedule.heldTurn(turn, member));
    }

    function test_TheTurnViewsAndTheVaultFollowAPromotion() public {
        _startTurn();
        OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
        _lapse();
        (uint16 position, address member) = _expectedPromotion();
        schedule.promote();
        uint64 generation = vault.generationCounter();

        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        assertEq(current.candidate, member);
        assertEq(current.position, position);
        assertEq(current.reason, 2);
        assertEq(current.generation, generation);
        assertEq(current.setId, drawn.setId, "the draw's set stays");
        assertEq(current.prior, drawn.prior);
        assertEq(current.seed, drawn.seed);
        assertEq(current.source, drawn.source);
        (uint64 inEffect, OperatorSchedule.Turn memory named) = schedule.turnAt(uint64(block.timestamp));
        assertEq(inEffect, turn);
        assertEq(named.candidate, member);
        (address candidate, uint64 namedGeneration, uint64 since, uint64 until) =
            schedule.namedAt(uint64(block.timestamp));
        assertEq(candidate, member);
        assertEq(namedGeneration, generation);
        assertEq(since, schedule.boundary(turn));
        assertEq(until, schedule.boundary(turn) + 1 days);

        assertEq(registry.namedOperator().candidate, member);
        assertEq(registry.namedOperator().adminKey, _key(member));
        assertEq(registry.operatorGeneration(), generation);
        assertEq(vault.currentRelease(), registry.candidateOf(member).release);
        assertEq(vault.releaseGeneration(), generation);
        assertEq(vault.syncedRelease(), vault.currentRelease());

        // The draw's assignment is kept as it was.
        (OperatorSchedule.Assignment memory first, OperatorSchedule.Revision[] memory revisions) = _assignment(turn, 0);
        assertEq(first.candidate, alice);
        assertEq(first.reason, 0);
        assertEq(first.standbyKey, address(0));
        assertEq(revisions.length, 1);
        assertEq(revisions[0].generation, drawn.generation);
    }

    function test_PromotionsPassSeveralSilentMembersInARow() public {
        _startTurn();
        address[3] memory promoted;
        uint16 last = 0;
        for (uint256 i = 0; i < 3; ++i) {
            // No promoted member ever posts a head: each is passed in turn.
            _lapse();
            (uint16 position, address member) = _expectedPromotion();
            (, uint16 at) = schedule.promote();
            assertEq(at, position);
            assertGt(at, last, "positions only advance within a turn");
            assertEq(schedule.turnOf(turn).candidate, member);
            (OperatorSchedule.Assignment memory assignment,) = _assignment(turn, at);
            assertEq(vault.checkpointSigner(), assignment.standbyKey);
            assertEq(vault.signerEpoch(), 2 + i);
            for (uint256 j = 0; j < i; ++j) {
                assertTrue(promoted[j] != member, "a member is never retried in a turn");
            }
            (promoted[i], last) = (member, at);
        }
        assertTrue(promoted[0] != alice && promoted[1] != alice && promoted[2] != alice);
    }

    function test_OrderExhaustionRevertsNoQualifiedPosition() public {
        _startTurn();
        // Bob, carol and dave held nothing of the turn; alice, drawn, never took its keys, so she comes
        // last. Each then held the keys under a promotion, and none qualifies again.
        for (uint256 i = 0; i < 4; ++i) {
            _lapse();
            schedule.promote();
            assertEq(schedule.turnOf(turn).candidate == alice, i == 3);
        }
        _lapse();
        address signer = vault.checkpointSigner();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.NoQualifiedPosition.selector, turn));
        schedule.promote();
        assertEq(vault.checkpointSigner(), signer);
    }

    function test_AMemberThatHeldAnAssignmentOfTheTurnIsPassed() public {
        address[] memory order = new address[](5);
        (order[0], order[1], order[2], order[3], order[4]) = (alice, bob, bob, alice, carol);
        _startTurn(order);
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(position, 1);
        assertEq(schedule.turnOf(turn).candidate, bob);
        // Position 2 names bob again, who held the keys under his promotion, and position 3 alice, who
        // held the draw but never the keys: carol, at position 4, held nothing of the turn and comes first.
        _lapse();
        (, position) = schedule.promote();
        assertEq(position, 4);
        assertEq(schedule.turnOf(turn).candidate, carol);
        assertTrue(schedule.heldKeys(turn, bob));
        assertFalse(schedule.heldKeys(turn, alice));
    }

    function test_AMemberWithoutAPromotableKeyOrListingIsPassed() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (alice, bob, carol, dave);
        _startTurn(order);
        // Bob's release move voids his standby key; carol leaves the registry.
        _moveRelease(bob, 60);
        vm.prank(carol);
        registry.deregister();
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(position, 3);
        assertEq(schedule.turnOf(turn).candidate, dave);
    }

    /// Bob, at position 1, has no promotable key at the first promotion and registers one after it:
    /// the second promotion goes on from position 2, never back to him.
    function test_PositionsOnlyAdvanceWithinATurn() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (alice, bob, carol, dave);
        _startTurn(order);
        _moveRelease(bob, 60);
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(position, 2);
        _registerStandby(bob);
        assertTrue(schedule.promotable(bob) != address(0));
        _lapse();
        (, position) = schedule.promote();
        assertEq(position, 3);
        assertEq(schedule.turnOf(turn).candidate, dave);
    }

    function test_PromotionIsRefusedWhileNoTurnNamesTheOperator() public {
        _startTurn();
        vm.warp(schedule.boundary(turn) - 1);
        vm.expectRevert(OperatorSchedule.NoTurn.selector);
        schedule.promote();
        assertEq(schedule.promotionDueAt(), type(uint64).max);
    }

    function test_PromotionIsRefusedWhileTheGuardianHasRetiredTheSigner() public {
        _startTurn();
        _lapse();
        vm.prank(multisig);
        vault.retireSigner();
        vm.expectRevert(Vault.NoSigner.selector);
        schedule.promote();
        assertEq(vault.checkpointSigner(), address(0));
        assertEq(vault.signerEpoch(), 1);
    }

    function test_PromotionIsRefusedOnceTheExitHasStarted() public {
        _startTurn();
        ExitVerifier.ExitTerms memory terms;
        vault.startExit(terms, new bytes32[](0));
        _lapse();
        vm.expectRevert(Vault.ExitActive.selector);
        schedule.promote();
        assertEq(vault.signerEpoch(), 1);
    }

    function test_OnlyTheScheduleInstallsAStandbyKey() public {
        _startTurn();
        address key = schedule.promotable(bob);
        vm.expectRevert(Vault.UnauthorizedSchedule.selector);
        vault.promoteSigner(key);
        vm.expectRevert(Vault.UnauthorizedSchedule.selector);
        vm.prank(address(registry));
        vault.promoteSigner(key);
        vm.expectRevert(Vault.ZeroAddress.selector);
        vm.prank(address(schedule));
        vault.promoteSigner(address(0));
    }

    function test_AMemberHoldingAnyAssignmentOfTheRunningTurnCannotLeave() public {
        _startTurn();
        _lapse();
        schedule.promote();
        address promoted = schedule.turnOf(turn).candidate;
        // Alice's assignment was superseded, but she held one of the running turn.
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(alice);
        registry.deregister();
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(promoted);
        registry.deregister();
        assertTrue(registry.holdsTurn(alice));
        assertTrue(registry.holdsTurn(promoted));
    }

    /// Carol's turn is drawn while alice is named, so alice is its source; bob's promotion then moves
    /// the keys carol takes at her boundary to him.
    function test_APromotedHolderCannotLeaveBeforeTheNextTurnsMemberTakesTheKeys() public {
        _promoteAfterTheNextDraw();
        vm.warp(schedule.boundary(turn + 1));
        assertEq(schedule.sourceOf(turn + 1), alice, "the draw's source stays");
        assertTrue(registry.holdsTurn(bob), "bob holds the keys carol takes");
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
    }

    /// Bob stays through carol's turn, as a source does, even once she has taken his keys, and may leave
    /// once the turn after it starts.
    function test_APromotedHolderMayLeaveOnceTheTurnAfterTheNextStarts() public {
        _promoteAfterTheNextDraw();
        _relayPicking(turn + 2, setId, dave);
        vm.warp(schedule.boundary(turn + 1));
        // Carol's first head, signed with the keys she took from bob, latches her assignment.
        _beat();
        (uint64 latchedAt,, address latched,,) = logHeads.latches(turn + 1, schedule.turnOf(turn + 1).position);
        assertEq(latchedAt, block.timestamp);
        assertEq(latched, carol);
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

    /// Carol never takes her turn, so bob keeps the keys into the turn after it, until its member takes
    /// them.
    function test_APromotedHolderStaysThroughATurnNoMemberTook() public {
        _promoteAfterTheNextDraw();
        _relayPicking(turn + 2, setId, dave);
        _relayPicking(turn + 3, setId, alice);
        _draw(turn + 2);
        vm.warp(schedule.boundary(turn + 2));
        assertTrue(registry.holdsTurn(bob), "no member took bob's keys");
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
        // Dave's first head, signed with the keys he took from bob, latches his assignment.
        _beat();
        (, uint64 epoch, address latched,,) = logHeads.latches(turn + 2, schedule.turnOf(turn + 2).position);
        assertEq(epoch, 2);
        assertEq(latched, dave);
        _draw(turn + 3);
        vm.warp(schedule.boundary(turn + 3));
        assertFalse(registry.holdsTurn(bob));
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateOf(bob).adminKey, address(0));
    }

    /// A promotion in carol's turn moves the signer to another standby key, so the keys the turn after
    /// it takes are no longer bob's.
    function test_APromotionInTheNextTurnFreesThePromotedHolder() public {
        _promoteAfterTheNextDraw();
        _relayPicking(turn + 2, setId, dave);
        _draw(turn + 2);
        _lapse();
        (uint64 promotedIn,) = schedule.promote();
        assertEq(promotedIn, turn + 1);
        vm.warp(schedule.boundary(turn + 2));
        assertFalse(registry.holdsTurn(bob));
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateOf(bob).adminKey, address(0));
    }

    /// No member takes the keys bob holds for a week of turns, and he signs no head: he stays while a
    /// member taking a turn looks back to him, seven turns, and may leave at the eighth.
    function test_APromotedHolderStaysForAtMostAWeekOfUntakenTurns() public {
        _promoteAfterTheNextDraw();
        _relayWithoutBob();
        for (uint64 next = turn + 2; next <= turn + 7; ++next) {
            _draw(next);
        }
        vm.warp(schedule.boundary(turn + 7));
        assertTrue(registry.holdsTurn(bob), "seven turns back");
        _draw(turn + 8);
        vm.warp(schedule.boundary(turn + 8));
        assertFalse(registry.holdsTurn(bob), "eight turns back");
        vm.prank(bob);
        registry.deregister();
    }

    /// Bob signs heads with the keys no member takes for a week of turns: he stays while his release
    /// signs the newest head, past the walk's seven turns, until a member's head shows it took them.
    function test_APromotedHolderSigningTheNewestHeadStaysPastAWeek() public {
        _promoteAfterTheNextDraw();
        vm.warp(block.timestamp + 20);
        _beat();
        _relayWithoutBob();
        for (uint64 next = turn + 2; next <= turn + 8; ++next) {
            _draw(next);
        }
        vm.warp(schedule.boundary(turn + 8));
        assertTrue(registry.holdsTurn(bob), "his release signed the newest head");
        // The member of turn + 8 takes the keys, and its first head signs with its own release.
        _beat();
        assertFalse(registry.holdsTurn(bob));
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateOf(bob).adminKey, address(0));
    }

    /// Alice takes her turn and fails; bob's promotion replaces her keys before carol's turn is drawn,
    /// so carol takes bob's keys and alice holds none.
    function test_AMemberAPromotionPassedMayLeaveAtTheNextBoundary() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, carol);
        _beat();
        (uint64 latchedAt,,,,) = logHeads.latches(turn, 0);
        assertEq(latchedAt, block.timestamp, "alice took her turn");
        _lapse();
        schedule.promote();
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), bob);
        vm.warp(schedule.boundary(turn + 1));
        assertTrue(registry.holdsTurn(bob), "bob holds the keys carol takes");
        assertFalse(registry.holdsTurn(alice));
        vm.prank(alice);
        registry.deregister();
        assertEq(registry.candidateOf(alice).adminKey, address(0));
    }

    /// Alice takes her turn, and carol's is drawn while alice is named, so alice is its source; alice
    /// then fails and bob's promotion replaces her keys, so at carol's boundary she holds none of them.
    function test_TheNextTurnsSourceAPromotionPassedMayLeaveAtItsBoundary() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, carol);
        _beat();
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), alice);
        _lapse();
        schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, bob);
        vm.warp(schedule.boundary(turn + 1));
        assertTrue(registry.holdsTurn(bob), "bob holds the keys carol takes");
        assertFalse(registry.holdsTurn(alice));
        vm.prank(alice);
        registry.deregister();
        assertEq(registry.candidateOf(alice).adminKey, address(0));
    }

    /// Once the turns end, the timelock installs alice afresh, who held the last turn before bob's
    /// promotion and holds the keys the next drawn turn's member takes; bob holds none of them.
    function test_AnInstallationFreesTheLastTurnsPromotedMember() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order);
        _lapse();
        schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, bob);
        vm.prank(address(governance));
        registry.setRotation(false);
        vm.warp(schedule.turnsRunUntil());
        vm.prank(address(governance));
        registry.installOperator(alice, _key(alice), _digest(7), CONFIG);
        vm.prank(address(governance));
        registry.setRotation(true);
        uint64 later = _nextTurn() + 1;
        _relayPicking(later, setId, carol);
        _draw(later);
        assertEq(schedule.sourceOf(later), alice);
        vm.warp(schedule.boundary(later));
        assertTrue(registry.holdsTurn(alice), "the installed operator hands carol the keys");
        assertFalse(registry.holdsTurn(bob));
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateOf(bob).adminKey, address(0));
    }

    /// @dev Relays seeds for `turn + 2` to `turn + 8` that pick dave, alice and carol, never bob.
    function _relayWithoutBob() internal {
        address[3] memory others = [dave, alice, carol];
        for (uint64 next = turn + 2; next <= turn + 8; ++next) {
            _relayPicking(next, setId, others[next % 3]);
        }
    }

    /// @dev Starts `turn` for alice, draws `turn + 1` for carol, and promotes bob in `turn`.
    function _promoteAfterTheNextDraw() internal {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, carol);
        _draw(turn + 1);
        assertEq(schedule.turnOf(turn + 1).candidate, carol);
        _lapse();
        schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, bob);
    }

    function test_AReleaseMoveAppendsARevisionAndErasesNone() public {
        _startTurn();
        (, OperatorSchedule.Revision[] memory drawn) = _assignment(turn, 0);
        _moveRelease(alice, 70);
        _moveRelease(alice, 71);
        (, OperatorSchedule.Revision[] memory revisions) = _assignment(turn, 0);
        assertEq(revisions.length, 3);
        assertEq(revisions[0].release, drawn[0].release);
        assertEq(revisions[0].generation, drawn[0].generation);
        assertEq(revisions[1].release, _release(70));
        assertEq(revisions[2].release, _release(71));
        assertGt(revisions[1].generation, revisions[0].generation);
        assertGt(revisions[2].generation, revisions[1].generation);
        assertEq(schedule.turnOf(turn).generation, revisions[2].generation);

        // A promoted member's move revises its own assignment, not the superseded one.
        _lapse();
        (, uint16 position) = schedule.promote();
        address promoted = schedule.turnOf(turn).candidate;
        _moveRelease(promoted, 72);
        (, OperatorSchedule.Revision[] memory own) = _assignment(turn, position);
        assertEq(own.length, 2);
        assertEq(own[1].release, _release(72));
        (, revisions) = _assignment(turn, 0);
        assertEq(revisions.length, 3);
    }

    function test_ANextTurnDrawnBeforeAPromotionFollowsTheKeysItMoved() public {
        // The outgoing member drawn again: its continued generation names keys it no longer holds.
        _startTurn();
        _draw(turn + 1);
        OperatorSchedule.Turn memory next = schedule.turnOf(turn + 1);
        assertEq(next.candidate, alice);
        assertEq(next.generation, schedule.turnOf(turn).generation, "a repeat turn continues its holder's");
        _lapse();
        schedule.promote();
        uint64 promoted = schedule.turnOf(turn).generation;
        OperatorSchedule.Turn memory moved = schedule.turnOf(turn + 1);
        assertGt(moved.generation, promoted, "the outgoing member's next turn takes a new generation");
        (, OperatorSchedule.Revision[] memory revisions) = _assignment(turn + 1, next.position);
        assertEq(revisions.length, 2, "recorded as a revision, erasing none");
        assertEq(revisions[0].generation, next.generation);
        assertEq(revisions[1].generation, moved.generation);
    }

    function test_ThePromotedMembersNextTurnContinuesThePromotionsGeneration() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, bob);
        _startTurn(order, bob);
        _draw(turn + 1);
        OperatorSchedule.Turn memory next = schedule.turnOf(turn + 1);
        assertEq(next.candidate, bob);
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, bob, "the order names bob next");
        assertEq(position, 1);
        uint64 promoted = schedule.turnOf(turn).generation;
        assertEq(schedule.turnOf(turn + 1).generation, promoted, "bob's next turn continues as a repeat turn");
        (, OperatorSchedule.Revision[] memory revisions) = _assignment(turn + 1, next.position);
        assertEq(revisions[revisions.length - 1].generation, promoted);
        assertEq(revisions[revisions.length - 1].release, registry.candidateOf(bob).release);
    }

    function test_ThePromotedMembersFirstHeadLatchesItsAssignment() public {
        _startTurn();
        _beat();
        _lapse();
        (, uint16 position) = schedule.promote();
        address promoted = schedule.turnOf(turn).candidate;
        vm.warp(block.timestamp + 20);
        LogHeads.HeadV2 memory head = _nextHead();
        assertEq(head.position, position);
        assertEq(head.parentSeq, 0, "a new signer epoch starts a new chain");
        _post(head);
        (uint64 at, uint64 epoch, address member,, uint64 generation) = logHeads.latches(turn, position);
        assertEq(at, block.timestamp);
        assertEq(epoch, 2);
        assertEq(member, promoted);
        assertEq(generation, schedule.turnOf(turn).generation);
        (uint64 drawnAt,, address drawnMember,,) = logHeads.latches(turn, 0);
        assertEq(drawnAt, schedule.boundary(turn), "the drawn assignment's latch stays");
        assertEq(drawnMember, alice);
        assertEq(schedule.promotionDueAt(), (_slot() + MISSES + 1) * INTERVAL);
    }
}
