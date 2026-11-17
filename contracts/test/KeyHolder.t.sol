// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {SkipTest} from "./Skip.t.sol";

/// The key holder at a turn's boundary, whose enclave holds the keys of an earlier turn, against the
/// turn's skips and promotions at the defaults: 60 s slots, five missed heartbeats, a take window of two
/// slots. Each poll does what every member's liveness caller does (`liveness/caller.rs`): it witnesses
/// the slot, promotes once `promotionDueAt` has passed and skips once `skipDueAt` has passed.
contract KeyHolderTest is SkipTest {
    /// @dev The assignment alice's enclave holds the keys under, which its heads bind while the turn in
    /// effect names another member.
    struct Binding {
        uint64 turn;
        uint16 position;
        bytes32 release;
        uint64 generation;
    }

    Binding internal holding;
    /// @dev When set, the member the turns drawn for bob name at position 1.
    address internal second;

    function setUp() public {
        _deployLiveness();
    }

    /// @dev Adopts the first `count` of alice, bob, carol and dave (40:30:20:10), draws a turn for alice,
    /// whose enclave takes it with its first head, and goes to the boundary of the next turn, drawn for
    /// bob, with alice's enclave beating until five seconds before it.
    function _aliceHoldsTheKeys(uint256 count) internal {
        address[4] memory all = [alice, bob, carol, dave];
        uint256[4] memory weights = [uint256(40_000e18), 30_000e18, 20_000e18, 10_000e18];
        address[] memory members = new address[](count);
        uint256[] memory stakes = new uint256[](count);
        for (uint256 i = 0; i < count; ++i) {
            (members[i], stakes[i]) = (all[i], weights[i]);
        }
        setId = _rotate(members, stakes);
        turn = _nextTurn() + 1;
        seed = _relayPicking(turn, setId, alice);
        if (second == address(0)) {
            _relayPicking(turn + 1, setId, bob);
        } else {
            address[] memory order = new address[](2);
            (order[0], order[1]) = (bob, second);
            _relayOrder(turn + 1, order);
        }
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _beat();
        (,, address latched,,) = logHeads.latches(turn, 0);
        assertEq(latched, alice, "alice took her turn");
        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        holding = Binding(turn, current.position, registry.candidateOf(alice).release, current.generation);
        _draw(turn + 1);
        turn += 1;
        seed = schedule.turnOf(turn).seed;
        assertEq(schedule.turnOf(turn).candidate, bob, "the turn is drawn for bob");
        vm.warp(schedule.boundary(turn) - 5);
        ARB.setBlockNumber(ARB.arbBlockNumber() + 240);
        _aliceBeats();
        vm.warp(schedule.boundary(turn));
    }

    /// @dev A live head from alice's enclave, binding the assignment it holds the keys under and its own
    /// release, signed by the vault's signer.
    function _aliceBeats() internal {
        LogHeads.HeadV2 memory head = _nextHead();
        (head.turn, head.position, head.release, head.generation) =
        (holding.turn, holding.position, holding.release, holding.generation);
        _post(head);
    }

    /// @dev One poll of a member's liveness caller; a call the chain would refuse is not sent.
    function _poll() internal {
        schedule.witness();
        if (block.timestamp >= schedule.promotionDueAt()) {
            try schedule.promote() {} catch {}
        }
        if (block.timestamp >= schedule.skipDueAt()) {
            try schedule.skip() {} catch {}
        }
    }

    /// @dev The next slot: the callers poll five seconds in and, while `beating` and its key is still the
    /// signer, alice's enclave posts a head at thirty.
    function _slotOn(bool beating) internal {
        uint64 epoch = vault.signerEpoch();
        _nextSlot(5, 240);
        _poll();
        if (beating && vault.signerEpoch() == epoch) {
            vm.warp(block.timestamp + 25);
            _aliceBeats();
        }
    }

    /// @dev Polls slot after slot until the signer epoch moves on, at most `slots` of them.
    function _untilPromoted(bool beating, uint256 slots) internal {
        uint64 epoch = vault.signerEpoch();
        for (uint256 i = 0; i < slots && vault.signerEpoch() == epoch; ++i) {
            _slotOn(beating);
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
    }

    /// @dev Asserts the turn's member is one of `a`, `b` and `c`, installed on its own standby key.
    function _promotedOneOf(address a, address b, address c, address key) internal view returns (address member) {
        member = schedule.turnOf(turn).candidate;
        assertTrue(member == a || member == b || member == c, "a member other than the key holder");
        assertEq(vault.checkpointSigner(), key, "on its own standby key");
    }

    /// Four members. Alice's host refuses every take of bob's turn while her enclave beats on, binding
    /// her earlier assignment. The callers skip bob to carol and carol to dave, which uses the order up,
    /// and the untaken-turn promotion seven slots after the boundary names one of them, not alice's own
    /// standby key: refusing the handover does not win the turn. When that member stays silent too, the
    /// next promotion again names one of them before alice, whom the first moved past.
    function test_AKeyHolderRefusingEverySuccessorDoesNotTakeTheTurnItWithheld() public {
        _aliceHoldsTheKeys(4);
        address[4] memory keys = [
            schedule.promotable(alice), schedule.promotable(bob), schedule.promotable(carol), schedule.promotable(dave)
        ];
        uint64 boundarySlot = _slot();
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn(true);
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        assertEq(_slot(), boundarySlot + 7, "at heartbeatMisses + 2 slots after the boundary");
        assertTrue(schedule.heldTurn(turn, bob) && schedule.heldTurn(turn, carol) && schedule.heldTurn(turn, dave));
        address first = schedule.turnOf(turn).candidate;
        address key = first == bob ? keys[1] : first == carol ? keys[2] : keys[3];
        _promotedOneOf(bob, carol, dave, key);
        assertTrue(vault.checkpointSigner() != keys[0], "not alice's standby key");
        assertTrue(schedule.heldKeys(turn, alice) && schedule.heldKeys(turn, first));
        assertFalse(schedule.heldTurn(turn, alice), "alice held the keys, not an assignment");

        _untilPromoted(false, MISSES + 2);
        address next = schedule.turnOf(turn).candidate;
        assertTrue(next != first && next != alice, "a member the key holder kept out, before the holder");
        assertTrue(next == bob || next == carol || next == dave);
    }

    /// Three members, nobody malicious: alice's enclave, holding the keys, stops for good five seconds
    /// before the boundary of bob's turn. No one can take the keys from a stopped holder, so the skip of
    /// bob to carol three slots in is spent; five slots in, the missed-head promotion names bob or carol,
    /// listed, running and holding promotable standby keys, not the stopped holder's standby key, and the
    /// turn has an operator again one detection window after the stop.
    function test_AStoppedKeyHolderAtABoundaryLeavesALiveMemberToPromote() public {
        _aliceHoldsTheKeys(3);
        address aliceKey = schedule.promotable(alice);
        (address bobKey, address carolKey) = (schedule.promotable(bob), schedule.promotable(carol));
        uint64 boundarySlot = _slot();
        _poll();
        _untilPromoted(false, 14);
        assertEq(_slot(), boundarySlot + 5, "missed heads counted from alice's last head");
        assertTrue(schedule.heldTurn(turn, bob) && schedule.heldTurn(turn, carol), "the draw and the skip");
        address promoted = schedule.turnOf(turn).candidate;
        _promotedOneOf(bob, carol, bob, promoted == bob ? bobKey : carolKey);
        assertTrue(vault.checkpointSigner() != aliceKey);
    }

    /// The key holder is the candidate whose assignment recorded the release that signed the newest head,
    /// not whichever candidate registers that release now. Alice's release-1 enclave holds the keys and
    /// beats; after the boundary of bob's turn she moves her candidacy to another approved release, which
    /// withdraws release 1's approval, and the turn's order names her next. No skip names her.
    function test_ASkipNeverNamesTheMemberWhoseEnclaveHoldsTheKeys() public {
        second = alice;
        _aliceHoldsTheKeys(4);
        assertEq(schedule.memberAt(setId, seed, turn, 1), alice);
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(77), CONFIG, true);
        vm.prank(alice);
        registry.setRelease(_digest(77), CONFIG);
        assertTrue(registry.candidateOf(alice).release != holding.release);
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        assertEq(logHeads.headRelease(vault.signerEpoch()), holding.release, "the keys are on alice's release 1");
        assertEq(schedule.releaseCandidate(holding.release), alice);
        for (uint256 slots = 0; slots < 6; ++slots) {
            _slotOn(true);
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the key holder");
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped, past alice");
    }

    /// Before the first head of a promotion's signer epoch, the promoted member holds the keys. Carol is
    /// promoted in alice's turn and posts no head; the next turn's order names dave, then carol, then bob,
    /// and its skip passes over carol, whose standby key is the signer, to bob.
    function test_ASilentPromotedMemberIsTheKeyHolderOfTheNextTurn() public {
        address[] memory members = new address[](4);
        uint256[] memory stakes = new uint256[](4);
        (members[0], members[1], members[2], members[3]) = (alice, bob, carol, dave);
        (stakes[0], stakes[1], stakes[2], stakes[3]) = (40_000e18, 30_000e18, 20_000e18, 10_000e18);
        setId = _rotate(members, stakes);
        address[] memory order = new address[](2);
        (order[0], order[1]) = (alice, carol);
        address[] memory next = new address[](3);
        (next[0], next[1], next[2]) = (dave, carol, bob);
        turn = _nextTurn() + 1;
        seed = _relayOrder(turn, order);
        bytes32 nextSeed = _relayOrder(turn + 1, next);
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _lapse();
        schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, carol);
        assertEq(logHeads.headRelease(vault.signerEpoch()), bytes32(0), "carol posts no head");
        // No witness marks a slot until the next turn, so no count passes before it.
        _draw(turn + 1);
        (turn, seed) = (turn + 1, nextSeed);
        vm.warp(schedule.boundary(turn));
        _marked(TAKE);
        assertLe(schedule.skipDueAt(), block.timestamp);
        assertGt(schedule.promotionDueAt(), block.timestamp, "the skip comes first");
        (, uint16 position) = schedule.skip();
        assertEq(position, 2, "carol, at position 1, is passed over");
        assertEq(schedule.turnOf(turn).candidate, bob);
    }
}
