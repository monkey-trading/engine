// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {FirstTurnTest} from "./FirstTurnHolder.t.sol";

/// Whom a promotion names in a turn whose key holder refuses every take. Five members: alice, the first
/// turn's source and the key holder, bob, carol and dave, and eve with a twentieth of the weight. Which
/// members the holder's refusals leave holding an assignment never moves a member ahead of another, and a
/// promotion's search reaches every member wherever the turn's position stands, so it never refuses while
/// a member qualifies and names the key holder only after every other qualified member.
contract PromotionOrderTest is FirstTurnTest {
    address internal eve = makeAddr("eve");
    /// @dev Eve's place in the scan `_seedFor` gives: 4 is its last.
    uint16 internal eveAt = 4;
    /// @dev When set, `_seedFor` with no order relays the first seed from this salt that does not draw alice.
    uint256 internal salt;
    bool internal salted;

    function _members() internal view override returns (address[] memory all, uint256[] memory weights) {
        all = new address[](5);
        weights = new uint256[](5);
        (all[0], all[1], all[2], all[3], all[4]) = (alice, bob, carol, dave, eve);
        (weights[0], weights[1], weights[2], weights[3], weights[4]) =
        (40_000e18, 30_000e18, 20_000e18, 10_000e18, 5_500e18);
    }

    /// @dev Given an order, a seed whose first positions pick it. With none: a salted seed, or else a seed
    /// whose position 0 picks bob, whose other weighted positions never pick eve, and whose scan has eve at
    /// `eveAt`.
    function _seedFor(uint64 at, address[] memory order) internal override returns (bytes32 found) {
        if (order.length != 0) return _relayOrder(at, order);
        uint16 weighted = schedule.MAX_DRAW_POSITIONS();
        for (uint256 i = 0;; ++i) {
            if (salted) {
                found = keccak256(abi.encode("salted seed", at, salt, i));
                if (schedule.memberAt(setId, found, at, 0) != alice) break;
                continue;
            }
            found = keccak256(abi.encode("scan seed", at, i));
            if (schedule.memberAt(setId, found, at, 0) != bob) continue;
            if (schedule.memberAt(setId, found, at, weighted + eveAt) != eve) continue;
            bool picked = false;
            for (uint16 p = 1; p < weighted && !picked; ++p) {
                picked = schedule.memberAt(setId, found, at, p) == eve;
            }
            if (!picked) break;
        }
        _relay(at, found);
    }

    /// @dev Moves `member` to another release the timelock approved for it, leaving it no promotable
    /// standby key until it registers one.
    function _keyless(address member, uint256 release) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(member, _digest(release), CONFIG, true);
        vm.prank(member);
        registry.setRelease(_digest(release), CONFIG);
        assertEq(schedule.promotable(member), address(0));
    }

    /// The order is bob, carol, dave, bob. Alice refuses every take: the draw and the two skips due before
    /// the untaken-turn promotion assign bob, carol and dave, and eve alone holds nothing of the turn. The
    /// promotion names bob at position 3, the next member by position, not eve.
    function test_AHoldersRefusalsDoNotChooseWhichMemberIsPromoted() public {
        address[] memory order = new address[](4);
        (order[0], order[1], order[2], order[3]) = (bob, carol, dave, bob);
        _aliceIsTheSource(order, false, false);
        address bobKey = schedule.promotable(bob);
        _aliceRefusesUntilPromoted();
        assertTrue(schedule.heldTurn(turn, carol) && schedule.heldTurn(turn, dave), "the two skips");
        assertFalse(schedule.heldTurn(turn, eve));
        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        assertEq(current.position, 3);
        assertEq(current.candidate, bob, "not the one member the refusals left unassigned");
        assertEq(vault.checkpointSigner(), bobKey);
    }

    /// Eve's promotion lands at the scan's last position: bob, carol and dave are between standby keys
    /// then, having moved releases. They register keys for their new releases and eve never starts. The
    /// next promotion names one of them, from the positions before eve's in the scan.
    function test_APromotionIntoTheScanLeavesTheMembersBeforeItToPromote() public {
        _eveIsPromotedIntoTheScan();
        assertEq(schedule.memberAt(setId, seed, turn, schedule.MAX_DRAW_POSITIONS() + eveAt), eve);
        _theOthersRegisterKeysAndEveStaysSilent();
        address member = schedule.turnOf(turn).candidate;
        assertTrue(member == bob || member == carol || member == dave, "a member before eve in the scan");
        (OperatorSchedule.Assignment memory promoted,) = schedule.assignmentOf(turn, schedule.turnOf(turn).position);
        assertEq(vault.checkpointSigner(), promoted.standbyKey);
    }

    /// The same with the scan ending eve, alice: the next promotion names one of bob, carol and dave
    /// before alice, the key holder that refused every take, who stands right after eve.
    function test_AHolderWithASmallAccompliceIsNotPromotedBeforeTheMembersItKeptOut() public {
        eveAt = 3;
        _eveIsPromotedIntoTheScan();
        assertEq(schedule.memberAt(setId, seed, turn, schedule.MAX_DRAW_POSITIONS() + 4), alice);
        address aliceKey = schedule.promotable(alice);
        _theOthersRegisterKeysAndEveStaysSilent();
        address member = schedule.turnOf(turn).candidate;
        assertTrue(member == bob || member == carol || member == dave, "a member the holder kept out");
        assertTrue(vault.checkpointSigner() != aliceKey, "not the holder's own standby key");
    }

    /// For any seed, and any of bob, carol, dave and eve between standby keys at the first promotion, each
    /// promotion while alice refuses and every promoted member stays silent names exactly the member the
    /// rule's position walk names (`_expectedPromotion`); it names one of ranks 0 and 1 whenever such a
    /// member qualifies anywhere in the set, the key holder or a holder moved past only when none does, and
    /// it refuses only when no member qualifies at all.
    /// forge-config: default.fuzz.runs = 128
    function testFuzz_APromotionNamesAQualifiedMemberWhereverItStands(uint256 salt_, uint8 keyless) public {
        (salt, salted) = (salt_, true);
        address[] memory none = new address[](0);
        _aliceIsTheSource(none, false, false);
        address[4] memory others = [bob, carol, dave, eve];
        for (uint256 i = 0; i < 4; ++i) {
            if (keyless & (1 << i) != 0) _keyless(others[i], 90 + i);
        }
        _poll();
        vm.warp(block.timestamp + 30);
        _aliceBeats();
        uint64 epoch = vault.signerEpoch();
        // Alice refuses every take until the first promotion.
        for (uint256 slots = 0; slots < 12 && vault.signerEpoch() == epoch; ++slots) {
            _nextSlot(5, 240);
            schedule.witness();
            if (block.timestamp >= schedule.promotionDueAt()) {
                if (!_promotesAsTheRuleSays()) return;
            } else if (block.timestamp >= schedule.skipDueAt()) {
                try schedule.skip() {} catch {}
            }
            if (vault.signerEpoch() == epoch) {
                vm.warp(block.timestamp + 25);
                _aliceBeats();
            }
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a promotion happened");
        // The keyless members register keys; every promoted member stays silent.
        for (uint256 i = 0; i < 4; ++i) {
            if (keyless & (1 << i) != 0) _registerStandby(others[i]);
        }
        for (uint256 promotions = 0; promotions < 6; ++promotions) {
            _lapse();
            if (!_promotesAsTheRuleSays()) return;
        }
        fail("more promotions than members");
    }

    /// @dev Promotes, checking the outcome against the rule; false once it refuses.
    function _promotesAsTheRuleSays() internal returns (bool) {
        (uint16 position, address expected) = _expectedPromotion();
        (address[] memory members, bool[] memory unkeyed, bool[] memory last) = _qualified();
        (bool anyUnkeyed, bool anyLast) = (false, false);
        for (uint256 i = 0; i < members.length; ++i) {
            (anyUnkeyed, anyLast) = (anyUnkeyed || unkeyed[i], anyLast || last[i]);
        }
        try schedule.promote() returns (uint64, uint16 promoted) {
            assertEq(promoted, position, "the rule's position");
            address member = schedule.turnOf(turn).candidate;
            assertEq(member, expected, "the rule's member");
            for (uint256 i = 0; i < members.length; ++i) {
                if (members[i] != member) continue;
                if (anyUnkeyed) assertTrue(unkeyed[i], "a member that never held the keys was passed by");
                else assertTrue(last[i], "promoted a member that does not qualify");
            }
            return true;
        } catch (bytes memory reason) {
            assertEq(bytes4(reason), OperatorSchedule.NoQualifiedPosition.selector);
            assertFalse(anyUnkeyed || anyLast, "refused while a member qualified");
            assertEq(expected, address(0));
            return false;
        }
    }

    /// @dev Which members of the set, wherever they stand in the turn's order, qualify now, listed under
    /// their adopted keys with a promotable standby key: `unkeyed`, one that never held the turn's keys and
    /// is not the key holder; `last`, the key holder or a holder a promotion moved past, holding no
    /// assignment of the turn.
    function _qualified() internal view returns (address[] memory members, bool[] memory unkeyed, bool[] memory last) {
        (,, members,) = schedule.setOf(setId);
        (unkeyed, last) = (new bool[](members.length), new bool[](members.length));
        address holder = _keyHolder();
        for (uint256 i = 0; i < members.length; ++i) {
            address member = members[i];
            if (
                !registry.isListed(member) || registry.keyHolder(_key(member)) != member
                    || schedule.promotable(member) == address(0)
            ) continue;
            bool keys = schedule.heldKeys(turn, member);
            bool holds = member == holder;
            unkeyed[i] = !keys && !holds;
            last[i] = !schedule.heldTurn(turn, member) && keys != holds;
        }
    }

    /// @dev Alice is the source of the first turn, drawn for bob, and refuses every take. Bob, carol and
    /// dave move releases at the boundary. The skips assign carol and dave, and the untaken-turn promotion
    /// names eve, the only member with a promotable key, at the scan's position `eveAt`.
    function _eveIsPromotedIntoTheScan() internal {
        address[] memory none = new address[](0);
        _aliceIsTheSource(none, false, false);
        _keyless(bob, 91);
        _keyless(carol, 92);
        _keyless(dave, 93);
        _aliceRefusesUntilPromoted();
        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        assertEq(current.candidate, eve);
        assertEq(current.position, schedule.MAX_DRAW_POSITIONS() + eveAt, "in the scan");
        assertTrue(schedule.heldTurn(turn, bob) && schedule.heldTurn(turn, carol) && schedule.heldTurn(turn, dave));
    }

    /// @dev Bob, carol and dave register standby keys for their new releases, vouched by eve's standby key,
    /// the vault's signer; eve never starts, and the callers poll until the missed-head promotion.
    function _theOthersRegisterKeysAndEveStaysSilent() internal {
        address[3] memory live = [bob, carol, dave];
        for (uint256 i = 0; i < 3; ++i) {
            _registerStandby(live[i]);
            assertTrue(registry.isListed(live[i]) && schedule.promotable(live[i]) != address(0), "listed, with a key");
            assertFalse(schedule.heldKeys(turn, live[i]), "never held the turn's keys");
        }
        silent = true;
        uint64 epoch = vault.signerEpoch();
        for (uint256 slots = 0; slots < 8 && vault.signerEpoch() == epoch; ++slots) {
            _slotOn();
        }
        assertEq(vault.signerEpoch(), epoch + 1, "a second promotion happened");
    }
}
