// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";

/// Random warps, witnesses, live and held heads and promotions against one turn naming the operator:
/// the time a promotion is due never moves earlier within a signer epoch while it is still ahead, and `promote` reverts with
/// `PromotionNotDue` exactly before it. The test contract is its own handler.
contract LivenessInvariantTest is LivenessTest {
    mapping(uint64 epoch => uint64 dueAt) internal lastDue;

    uint256 internal decreases;
    uint256 internal earlyPasses;
    uint256 internal lateRefusals;
    uint256 internal unexplainedReverts;
    uint256 internal promotions;
    uint256 internal headsPosted;
    uint256 internal modelMismatches;

    function setUp() public {
        _deployLiveness();
        _startTurn();
        _track();

        targetContract(address(this));
        bytes4[] memory selectors = new bytes4[](6);
        selectors[0] = this.handleWarp.selector;
        selectors[1] = this.handleWitness.selector;
        selectors[2] = this.handleHead.selector;
        selectors[3] = this.handlePromote.selector;
        selectors[4] = this.handleLapse.selector;
        selectors[5] = this.handleIncumbentHead.selector;
        targetSelector(FuzzSelector({addr: address(this), selectors: selectors}));
    }

    /// @dev Records `promotionDueAt` for the signer epoch, noting a value below the last one seen in it
    /// that was still ahead of block time: once the count has passed the value is at or before now, and
    /// falls to the slot of the epoch's start, so only a fall from a future time moves a promotion earlier
    /// than a fence set on it.
    function _track() internal {
        uint64 epoch = vault.signerEpoch();
        uint64 dueAt = schedule.promotionDueAt();
        if (dueAt < lastDue[epoch] && lastDue[epoch] > block.timestamp) ++decreases;
        if (dueAt != _modelDue()) ++modelMismatches;
        lastDue[epoch] = dueAt;
    }

    /// @dev `promotionDueAt` as its doc comment states it, read from the heads, latches and marks on
    /// chain: the earlier of the missed-head count, from the later slot of the epoch's latest head and of
    /// the signer change over `misses` slots, and, while the turn's current assignment is unlatched, the
    /// untaken-turn count, from the later slot of the boundary and of the signer change over `misses + 1`.
    function _modelDue() internal view returns (uint64 due) {
        (uint64 interval,, uint16 misses,) = schedule.liveness();
        (,,,, uint64 postedAt) = logHeads.heads(vault.signerEpoch());
        uint64 changed = vault.signerSince() / interval;
        uint64 latest = changed;
        if (postedAt / interval > latest) latest = postedAt / interval;
        due = _modelCount(latest, misses, interval);
        (uint64 at,,,,) = logHeads.latches(turn, schedule.turnOf(turn).position);
        if (at != 0) return due;
        uint64 since = schedule.boundary(turn) / interval;
        if (changed > since) since = changed;
        uint64 untaken = _modelCount(since, misses + 1, interval);
        if (untaken < due) due = untaken;
    }

    /// @dev `(max(s, u) + count + 1) * interval`, with `u` the latest unmarked slot after `s` among the
    /// `count` complete slots before the current one.
    function _modelCount(uint64 s, uint64 count, uint64 interval) internal view returns (uint64) {
        uint64 current = uint64(block.timestamp) / interval;
        uint64 last = s;
        for (uint64 slot = current - count; slot < current; ++slot) {
            (uint64 at,) = schedule.marks(slot);
            bool marked = at != 0 && at / interval == slot;
            if (slot > s && !marked) last = slot;
        }
        return (last + count + 1) * interval;
    }

    /// @dev The last second the turn names the operator.
    function _turnEnd() internal view returns (uint256) {
        return schedule.boundary(turn) + 1 days - 1;
    }

    function _warpBy(uint256 seconds_) internal {
        uint256 target = block.timestamp + seconds_;
        uint256 end = _turnEnd();
        if (target > end) target = end;
        if (target <= block.timestamp) return;
        // Arbitrum blocks come at the pinned 250 ms.
        ARB.setBlockNumber(ARB.arbBlockNumber() + (target - block.timestamp) * 4);
        vm.warp(target);
    }

    function _tryWitness() internal {
        try schedule.witness() returns (bool) {} catch {}
    }

    /// @dev Posts a live or held head for the current epoch under its signer's key; a refusal is allowed.
    function _tryHead(bool held) internal {
        LogHeads.HeadV2 memory head = _nextHead();
        if (held) head = _held(head, 1, uint64(ARB.arbBlockNumber() - 1));
        bytes memory signature = _sign(head, _signerKey());
        try logHeads.postV2(head, signature) {
            ++headsPosted;
        } catch {}
    }

    function _tryPromote() internal {
        uint64 dueAt = schedule.promotionDueAt();
        bool due = block.timestamp >= dueAt;
        try schedule.promote() returns (uint64, uint16) {
            if (!due) ++earlyPasses;
            ++promotions;
        } catch (bytes memory reason) {
            bytes4 selector = bytes4(reason);
            if (selector == OperatorSchedule.PromotionNotDue.selector) {
                if (due) ++lateRefusals;
            } else if (!due) {
                // Before its time a promotion can only be refused as not due.
                ++unexplainedReverts;
            }
        }
    }

    function handleWarp(uint256 seconds_) external {
        _warpBy(bound(seconds_, 0, 400));
        _track();
    }

    function handleWitness() external {
        _tryWitness();
        _track();
    }

    function handleHead(bool held) external {
        _tryHead(held);
        _track();
    }

    /// @dev A live head binding an earlier turn, as an incumbent past its turn posts: it resets the
    /// missed-head count and latches nothing.
    function handleIncumbentHead() external {
        LogHeads.HeadV2 memory head = _nextHead();
        (head.turn, head.position, head.generation) = (turn - 1, 0, 1);
        try logHeads.postV2(head, _sign(head, _signerKey())) {
            ++headsPosted;
        } catch {}
        _track();
    }

    /// @dev Witnesses the slots from the next one on until the count would pass, so promotions come up
    /// within a turn's day.
    function handleLapse() external {
        _tryWitness();
        uint256 end = _turnEnd();
        for (uint256 i = 0; i < MISSES + 1 && block.timestamp + INTERVAL < end; ++i) {
            _nextSlot(10, 240);
            _tryWitness();
        }
        _track();
    }

    function handlePromote() external {
        _tryPromote();
        _track();
    }

    /// forge-config: default.invariant.runs = 100
    /// forge-config: default.invariant.depth = 60
    /// forge-config: default.invariant.fail-on-revert = false
    function invariant_PromotionDueAtNeverDecreasesWithinAnEpoch() public view {
        assertEq(decreases, 0);
    }

    /// forge-config: default.invariant.runs = 100
    /// forge-config: default.invariant.depth = 60
    /// forge-config: default.invariant.fail-on-revert = false
    function invariant_PromoteRefusesAsNotDueExactlyBeforeDueAt() public view {
        assertEq(earlyPasses, 0, "promoted before it was due");
        assertEq(lateRefusals, 0, "refused as not due once due");
        assertEq(unexplainedReverts, 0, "reverted for another reason before it was due");
    }

    /// forge-config: default.invariant.runs = 100
    /// forge-config: default.invariant.depth = 60
    /// forge-config: default.invariant.fail-on-revert = false
    function invariant_PromotionDueAtIsTheDocumentedCount() public view {
        assertEq(modelMismatches, 0);
    }

    /// For random histories, `promote` one second before `promotionDueAt` reverts as not due and at it
    /// is not refused as not due, with every slot up to then witnessed so the due time holds still.
    /// forge-config: default.fuzz.runs = 300
    function testFuzz_PromoteIsDueExactlyAtPromotionDueAt(bytes32 entropy, uint8 steps) public {
        for (uint256 i = 0; i < bound(steps, 0, 14); ++i) {
            uint256 pick = uint256(keccak256(abi.encode(entropy, i)));
            uint256 action = pick % 4;
            if (action == 0) _warpBy(bound(pick >> 8, 0, 400));
            else if (action == 1) _tryWitness();
            else _tryHead(action == 3);
        }
        _tryWitness();

        uint64 dueAt = schedule.promotionDueAt();
        assertEq(dueAt, _modelDue(), "the documented count");
        if (dueAt > block.timestamp) {
            // Witness each slot's first second up to the one holding `dueAt - 1`.
            for (uint64 slot = _slot() + 1; uint256(slot) * INTERVAL < dueAt; ++slot) {
                vm.warp(uint256(slot) * INTERVAL);
                ARB.setBlockNumber(ARB.arbBlockNumber() + 240);
                _tryWitness();
            }
            assertEq(schedule.promotionDueAt(), dueAt, "the due time holds while every slot is witnessed");
            if (block.timestamp < dueAt - 1) {
                ARB.setBlockNumber(ARB.arbBlockNumber() + 40);
                vm.warp(dueAt - 1);
            }
            vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.PromotionNotDue.selector, dueAt));
            schedule.promote();
            vm.warp(dueAt);
        }

        (, address member) = _expectedPromotion();
        try schedule.promote() returns (uint64, uint16) {
            assertTrue(member != address(0), "promoted with no qualified member");
        } catch (bytes memory reason) {
            assertTrue(bytes4(reason) != OperatorSchedule.PromotionNotDue.selector, "refused as not due at dueAt");
            assertEq(member, address(0), "refused with a qualified member");
        }
    }
}

/// `setLiveness` accepts a rule exactly when every bound its doc comment states holds, checked against an
/// independent statement of them.
contract LivenessRuleFuzzTest is LivenessTest {
    function setUp() public {
        _deployLiveness();
    }

    /// @dev The documented bounds, restated: the deployment's minimums and maximums (30..120 s, 5..60 s,
    /// 2..9 misses, 10..256 blocks), at least two misses, detection in ten minutes, the tolerance under
    /// one interval, the held-head window within 256 blocks and under one interval at 250 ms a block,
    /// and the miss window above the 120 s switch budget.
    function _holds(uint64 interval, uint64 tolerance, uint16 misses, uint16 blocks) internal pure returns (bool) {
        if (interval < 30 || interval > 120) return false;
        if (tolerance < 5 || tolerance > 60) return false;
        if (misses < 2 || misses > 9) return false;
        if (blocks < 10 || blocks > 256) return false;
        uint256 detection = uint256(interval) * (uint256(misses) + 1) + tolerance;
        if (detection > 10 minutes) return false;
        if (tolerance >= interval) return false;
        uint256 windowMillis = uint256(blocks) * 250;
        if (windowMillis >= uint256(interval) * 1000) return false;
        if (uint256(interval) * misses <= SWITCH_BUDGET) return false;
        return true;
    }

    /// forge-config: default.fuzz.runs = 3000
    function testFuzz_TheRuleIsAcceptedExactlyWhenItsBoundsHold(
        uint64 interval,
        uint64 tolerance,
        uint16 misses,
        uint16 blocks
    ) public {
        // Reach around the bounds, so refusals on each side and acceptances both come up.
        interval = uint64(bound(interval, 0, 150));
        tolerance = uint64(bound(tolerance, 0, 80));
        misses = uint16(bound(misses, 0, 12));
        blocks = uint16(bound(blocks, 0, 300));
        OperatorSchedule.Liveness memory rule = OperatorSchedule.Liveness(interval, tolerance, misses, blocks);

        vm.prank(address(governance));
        try schedule.setLiveness(rule) {
            assertTrue(_holds(interval, tolerance, misses, blocks), "accepted a rule outside its bounds");
            (uint64 i, uint64 t, uint16 m, uint16 b) = schedule.liveness();
            assertEq(i, interval);
            assertEq(t, tolerance);
            assertEq(m, misses);
            assertEq(b, blocks);
        } catch (bytes memory reason) {
            assertEq(bytes4(reason), OperatorSchedule.InvalidLiveness.selector);
            assertFalse(_holds(interval, tolerance, misses, blocks), "refused a rule within its bounds");
        }
    }

    /// forge-config: default.fuzz.runs = 1000
    function testFuzz_AnyRuleOutsideTheDeploymentRangeIsRefused(
        uint64 interval,
        uint64 tolerance,
        uint16 misses,
        uint16 blocks
    ) public {
        vm.assume(!_holds(interval, tolerance, misses, blocks));
        vm.prank(address(governance));
        vm.expectRevert(OperatorSchedule.InvalidLiveness.selector);
        schedule.setLiveness(OperatorSchedule.Liveness(interval, tolerance, misses, blocks));
    }
}
