// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";
import {MockArbSys} from "./mocks/MockArbSys.sol";

/// @notice A turn with no relayed seed draws from a fallback that folds in the hash of an Arbitrum block
/// fixed from its draw deadline, so the weights a rotation set takes at its close, a week or more
/// earlier, cannot be chosen for the seed its turns draw.
contract FallbackSeedTest is ScheduleTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");

    bytes32 internal lastSeed;
    uint64 internal lastRelayed;
    uint64 internal nextDraw;
    bool internal relaysWork;

    function setUp() public {
        _deploySchedule();
    }

    /// @dev Alice and bob hold 45% each and carol 10%, in one set in effect; one seed is on the
    /// receiver, for the next turn, and from here none arrives unless `relaysWork`. Carol holds a little
    /// undelegated stake.
    function _threeMembers() internal {
        address[] memory members = new address[](3);
        uint256[] memory stakes = new uint256[](3);
        (members[0], members[1], members[2]) = (alice, bob, carol);
        (stakes[0], stakes[1], stakes[2]) = (45_000e18, 45_000e18, 10_000e18);
        _rotate(members, stakes);
        _stake(carol, 1e12);
        lastRelayed = _nextTurn() + 1;
        lastSeed = keccak256("the last seed relayed");
        _relay(lastRelayed, lastSeed);
        nextDraw = lastRelayed;
    }

    /// @dev Draws every turn up to and including `turn`, each at its draw deadline; while relays work,
    /// each turn's mix is relayed once its epoch has ended.
    function _drawThrough(uint64 turn) internal {
        while (nextDraw <= turn) {
            uint64 t = nextDraw++;
            if (relaysWork && receiver.relayedSeed(t) == 0) {
                uint256 mixKnownAt = schedule.drawDeadline(t) - 2 days + 384;
                if (block.timestamp < mixKnownAt) vm.warp(mixKnownAt);
                _relay(t, keccak256(abi.encode("a mix", t)));
            }
            vm.warp(schedule.drawDeadline(t));
            _draw(t);
        }
    }

    /// @dev The first turn whose draw deadline is at or after `effectiveAt`, which the set taking effect
    /// then draws.
    function _firstTurnOf(uint256 effectiveAt) internal view returns (uint64 turn) {
        for (turn = _nextTurn(); schedule.drawDeadline(turn) < effectiveAt; ++turn) {}
    }

    /// @dev Closes the ended epoch with carol's delegation raised by the fewest wei that make the set it
    /// adopts pick carol first under `seed` for the turn that set draws first.
    function _closePicking(function(uint64) view returns (bytes32) seed)
        internal
        returns (uint64 turn, uint32 setId, uint256 delta)
    {
        (uint64 epochLength,,,,,,) = registry.parameters();
        uint64 endsAt = uint64(registry.openEpochStart()) + epochLength;
        if (block.timestamp < endsAt) vm.warp(endsAt);
        turn = _firstTurnOf(block.timestamp + 7 days + FINALITY_MARGIN);
        for (delta = 1;; ++delta) {
            uint256 snapshot = vm.snapshotState();
            vm.prank(carol);
            staking.delegate(carol, delta);
            registry.closeEpoch();
            setId = schedule.setCount();
            if (schedule.memberAt(setId, seed(turn), turn, 0) == carol) break;
            vm.revertToState(snapshot);
        }
    }

    /// @dev The fallback over the last relayed seed as it stood before the block hash was folded in.
    function _withoutABlock(uint64 turn) internal view returns (bytes32) {
        return keccak256(abi.encode(lastSeed, turn));
    }

    /// With relays stopped, sets whose weights at their closes make the fallback over the last relayed
    /// seed pick carol first do not decide their turns: each draws from that seed hashed with the turn
    /// and the block fixed from its deadline, which leads with carol only as her weight's share allows,
    /// where the seed known at each close would have led with her every time.
    function test_FallbackTurnsAreNotSetByTheWeightsAtTheirCloses() public {
        _threeMembers();
        uint256 led = 0;
        for (uint256 i = 0; i < 3; ++i) {
            (uint64 turn, uint32 setId,) = _closePicking(_withoutABlock);
            assertEq(schedule.memberAt(setId, _withoutABlock(turn), turn, 0), carol);
            _drawThrough(turn);
            OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
            assertEq(drawn.setId, setId);
            uint256 fixedIn = receiver.fallbackBlock(turn);
            assertEq(drawn.seed, keccak256(abi.encode(lastSeed, turn, MockArbSys(address(0x64)).hashOf(fixedIn))));
            assertTrue(drawn.seed != _withoutABlock(turn), "the seed known at the close is not the seed drawn");
            if (drawn.candidate == carol) ++led;
        }
        assertLt(led, 3, "the weights set at each close led every turn they were set for");
    }

    /// The fallback's block is the first block a fix landed in from the deadline: no call before the
    /// deadline, when the set is long fixed, can choose it.
    function test_AFallbacksBlockIsFixedFromTheTurnsDrawDeadline() public {
        _threeMembers();
        uint64 turn = lastRelayed + 1;
        _drawThrough(lastRelayed);
        vm.warp(schedule.drawDeadline(turn) - 1);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.SeedNotFinal.selector, turn, schedule.drawDeadline(turn)));
        receiver.fixFallback(turn);
        vm.warp(schedule.drawDeadline(turn));
        uint256 fixedIn = MockArbSys(address(0x64)).arbBlockNumber();
        assertEq(schedule.draw(turn), address(0), "the first call only fixes the block");
        assertEq(receiver.fallbackBlock(turn), fixedIn);
        assertEq(schedule.turnOf(turn).candidate, address(0));
        assertEq(schedule.draw(turn), address(0), "a call in the same block draws no one either");
        _nextArbitrumBlock();
        address drawn = schedule.draw(turn);
        assertTrue(drawn != address(0));
        assertEq(schedule.turnOf(turn).seed, keccak256(abi.encode(lastSeed, turn, receiver.fallbackHash(turn))));
    }

    /// With a working relay, the turn draws its relayed mix, with no Arbitrum block, in one call.
    function test_ARelayedTurnDrawsItsMixInOneCall() public {
        relaysWork = true;
        _threeMembers();
        _drawThrough(lastRelayed);
        uint64 turn = lastRelayed + 1;
        vm.warp(schedule.drawDeadline(turn) - 1 days);
        bytes32 mix = keccak256("a mix");
        _relay(turn, mix);
        vm.warp(schedule.drawDeadline(turn));
        assertTrue(schedule.draw(turn) != address(0));
        assertEq(schedule.turnOf(turn).seed, mix);
        assertEq(receiver.fallbackBlock(turn), 0);
    }

    /// After a receiver change, a turn with no seed on the new receiver draws from the schedule's
    /// anchor hashed with the turn and the block the new receiver fixed, not from a value known before
    /// the deadline.
    function test_AfterAReceiverChangeTheAnchorFallbackFoldsInTheFixedBlock() public {
        _threeMembers();
        _drawThrough(lastRelayed);
        SeedReceiver forked = new SeedReceiver(makeAddr("forked relay"), GENESIS, 1 days);
        vm.prank(address(governance));
        schedule.setSeedReceiver(forked);
        uint64 turn = lastRelayed + 1;
        bytes32 anchor = schedule.seedAnchor();
        vm.warp(schedule.drawDeadline(turn));
        _draw(turn);
        bytes32 blockHash = forked.fallbackHash(turn);
        assertEq(blockHash, MockArbSys(address(0x64)).hashOf(forked.fallbackBlock(turn)));
        assertEq(schedule.turnOf(turn).seed, keccak256(abi.encode(anchor, turn, blockHash)));
        assertTrue(schedule.turnOf(turn).seed != keccak256(abi.encode(anchor, turn)));
    }
}
