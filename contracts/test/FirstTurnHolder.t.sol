// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {Vault} from "../src/Vault.sol";
import {SkipTest} from "./Skip.t.sol";

/// The operator the registry names before the first turn's boundary holds the keys under no assignment of
/// the schedule. The timelock installs alice, a registered candidate, as that operator, rotation turns on, a
/// close adopts her with bob, carol and dave, and the first turn is drawn for another member with alice as
/// its source. The polls are every member's liveness caller's (`liveness/caller.rs`), at the defaults: 60 s
/// slots, five missed heartbeats, a take window of two slots.
abstract contract FirstTurnTest is SkipTest {
    /// @dev The release alice's enclave holds the keys on, which its heads name while they bind no
    /// assignment (`log_heads/v2.rs`: the sealed keys' release).
    bytes32 internal holderRelease;

    /// @dev When set, alice's host drops every head her enclave signs: none reaches the chain.
    bool internal silent;

    /// @dev When not empty, the order the first positions of the turn after the first pick.
    address[] internal nextOrder;

    function setUp() public {
        _deployLiveness();
    }

    /// @dev Registers `_members()` with their stakes, has the timelock install alice as the operator in
    /// effect on the release she registered, turns rotation on and adopts the set, then draws the first
    /// turn with a seed from `_seedFor(order)` while the registry names alice, so she is its source. When
    /// `moveBeforeDraw` or `moveAfterDraw`, alice moves her candidacy to another release the timelock
    /// approved for her, which the vault names at once, and registers a standby key for it.
    function _aliceIsTheSource(address[] memory order, bool moveBeforeDraw, bool moveAfterDraw) internal {
        (address[] memory all, uint256[] memory weights) = _members();
        for (uint256 i = 0; i < all.length; ++i) {
            _candidate(all[i], weights[i], i + 1);
            _delegate(all[i], all[i]);
        }
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (alice, _key(alice), _digest(1), CONFIG))
        );
        assertEq(registry.operatorInEffect().candidate, alice);
        _setRotation(true);
        _closeNext();
        setId = schedule.pendingSetId();
        (, uint64 effectiveAt,,) = schedule.setOf(setId);
        vm.warp(effectiveAt);
        turn = _nextTurn() + 1;
        seed = _seedFor(turn, order);
        if (nextOrder.length != 0) _relayOrder(turn + 1, nextOrder);
        if (moveBeforeDraw) _aliceUpgrades();
        _draw(turn);
        assertTrue(schedule.turnOf(turn).candidate != alice, "the first turn is drawn for another member");
        assertEq(schedule.sourceOf(turn), alice, "alice, the operator in effect, is its source");
        if (moveAfterDraw) _aliceUpgrades();
        holderRelease = registry.candidateOf(alice).release;
        assertEq(registry.operatorRelease(), holderRelease, "the vault names the release alice runs");
        assertTrue(schedule.promotable(alice) != address(0), "alice holds a promotable standby key");
        vm.warp(schedule.boundary(turn));
    }

    /// @dev The candidates and their stakes: alice, bob, carol and dave, 40:30:20:10.
    function _members() internal virtual returns (address[] memory all, uint256[] memory weights) {
        all = new address[](4);
        weights = new uint256[](4);
        (all[0], all[1], all[2], all[3]) = (alice, bob, carol, dave);
        (weights[0], weights[1], weights[2], weights[3]) = (40_000e18, 30_000e18, 20_000e18, 10_000e18);
    }

    /// @dev Relays for `at` a seed whose first positions pick `order`.
    function _seedFor(uint64 at, address[] memory order) internal virtual returns (bytes32) {
        return _relayOrder(at, order);
    }

    /// @dev Alice's upgrade: the timelock approves release 77 for her, she moves to it, and registers a
    /// standby key for it.
    function _aliceUpgrades() internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(77), CONFIG, true);
        vm.prank(alice);
        registry.setRelease(_digest(77), CONFIG);
        _registerStandby(alice);
    }

    /// @dev A live head from alice's enclave: it holds the keys under no assignment the schedule names,
    /// so the head binds none and names the release its keys are sealed in.
    function _aliceBeats() internal {
        LogHeads.HeadV2 memory head = _nextHead();
        (head.turn, head.position, head.release, head.generation) = (0, 0, holderRelease, 0);
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

    /// @dev The next slot: the callers poll five seconds in and, unless `silent` and while its key is
    /// still the signer, alice's enclave posts a head at thirty.
    function _slotOn() internal {
        uint64 epoch = vault.signerEpoch();
        _nextSlot(5, 240);
        _poll();
        if (!silent && vault.signerEpoch() == epoch) {
            vm.warp(block.timestamp + 25);
            _aliceBeats();
        }
    }

    /// @dev The key holder `LogHeads` names now.
    function _holderNow() internal view returns (address holder) {
        (holder,) = logHeads.keyHolder();
    }

    /// @dev Alice's enclave beats from the boundary, refusing every take, until a promotion moves the
    /// signer, at most twelve slots.
    function _aliceRefusesUntilPromoted() internal {
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
    }
}

/// The first turn's source is the key holder: before the signer epoch's first head, while no member has
/// taken the turn and no promotion has moved its keys; and through any release move, which records its new
/// release for it. A skip never names it, and a promotion names it only after every member that never held
/// the turn's keys.
contract FirstTurnHolderTest is FirstTurnTest {
    /// The order names alice after bob. Alice upgrades after the draw and refuses bob's take: no skip
    /// names her, since her enclave holds the keys on the release her move recorded.
    function test_ASkipNeverNamesTheSourceThatUpgradedAfterTheDraw() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        _aliceIsTheSource(order, false, true);
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        assertEq(logHeads.headRelease(vault.signerEpoch()), holderRelease, "alice's enclave signs the heads");
        for (uint256 slots = 0; slots < 6; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the member whose enclave holds the keys");
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped, past alice");
    }

    /// The order names bob, carol, dave, then alice. Alice upgrades after the draw and refuses every take.
    /// The skips use carol and dave up, and the untaken-turn promotion seven slots after the boundary names
    /// one of the members she kept out, on its own standby key, not alice's.
    function test_ASourceThatUpgradedAfterTheDrawIsNotPromotedIntoTheTurnItWithheld() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (bob, carol, dave, alice);
        _aliceIsTheSource(order, false, true);
        address aliceKey = schedule.promotable(alice);
        _aliceRefusesUntilPromoted();
        address member = schedule.turnOf(turn).candidate;
        assertTrue(member == bob || member == carol || member == dave, "a member the source kept out");
        assertTrue(vault.checkpointSigner() != aliceKey, "not the source's own standby key");
        assertTrue(schedule.heldKeys(turn, alice), "the promotion moved past alice as the key holder");
    }

    /// The same refusal with no upgrade: the draw recorded alice's release for her, the skips pass her
    /// over and the promotion names one of the members she kept out.
    function test_ASourceOnTheReleaseTheDrawRecordedIsPassedOver() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        _aliceIsTheSource(order, false, false);
        address aliceKey = schedule.promotable(alice);
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        assertEq(schedule.releaseCandidate(holderRelease), alice);
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the key holder");
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        assertTrue(vault.checkpointSigner() != aliceKey);
    }

    /// The same upgrade before the draw: the draw records the new release for alice, and she is passed
    /// over.
    function test_ASourceThatUpgradedBeforeTheDrawIsPassedOver() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        _aliceIsTheSource(order, true, false);
        address aliceKey = schedule.promotable(alice);
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        assertEq(schedule.releaseCandidate(holderRelease), alice);
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the key holder");
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        assertTrue(vault.checkpointSigner() != aliceKey);
    }

    /// No upgrade, and alice's host drops every head and refuses bob's take. Her signer epoch was started
    /// by no promotion and holds no head, and v2 heads are taken only from the first boundary, yet the
    /// skip three slots in passes her over: as the turn's source, she holds the keys.
    function test_ASkipNeverNamesTheFirstTurnsSourceBeforeItsFirstHead() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        _aliceIsTheSource(order, false, false);
        silent = true;
        _poll();
        assertEq(logHeads.headRelease(vault.signerEpoch()), bytes32(0), "no v2 head in the epoch");
        for (uint256 slots = 0; slots < 4; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the member whose enclave holds the keys");
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped, past alice");
    }

    /// The same silence, with the order bob, carol, alice, dave. The skip three slots in names carol; the
    /// missed-head promotion five slots in passes alice, the source, and names another member on its own
    /// standby key, while bob, carol and dave are listed and hold promotable keys.
    function test_ASilentFirstTurnsSourceIsNotPromotedIntoTheTurnItWithheld() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (bob, carol, alice, dave);
        _aliceIsTheSource(order, false, false);
        silent = true;
        address aliceKey = schedule.promotable(alice);
        assertTrue(schedule.promotable(bob) != address(0) && schedule.promotable(dave) != address(0));
        _poll();
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        assertTrue(vault.checkpointSigner() != aliceKey, "not the source's own standby key");
        assertTrue(schedule.turnOf(turn).candidate != alice, "the source that refused every take took the turn");
        assertTrue(schedule.heldKeys(turn, alice), "the promotion moved past alice as the key holder");
    }

    /// Every release move records the new release for the candidate that moved, whether or not a turn
    /// names it: dave, in no turn, and alice, the source of a turn drawn for bob, before any boundary.
    function test_EveryReleaseMoveRecordsItsCandidate() public {
        address[] memory order = new address[](1);
        order[0] = bob;
        _aliceIsTheSource(order, false, false);
        address[2] memory movers = [dave, alice];
        for (uint256 i = 0; i < 2; ++i) {
            vm.prank(address(governance));
            registry.setReleaseApproval(movers[i], _digest(80 + i), CONFIG, true);
            assertEq(schedule.releaseCandidate(_release(80 + i)), address(0));
            vm.prank(movers[i]);
            registry.setRelease(_digest(80 + i), CONFIG);
            assertEq(schedule.releaseCandidate(_release(80 + i)), movers[i]);
        }
    }

    /// Bob takes the turn, so his enclave holds its keys. The guardian retires the signer and the timelock
    /// installs bob's registered enclave, in a new signer epoch with no head yet: bob is still the key
    /// holder. The missed-head promotion names alice, the next member, and moves past bob; when alice stays
    /// silent the next promotion passes bob at position 2, who held the turn's keys, and names dave.
    function test_AMemberThatTookTheTurnStaysTheKeyHolderUnderASignerTheTimelockInstalls() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (bob, alice, bob, dave);
        _aliceIsTheSource(order, false, false);
        _post(_nextHead());
        (uint64 latchedAt,,,,) = logHeads.latches(turn, 0);
        assertGt(latchedAt, 0, "bob took the turn");
        _newSignerEpochWithNoHead();
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(position, 1);
        assertEq(schedule.turnOf(turn).candidate, alice);
        assertTrue(schedule.heldKeys(turn, bob), "the promotion moved past bob as the key holder");
        _lapse();
        (, position) = schedule.promote();
        assertEq(position, 3, "bob, at position 2, held the turn's keys");
        assertEq(schedule.turnOf(turn).candidate, dave);
    }

    /// Bob takes the turn and stops; the missed-head promotion names carol, who stays silent. After the
    /// timelock installs carol's registered enclave in a new signer epoch with no head yet, carol is the key
    /// holder, not alice, the turn's source: the next promotion names alice.
    function test_APromotedMemberStaysTheKeyHolderUnderASignerTheTimelockInstalls() public {
        address[] memory order = new address[](3);
        (order[0], order[1], order[2]) = (bob, carol, alice);
        _aliceIsTheSource(order, false, false);
        _post(_nextHead());
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(position, 1);
        assertEq(schedule.turnOf(turn).candidate, carol, "the missed-head promotion names carol");
        assertFalse(schedule.heldKeys(turn, alice), "bob held the keys, not alice");
        _newSignerEpochWithNoHead();
        assertEq(_holderNow(), carol);
        _lapse();
        (, position) = schedule.promote();
        assertEq(position, 2);
        assertEq(schedule.turnOf(turn).candidate, alice, "alice is not taken for the key holder");
    }

    /// The same promotion of carol, with the next turn drawn before it for dave, with carol and then alice
    /// after him, so its source is bob. The timelock installs carol's registered enclave in a new signer
    /// epoch, which no head reaches, and nobody takes the next turn: carol still holds the keys, so its skip
    /// passes her, though she held nothing of it and is not its source, and names alice.
    function test_APromotedMemberHoldsTheKeysIntoTheNextTurn() public {
        address[] memory order = new address[](3);
        (order[0], order[1], order[2]) = (bob, carol, alice);
        nextOrder.push(dave);
        nextOrder.push(carol);
        nextOrder.push(alice);
        _aliceIsTheSource(order, false, false);
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), bob, "drawn while bob was named");
        _post(_nextHead());
        _lapse();
        schedule.promote();
        assertEq(schedule.turnOf(turn).candidate, carol, "the missed-head promotion names carol");
        _newSignerEpochWithNoHead();
        turn += 1;
        seed = schedule.turnOf(turn).seed;
        vm.warp(schedule.boundary(turn));
        assertEq(_holderNow(), carol);
        silent = true;
        for (uint256 slots = 0; slots < 4 && schedule.turnOf(turn).reason == 0; ++slots) {
            _slotOn();
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped");
        assertEq(schedule.turnOf(turn).candidate, alice, "the skip passes carol, who holds the keys");
    }

    /// The first turn is drawn for bob, with carol after him, and the next for dave, with bob next. Four
    /// slots before the first turn ends the timelock installs bob's registered enclave, which posts no head,
    /// and the skip passes bob to carol. The next turn begins with bob still holding the keys: neither its
    /// skip nor its promotion names him.
    function test_TheMemberTheTimelockInstalledHoldsTheKeysIntoTheNextTurn() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, carol);
        nextOrder.push(dave);
        nextOrder.push(bob);
        _aliceIsTheSource(order, false, false);
        _draw(turn + 1);
        vm.warp(schedule.boundary(turn + 1) - 4 * INTERVAL - 30);
        _newSignerEpochWithNoHead();
        silent = true;
        for (uint256 slots = 0; slots < 3 && schedule.turnOf(turn).reason == 0; ++slots) {
            _slotOn();
        }
        assertEq(schedule.turnOf(turn).candidate, carol, "the skip passes bob to carol");
        turn += 1;
        seed = schedule.turnOf(turn).seed;
        vm.warp(schedule.boundary(turn));
        assertEq(_holderNow(), bob, "bob still holds the keys");
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != bob, "a skip or promotion named the key holder");
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
    }

    /// Bob grows his drawn assignment's revisions by moving to the release he runs again and again, and the
    /// timelock installs his registered enclave before his take. The skip that passes him costs no more for
    /// the revisions: the key holder reads none of them.
    function test_ASkipPastAnInstalledMemberDoesNotReadItsRevisions() public {
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, carol);
        _aliceIsTheSource(order, false, false);
        for (uint256 i = 0; i < 300; ++i) {
            vm.prank(bob);
            registry.setRelease(_digest(2), CONFIG);
        }
        (, OperatorSchedule.Revision[] memory revisions) = schedule.assignmentOf(turn, 0);
        assertGt(revisions.length, 300);
        _newSignerEpochWithNoHead();
        _marked(TAKE);
        schedule.skip();
        uint256 skipGas = vm.lastCallGas().gasTotalUsed;
        assertEq(schedule.turnOf(turn).candidate, carol);
        assertLt(skipGas, 600_000, "the skip reads no revision");
    }

    /// The first turn is drawn for bob, with carol, bob and dave after him. The guardian retires the signer
    /// before bob takes it and the timelock installs bob's registered enclave, which posts no head: bob holds
    /// the keys. The skip passes him to carol, and the missed-head promotion passes bob at position 2, who
    /// withheld the keys under his own assignment, and names dave.
    function test_AMemberTheTimelockInstallsBeforeItsTakeIsNotPromotedBackIntoItsTurn() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (bob, carol, bob, dave);
        _aliceIsTheSource(order, false, false);
        _newSignerEpochWithNoHead();
        assertEq(vault.checkpointSigner(), registry.candidateOf(bob).enclave, "bob's registered enclave");
        assertEq(_holderNow(), bob);
        silent = true;
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
            if (vault.signerEpoch() == epoch && schedule.turnOf(turn).reason == 1) {
                assertEq(_holderNow(), bob, "the skip recorded bob as the key holder it passed");
                assertTrue(schedule.heldKeys(turn, bob), "the skip recorded bob as having held the keys");
            }
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        assertTrue(schedule.heldTurn(turn, carol), "the skip named carol");
        assertTrue(schedule.heldKeys(turn, bob), "the skip passed bob as the key holder");
        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        assertEq(current.position, 3, "bob, at position 2, withheld the keys");
        assertEq(current.candidate, dave);
    }

    /// Alice's host drops every head and no slot is witnessed through the first turn, drawn for bob, so it
    /// passes with no head, skip or promotion. The second turn, drawn for carol with alice next, names bob
    /// as its source, though bob never received the keys: alice still holds them, and its skip passes her.
    function test_TheFirstTurnsSourceStaysTheKeyHolderThroughAnUntakenTurn() public {
        address[] memory order = new address[](1);
        order[0] = bob;
        nextOrder.push(carol);
        nextOrder.push(alice);
        _aliceIsTheSource(order, false, false);
        silent = true;
        _draw(turn + 1);
        assertEq(schedule.sourceOf(turn + 1), bob, "the registry named bob at the draw");
        turn += 1;
        seed = schedule.turnOf(turn).seed;
        vm.warp(schedule.boundary(turn));
        address aliceKey = schedule.promotable(alice);
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the member whose enclave holds the keys");
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        assertTrue(vault.checkpointSigner() != aliceKey, "not the source's own standby key");
    }

    /// A turn is drawn for alice, who misses it; the skip names bob, who takes it, and the turns end. The
    /// timelock installs alice as the operator with a new signer, and the next turn, drawn for bob with alice
    /// next, names alice as its source. Alice's host drops every head. The turn before is bob's, taken, and
    /// alice held an assignment of it, yet she, not bob, holds the keys: the registry named her on its own
    /// at the draw. The skip passes her.
    function test_TheInstalledOperatorIsTheKeyHolderOfTheFirstTurnAfterItsInstallation() public {
        (address[] memory all, uint256[] memory weights) = _members();
        setId = _rotate(all, weights);
        uint64 taken = _nextTurn() + 1;
        address[] memory first = new address[](2);
        (first[0], first[1]) = (alice, bob);
        _relayOrder(taken, first);
        _draw(taken);
        vm.warp(schedule.boundary(taken));
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 4 && schedule.turnOf(taken).candidate != bob; ++slots) {
            _nextSlot(5, 240);
            schedule.witness();
            if (block.timestamp >= schedule.skipDueAt()) schedule.skip();
        }
        assertEq(schedule.turnOf(taken).candidate, bob, "the skip names bob");
        assertTrue(schedule.heldTurn(taken, alice), "alice held an assignment of the turn before");
        _post(_nextHead());
        assertEq(vault.signerEpoch(), epoch);
        // Each governance step passes the timelock's delay: bob's turn has ended by the installation.
        _setRotation(false);
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (alice, _key(alice), _digest(1), CONFIG))
        );
        vm.prank(address(governance));
        vault.setSigner(makeAddr("installed enclave"));
        _setRotation(true);
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        turn = _nextTurn() + 1;
        seed = _relayOrder(turn, order);
        _draw(turn);
        assertEq(schedule.sourceOf(turn), alice);
        assertEq(schedule.turnOf(turn).prior, taken);
        vm.warp(schedule.boundary(turn));
        silent = true;
        for (uint256 slots = 0; slots < 4; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the member whose enclave holds the keys");
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped, past alice");
    }

    /// Bob takes a turn and the turns end; the timelock installs alice as the operator and she takes the
    /// signer epoch's keys from bob's enclave, so the epoch's newest head is still bob's. The next turn, drawn
    /// for bob with alice next, names alice as its source, and her host drops every head: that head, from
    /// before the turn began, names no key holder, so alice holds the keys and the skip passes her.
    function test_TheInstalledOperatorThatKeepsTheSignerEpochIsTheKeyHolder() public {
        (address[] memory all, uint256[] memory weights) = _members();
        setId = _rotate(all, weights);
        uint64 taken = _nextTurn() + 1;
        address[] memory first = new address[](1);
        first[0] = bob;
        _relayOrder(taken, first);
        _draw(taken);
        vm.warp(schedule.boundary(taken));
        _post(_nextHead());
        uint64 epoch = vault.signerEpoch();
        bytes32 bobRelease = registry.candidateOf(bob).release;
        assertEq(logHeads.headRelease(epoch), bobRelease);
        // Each governance step passes the timelock's delay: bob's turn has ended by the installation.
        _setRotation(false);
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (alice, _key(alice), _digest(1), CONFIG))
        );
        _setRotation(true);
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        turn = _nextTurn() + 1;
        seed = _relayOrder(turn, order);
        _draw(turn);
        assertEq(schedule.sourceOf(turn), alice);
        assertEq(schedule.registryTurn(), turn, "drawn while the registry named its own operator");
        vm.warp(schedule.boundary(turn));
        assertEq(vault.signerEpoch(), epoch, "the signer epoch stays");
        assertEq(logHeads.headRelease(epoch), bobRelease, "the epoch's newest head is still bob's");
        assertEq(_holderNow(), alice);
        silent = true;
        for (uint256 slots = 0; slots < 4; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the member whose enclave holds the keys");
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped, past alice");
    }

    /// Alice holds the keys of her turn and is drawn for the next, with bob, alice and carol after her. Her
    /// heads still bind her earlier assignment, so the skip passes her, holding the keys, to bob, and records
    /// her as having held them. She hands them to bob, who takes the turn and stops: the missed-head
    /// promotion passes alice at position 2, who withheld the keys under her own assignment, and names carol.
    function test_ASkippedMemberThatHeldTheKeysIsNotPromotedOnceTheyMoveOn() public {
        (address[] memory all, uint256[] memory weights) = _members();
        setId = _rotate(all, weights);
        uint64 earlier = _nextTurn() + 1;
        address[] memory first = new address[](1);
        first[0] = alice;
        _relayOrder(earlier, first);
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (alice, bob, alice, carol);
        turn = earlier + 1;
        seed = _relayOrder(turn, order);
        _draw(earlier);
        vm.warp(schedule.boundary(earlier));
        LogHeads.HeadV2 memory taken = _nextHead();
        _post(taken);
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        LogHeads.HeadV2 memory head = _nextHead();
        (head.turn, head.position, head.release, head.generation) = (earlier, 0, taken.release, taken.generation);
        _post(head);
        assertEq(_holderNow(), alice);
        _marked(TAKE);
        schedule.skip();
        assertEq(schedule.turnOf(turn).candidate, bob);
        assertTrue(schedule.heldKeys(turn, alice), "the skip recorded alice as having held the keys");
        _post(_nextHead());
        assertEq(_holderNow(), bob, "bob took the turn with alice's keys");
        _lapse();
        (, uint16 position) = schedule.promote();
        assertEq(position, 3, "alice, at position 2, withheld the keys");
        assertEq(schedule.turnOf(turn).candidate, carol);
    }

    /// Bob takes a turn, and his host keeps back a held head his enclave signs then. The turns end, the
    /// timelock installs alice, who takes the signer epoch's keys, and the next turn is drawn for bob with
    /// alice next. Across a paused chain, bob's head is relayed after that turn's boundary: signed before it,
    /// it names no key holder, so alice still holds the keys and the skip passes her.
    function test_AHeadSignedBeforeTheRegistryTurnNamesNoKeyHolderHoweverLateItIsRelayed() public {
        (address[] memory all, uint256[] memory weights) = _members();
        setId = _rotate(all, weights);
        uint64 taken = _nextTurn() + 1;
        address[] memory first = new address[](1);
        first[0] = bob;
        _relayOrder(taken, first);
        _draw(taken);
        vm.warp(schedule.boundary(taken));
        _post(_nextHead());
        vm.warp(block.timestamp + 20);
        LogHeads.HeadV2 memory keptBack = _held(_nextHead(), 1, uint64(ARB.arbBlockNumber()));
        bytes memory signature = _sign(keptBack, _signerKey());
        _setRotation(false);
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (alice, _key(alice), _digest(1), CONFIG))
        );
        _setRotation(true);
        address[] memory order = new address[](2);
        (order[0], order[1]) = (bob, alice);
        turn = _nextTurn() + 1;
        seed = _relayOrder(turn, order);
        _draw(turn);
        vm.warp(schedule.boundary(turn) + 10);
        // The chain resumes: one block on from the one the head binds, and no slot witnessed since.
        ARB.setBlockNumber(ARB.arbBlockNumber() + 1);
        logHeads.postV2(keptBack, signature);
        (,,,, uint64 postedAt) = logHeads.heads(vault.signerEpoch());
        assertEq(postedAt, block.timestamp, "relayed after the turn's boundary");
        assertEq(_holderNow(), alice, "a head signed before the turn began names no holder");
        silent = true;
        for (uint256 slots = 0; slots < 4; ++slots) {
            _slotOn();
            assertTrue(schedule.turnOf(turn).candidate != alice, "a skip named the member whose enclave holds the keys");
        }
        assertEq(schedule.turnOf(turn).reason, 1, "skipped, past alice");
    }

    /// @dev The guardian retires the signer and the timelock installs the registered enclave of the
    /// operator the registry names, in a new signer epoch that no promotion started and no head has
    /// reached.
    function _newSignerEpochWithNoHead() internal {
        uint64 epoch = vault.signerEpoch();
        vm.prank(multisig);
        vault.retireSigner();
        address installed = registry.operatorEnclave();
        vm.prank(address(governance));
        vault.setSigner(installed);
        assertEq(vault.signerEpoch(), epoch + 1);
        assertEq(logHeads.headRelease(epoch + 1), bytes32(0));
    }
}
