// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vm} from "forge-std/Test.sol";

import {LogHeads} from "../src/LogHeads.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// `engine.log-head.v2` heartbeats while a turn names the operator: the version switch, live heads'
/// signing-time window, held heads' block binding, relays, and the latch of each turn's assignment.
contract LogHeadsV2Test is LivenessTest {
    uint256 private constant TOLERANCE_MS = 30_000;
    uint64 private constant HEAD_BLOCKS = 200;

    function setUp() public {
        _deployLiveness();
        _startTurn();
    }

    function _signV1(uint64 seq, bytes32 link, uint64 parentSeq, bytes32 parentLink)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.log-head.v1",
                uint64(block.chainid),
                address(vault),
                vault.signerEpoch(),
                seq,
                link,
                parentSeq,
                parentLink
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(_signerKey(), digest);
        return abi.encodePacked(r, s, v);
    }

    function _arbBlock() internal view returns (uint64) {
        return uint64(ARB.arbBlockNumber());
    }

    function _moveRelease(address candidate, uint256 seed_) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(candidate, _digest(seed_), CONFIG, true);
        vm.prank(candidate);
        registry.setRelease(_digest(seed_), CONFIG);
    }

    function _latch(uint64 at, uint16 position) internal view returns (LogHeads.Latch memory latch) {
        (latch.at, latch.signerEpoch, latch.member, latch.release, latch.generation) = logHeads.latches(at, position);
    }

    function _countLatches(Vm.Log[] memory logs) internal view returns (uint256 count) {
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].emitter == address(logHeads) && logs[i].topics[0] == LogHeads.TurnLatched.selector) ++count;
        }
    }

    function test_V1IsRefusedWhileATurnNamesTheOperator() public {
        bytes memory signature = _signV1(10, bytes32(uint256(10)), 0, bytes32(0));
        vm.expectRevert(LogHeads.UseV2.selector);
        logHeads.post(10, bytes32(uint256(10)), 0, bytes32(0), signature);
    }

    function test_V2IsRefusedWhileNoTurnNamesTheOperator() public {
        // The turn is drawn but its boundary has not come: the hourly v1 heads still apply.
        vm.warp(schedule.boundary(turn) - 1);
        assertEq(registry.operatorGeneration(), 0);
        LogHeads.HeadV2 memory head = _nextHead();
        bytes memory signature = _sign(head, _signerKey());
        vm.expectRevert(LogHeads.UseV1.selector);
        logHeads.postV2(head, signature);
        logHeads.post(10, bytes32(uint256(10)), 0, bytes32(0), _signV1(10, bytes32(uint256(10)), 0, bytes32(0)));
        LogHeads.Head memory held = _heldHead();
        assertEq(held.seq, 10);
        assertEq(held.signedAt, 0, "a v1 head carries no signing time");
        assertEq(held.postedAt, block.timestamp);
    }

    function test_AV2HeadExtendsTheEpochsV1Head() public {
        vm.warp(schedule.boundary(turn) - 1 hours);
        bytes32 link = keccak256("v1 link");
        logHeads.post(10, link, 0, bytes32(0), _signV1(10, link, 0, bytes32(0)));
        vm.warp(schedule.boundary(turn));
        LogHeads.HeadV2 memory head = _nextHead();
        (head.parentSeq, head.parentLink) = (10, keccak256("another link"));
        bytes memory signature = _sign(head, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.NotParent.selector, 1, 10, head.parentLink, 10, link));
        logHeads.postV2(head, signature);
        head = _nextHead();
        assertEq(head.parentSeq, 10);
        assertEq(head.parentLink, link);
        _post(head);
        assertEq(_heldHead().seq, 11);
    }

    function test_ALiveHeadRecordsItsSigningAndPostingTimes() public {
        vm.warp(block.timestamp + 7);
        LogHeads.HeadV2 memory head = _nextHead();
        vm.expectEmit(address(logHeads));
        emit LogHeads.HeadPostedV2(1, head.seq, head.link, turn, 0, 0, head.signedAt);
        _post(head);
        LogHeads.Head memory held = _heldHead();
        assertEq(held.seq, head.seq);
        assertEq(held.link, head.link);
        assertEq(held.signedAt, block.timestamp * 1000);
        assertEq(held.boundBlock, 0);
        assertEq(held.postedAt, block.timestamp);
    }

    function test_ALiveHeadSignedTooEarlyIsRefused() public {
        vm.warp(block.timestamp + 100);
        LogHeads.HeadV2 memory head = _nextHead();
        head.signedAt = uint64(block.timestamp * 1000 - TOLERANCE_MS - 1);
        bytes memory early = _sign(head, _signerKey());
        vm.expectRevert(
            abi.encodeWithSelector(LogHeads.SigningTimeOutsideTolerance.selector, head.signedAt, block.timestamp * 1000)
        );
        logHeads.postV2(head, early);
        head.signedAt += 1;
        _post(head);
        assertEq(_heldHead().signedAt, block.timestamp * 1000 - TOLERANCE_MS);
    }

    function test_ALiveHeadSignedTooLateIsRefused() public {
        LogHeads.HeadV2 memory head = _nextHead();
        head.signedAt = uint64(block.timestamp * 1000 + TOLERANCE_MS + 1);
        bytes memory late = _sign(head, _signerKey());
        vm.expectRevert(
            abi.encodeWithSelector(LogHeads.SigningTimeOutsideTolerance.selector, head.signedAt, block.timestamp * 1000)
        );
        logHeads.postV2(head, late);
        head.signedAt -= 1;
        _post(head);
        assertEq(_heldHead().signedAt, block.timestamp * 1000 + TOLERANCE_MS);
    }

    function test_ALiveHeadNotNewerInSigningTimeIsRefused() public {
        LogHeads.HeadV2 memory first = _beat();
        vm.warp(block.timestamp + 20);
        LogHeads.HeadV2 memory head = _nextHead();
        head.signedAt = first.signedAt;
        bytes memory same = _sign(head, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.SigningTimeNotNewer.selector, first.signedAt, first.signedAt));
        logHeads.postV2(head, same);
        head.signedAt = first.signedAt - 1;
        bytes memory older = _sign(head, _signerKey());
        vm.expectRevert(
            abi.encodeWithSelector(LogHeads.SigningTimeNotNewer.selector, first.signedAt - 1, first.signedAt)
        );
        logHeads.postV2(head, older);
        head.signedAt = first.signedAt + 1;
        _post(head);
    }

    function test_AHeldHeadWithAWrongHashIsRefused() public {
        LogHeads.HeadV2 memory head = _held(_nextHead(), 1, _arbBlock() - 10);
        head.blockHash = ARB.hashOf(_arbBlock() - 11);
        bytes memory signature = _sign(head, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.BlockHashMismatch.selector, head.blockNumber));
        logHeads.postV2(head, signature);
    }

    function test_AHeldHeadOutsideTheBlockWindowIsRefused() public {
        uint64 current = _arbBlock();
        uint64[3] memory outside = [current, current + 1, current - HEAD_BLOCKS - 1];
        for (uint256 i = 0; i < 3; ++i) {
            LogHeads.HeadV2 memory head = _held(_nextHead(), 1, outside[i]);
            bytes memory signature = _sign(head, _signerKey());
            vm.expectRevert(abi.encodeWithSelector(LogHeads.BlockOutsideWindow.selector, outside[i], current));
            logHeads.postV2(head, signature);
        }
        _post(_held(_nextHead(), 1, current - HEAD_BLOCKS));
        assertEq(_heldHead().boundBlock, current - HEAD_BLOCKS);
    }

    function test_AHeldHeadNotNewerThanTheBoundBlockIsRefused() public {
        uint64 bound = _arbBlock() - 5;
        _post(_held(_nextHead(), 1, bound));
        uint64[2] memory stale = [bound, bound - 1];
        for (uint256 i = 0; i < 2; ++i) {
            LogHeads.HeadV2 memory head = _held(_nextHead(), 1, stale[i]);
            bytes memory signature = _sign(head, _signerKey());
            vm.expectRevert(abi.encodeWithSelector(LogHeads.BlockNotNewer.selector, stale[i], bound));
            logHeads.postV2(head, signature);
        }
        _post(_held(_nextHead(), 1, bound + 1));
        assertEq(_heldHead().boundBlock, bound + 1);
    }

    function test_AHeldHeadOlderThanTheLatestMarkIsRefused() public {
        uint64 marked = _arbBlock();
        schedule.witness();
        ARB.setBlockNumber(marked + 10);
        LogHeads.HeadV2 memory head = _held(_nextHead(), 2, marked - 1);
        bytes memory signature = _sign(head, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.BlockBeforeMark.selector, marked - 1, marked));
        logHeads.postV2(head, signature);
        _post(_held(_nextHead(), 2, marked));
    }

    /// A host keeps a held head the enclave signed, lets a burst of blocks pass and then blocks come
    /// slowly while witnesses mark each slot: the cached head's block is still within `headBlocks`, but
    /// older than the block that marked the latest slot, so it cannot reset the count.
    function test_ACachedHeldHeadCannotResetACountWitnessesAdvance() public {
        uint64 bound = _arbBlock() - 1;
        LogHeads.HeadV2 memory cached = _held(_nextHead(), 1, bound);
        bytes memory signature = _sign(cached, _signerKey());
        ARB.setBlockNumber(_arbBlock() + 150);
        for (uint256 i = 0; i < 3; ++i) {
            _nextSlot(10, 1);
            schedule.witness();
        }
        (, uint64 marked) = schedule.latestMark();
        assertLe(_arbBlock() - bound, HEAD_BLOCKS, "the cached block is still in the window");
        vm.expectRevert(abi.encodeWithSelector(LogHeads.BlockBeforeMark.selector, bound, marked));
        logHeads.postV2(cached, signature);
    }

    function test_AHeldHeadMayRepeatTheSeqAndLinkItExtends() public {
        LogHeads.HeadV2 memory first = _beat();
        LogHeads.HeadV2 memory repeat = _held(_nextHead(), 1, _arbBlock() - 1);
        (repeat.seq, repeat.link) = (first.seq, first.link);
        vm.warp(block.timestamp + 30);
        _post(repeat);
        LogHeads.Head memory held = _heldHead();
        assertEq(held.seq, first.seq);
        assertEq(held.link, first.link);
        assertEq(held.boundBlock, _arbBlock() - 1);
        assertEq(held.postedAt, block.timestamp);

        // A live head may not repeat it, nor a held head repeat its seq under another link.
        LogHeads.HeadV2 memory live = _nextHead();
        (live.seq, live.link) = (first.seq, first.link);
        bytes memory liveSignature = _sign(live, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.NotNewer.selector, 1, first.seq, first.seq));
        logHeads.postV2(live, liveSignature);
        ARB.setBlockNumber(_arbBlock() + 1);
        LogHeads.HeadV2 memory fork = _held(_nextHead(), 1, _arbBlock() - 1);
        (fork.seq, fork.link) = (first.seq, keccak256("fork"));
        bytes memory forkSignature = _sign(fork, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.NotNewer.selector, 1, first.seq, first.seq));
        logHeads.postV2(fork, forkSignature);
    }

    function test_AHeldHeadIsNotHeldToTheSigningTimeWindow() public {
        _beat();
        // Through a trusted-time halt the enclave's lower bound on the time stays at its last sample.
        uint64 sampled = uint64(block.timestamp) * 1000;
        vm.warp(block.timestamp + 1 hours);
        LogHeads.HeadV2 memory head = _held(_nextHead(), 2, _arbBlock() - 1);
        head.signedAt = sampled;
        _post(head);
        assertEq(_heldHead().signedAt, sampled);
    }

    function test_AnUnknownHoldIsRefused() public {
        LogHeads.HeadV2 memory head = _held(_nextHead(), 3, _arbBlock() - 1);
        bytes memory signature = _sign(head, _signerKey());
        vm.expectRevert(abi.encodeWithSelector(LogHeads.InvalidHold.selector, 3));
        logHeads.postV2(head, signature);
    }

    function test_ARelayedHeadCountsForTheEnclaveThatSignedIt() public {
        _witnessFromNow(MISSES - 1);
        LogHeads.HeadV2 memory head = _nextHead();
        bytes memory signature = _sign(head, _signerKey());
        vm.prank(makeAddr("follower relaying the head"));
        logHeads.postV2(head, signature);
        assertEq(_heldHead().postedAt, block.timestamp);
        assertEq(schedule.promotionDueAt(), (_slot() + MISSES + 1) * INTERVAL, "the count restarts at the head");
    }

    function test_AForgedOrAlteredHeadIsRefused() public {
        LogHeads.HeadV2 memory head = _held(_nextHead(), 1, _arbBlock() - 1);
        bytes memory signature = _sign(head, _signerKey());
        bytes memory forged = _sign(head, privateKeyOf[_freshStandbyKey()]);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        logHeads.postV2(head, forged);
        for (uint256 field = 0; field < 14; ++field) {
            LogHeads.HeadV2 memory altered = _alter(head, field);
            vm.expectRevert(LogHeads.InvalidSigner.selector);
            logHeads.postV2(altered, signature);
        }
        logHeads.postV2(head, signature);
    }

    /// @dev `head` with field `field`, in declaration order, changed.
    function _alter(LogHeads.HeadV2 memory head, uint256 field) internal pure returns (LogHeads.HeadV2 memory a) {
        a = LogHeads.HeadV2(
            head.seq + (field == 0 ? 1 : 0),
            field == 1 ? keccak256("other") : head.link,
            head.parentSeq + (field == 2 ? 1 : 0),
            field == 3 ? keccak256("other") : head.parentLink,
            head.turn + (field == 4 ? 1 : 0),
            head.position + (field == 5 ? 1 : 0),
            field == 6 ? keccak256("other") : head.release,
            head.generation + (field == 7 ? 1 : 0),
            head.stamp + (field == 8 ? 1 : 0),
            head.signedAt + (field == 9 ? 1 : 0),
            field == 10 ? 2 : head.hold,
            field == 11 ? address(0xbeef) : head.confirmer,
            head.blockNumber + (field == 12 ? 1 : 0),
            field == 13 ? keccak256("other") : head.blockHash
        );
    }

    /// The fixed known-answer vector the node's tests replay: key 0x1234, chain 31337, vault
    /// 0x1111…11, epoch 7, seq 42, link 0x…2a, parent (41, 0x…29), turn 20765, position 3, release
    /// 0x…72, generation 9, stamp 1794123456000, signed at 1794123456789, hold 1, confirmer 0x2222…22,
    /// Arbitrum block 300000000 with hash 0x…b10c. Both sides must produce this signature.
    function test_LogHeadV2SignatureVector() public pure {
        bytes memory preimage = bytes.concat(
            abi.encodePacked(
                "engine.log-head.v2",
                uint64(31337),
                address(0x1111111111111111111111111111111111111111),
                uint64(7),
                uint64(42),
                bytes32(uint256(0x2a)),
                uint64(41),
                bytes32(uint256(0x29)),
                uint64(20765)
            ),
            abi.encodePacked(
                uint16(3),
                bytes32(uint256(0x72)),
                uint64(9),
                uint64(1794123456000),
                uint64(1794123456789),
                uint8(1),
                address(0x2222222222222222222222222222222222222222),
                uint64(300000000),
                bytes32(uint256(0xb10c))
            )
        );
        assertEq(preimage.length, 261);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(0x1234, keccak256(preimage));
        assertEq(r, bytes32(0xb1e55c9eee77638ce7547af58836fd069aacffd71ebc73b46a128b33b4fe0265));
        assertEq(s, bytes32(0x77009055603c1d79cc1952e5a78119e2cab2c3d4ed30000581b9627694eb2357));
        assertEq(v, 28);
    }

    function test_TheFirstHeadLatchesTheTurnsAssignment() public {
        vm.warp(block.timestamp + 12);
        LogHeads.HeadV2 memory head = _nextHead();
        uint64 generation = schedule.turnOf(turn).generation;
        bytes32 release = registry.candidateOf(alice).release;
        vm.expectEmit(address(logHeads));
        emit LogHeads.TurnLatched(turn, 0, alice, 1, release, generation);
        _post(head);
        LogHeads.Latch memory latch = _latch(turn, 0);
        assertEq(latch.at, block.timestamp);
        assertEq(latch.signerEpoch, 1);
        assertEq(latch.member, alice);
        assertEq(latch.release, release);
        assertEq(latch.generation, generation);
    }

    function test_LaterHeadsDoNotEraseTheLatch() public {
        _beat();
        LogHeads.Latch memory first = _latch(turn, 0);
        vm.warp(block.timestamp + 30);
        vm.recordLogs();
        _beat();
        _post(_held(_nextHead(), 1, _arbBlock() - 1));
        assertEq(_countLatches(vm.getRecordedLogs()), 0);
        LogHeads.Latch memory latch = _latch(turn, 0);
        assertEq(latch.at, first.at);
        assertEq(latch.generation, first.generation);
    }

    function test_AHeadOfASupersededAssignmentLatchesNothing() public {
        LogHeads.HeadV2 memory stale = _nextHead();
        bytes memory signature = _sign(stale, _signerKey());
        // Alice moves her release before her first head lands: the turn takes a new generation.
        _moveRelease(alice, 50);
        uint64 moved = schedule.turnOf(turn).generation;
        assertTrue(moved != stale.generation);
        vm.recordLogs();
        logHeads.postV2(stale, signature);
        assertEq(_countLatches(vm.getRecordedLogs()), 0, "the head is a heartbeat but latches nothing");
        assertEq(_heldHead().postedAt, block.timestamp);
        assertEq(_latch(turn, 0).at, 0);

        // A head naming a turn that is not the one in effect latches nothing either.
        LogHeads.HeadV2 memory early = _nextHead();
        early.turn = turn + 1;
        _post(early);
        assertEq(_latch(turn + 1, 0).at, 0);
        assertEq(_latch(turn, 0).at, 0);

        _beat();
        LogHeads.Latch memory latch = _latch(turn, 0);
        assertEq(latch.generation, moved);
        assertEq(latch.release, _release(50));
    }

    /// Alice moves away and back to her release: the turn's generation moves twice, and a head signed
    /// under the first generation, on the same release, latches nothing.
    function test_AHeadOfAnEarlierGenerationOnTheSameReleaseLatchesNothing() public {
        LogHeads.HeadV2 memory stale = _nextHead();
        bytes memory signature = _sign(stale, _signerKey());
        _moveRelease(alice, 50);
        _moveRelease(alice, 1);
        assertEq(registry.candidateOf(alice).release, stale.release);
        assertTrue(schedule.turnOf(turn).generation != stale.generation);
        logHeads.postV2(stale, signature);
        assertEq(_latch(turn, 0).at, 0);
        _beat();
        assertEq(_latch(turn, 0).generation, schedule.turnOf(turn).generation);
    }

    function test_AHeldFirstHeadLatches() public {
        _post(_held(_nextHead(), 2, _arbBlock() - 1));
        LogHeads.Latch memory latch = _latch(turn, 0);
        assertEq(latch.at, block.timestamp);
        assertEq(latch.member, alice);
    }

    /// Alice holds two turns in a row with no skip or promotion: each turn's assignment latches, the
    /// second from a held first head.
    function test_ConsecutiveTurnsOfOneMemberEachLatch() public {
        _beat();
        assertEq(_draw(turn + 1), alice);
        uint64 generation = schedule.turnOf(turn).generation;
        assertEq(schedule.turnOf(turn + 1).generation, generation, "a repeat turn keeps its generation");
        vm.warp(schedule.boundary(turn + 1) + 5);
        ARB.setBlockNumber(_arbBlock() + 345_600);
        _post(_held(_nextHead(), 1, _arbBlock() - 1));
        LogHeads.Latch memory second = _latch(turn + 1, 0);
        assertEq(second.at, block.timestamp);
        assertEq(second.member, alice);
        assertEq(second.generation, generation);
        LogHeads.Latch memory first = _latch(turn, 0);
        assertEq(first.at, schedule.boundary(turn));
        assertEq(first.member, alice);
    }
}
