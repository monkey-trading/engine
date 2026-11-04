// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {SeedSchedule} from "../src/SeedSchedule.sol";

contract SeedReceiverTest is Test {
    /// Mainnet's beacon genesis time.
    uint64 internal constant GENESIS = 1606824023;
    /// Turn 20763 starts at 22:30 UTC on 2026-11-06.
    uint64 internal constant TURN = 20763;

    address internal relay = makeAddr("relay");
    SeedReceiver internal receiver;

    function setUp() public {
        receiver = new SeedReceiver(relay, GENESIS);
    }

    function test_TheScheduleFixesMainnetTurnsToEpochsADayApart() public view {
        assertEq(receiver.boundary(TURN), 1794004200, "22:30 UTC on 2026-11-06");
        assertEq(receiver.drawDeadline(TURN), 1794004200 - 1 days);
        // The epoch containing 22:30 UTC on 2026-11-03, two days before the draw deadline.
        assertEq(receiver.seedEpoch(TURN), 486773);
        uint64 epochEnd = GENESIS + (486773 + 1) * 384;
        assertGt(epochEnd, receiver.drawDeadline(TURN) - 2 days);
        assertLe(epochEnd, receiver.drawDeadline(TURN) - 2 days + 384);
    }

    function testFuzz_ConsecutiveTurnsAre225EpochsApart(uint64 turn) public view {
        turn = uint64(bound(turn, 18_600, 1_000_000));
        assertEq(receiver.seedEpoch(turn + 1) - receiver.seedEpoch(turn), 225);
        assertEq(receiver.drawDeadline(turn + 1) - receiver.drawDeadline(turn), 1 days);
    }

    function test_RevertWhen_TheTurnIsBeforeGenesis() public {
        // Turn 18599's seed time, 22:30 UTC on 2020-11-30, is before mainnet's beacon genesis.
        vm.expectRevert(abi.encodeWithSelector(SeedSchedule.TurnBeforeGenesis.selector, uint64(18599)));
        receiver.seedEpoch(18599);
        assertEq(receiver.seedEpoch(18600), (uint64(18600) * 1 days - 178200 - GENESIS) / 384);
    }

    function test_RecordsASeedFromTheAliasedRelay() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        vm.expectEmit(address(receiver));
        emit SeedReceiver.SeedRecorded(TURN, 486773, bytes32(uint256(7)));
        deliver(TURN, bytes32(uint256(7)));
        assertEq(receiver.relayedSeed(TURN), bytes32(uint256(7)));
        assertEq(receiver.firstRelayedTurn(), TURN);
        assertEq(receiver.seedOf(TURN), bytes32(uint256(7)));
    }

    function test_TheAliasIsTheRelayPlusArbitrumsOffset() public {
        assertEq(receiver.relayAlias(), address(uint160(relay) + uint160(0x1111000000000000000000000000000000001111)));
        // An address past the offset's complement wraps, as Arbitrum's alias does.
        SeedReceiver high = new SeedReceiver(address(type(uint160).max), GENESIS);
        assertEq(high.relayAlias(), address(uint160(0x1111000000000000000000000000000000001110)));
    }

    function test_RevertWhen_TheSenderIsNotTheAliasedRelay() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        uint64 epoch = receiver.seedEpoch(TURN);
        // The relay's own address is what an Ethereum contract's call never arrives from.
        for (uint256 i = 0; i < 2; ++i) {
            address sender = i == 0 ? relay : makeAddr("other");
            vm.prank(sender);
            vm.expectRevert(abi.encodeWithSelector(SeedReceiver.NotRelay.selector, sender));
            receiver.receiveSeed(TURN, epoch, bytes32(uint256(7)));
        }
    }

    function test_RevertWhen_TheEpochIsNotTheTurns() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        uint64 epoch = receiver.seedEpoch(TURN);
        address alias_ = receiver.relayAlias();
        vm.startPrank(alias_);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.WrongEpoch.selector, TURN, epoch + 1, epoch));
        receiver.receiveSeed(TURN, epoch + 1, bytes32(uint256(7)));
        // The next turn's epoch, a day later, is not this turn's either.
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.WrongEpoch.selector, TURN, epoch + 225, epoch));
        receiver.receiveSeed(TURN, epoch + 225, bytes32(uint256(7)));
    }

    function test_RevertWhen_TheSeedArrivesAtOrAfterTheDrawDeadline() public {
        uint64 deadline = receiver.drawDeadline(TURN);
        vm.warp(deadline);
        uint64 epoch = receiver.seedEpoch(TURN);
        vm.prank(receiver.relayAlias());
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.DrawDeadlinePassed.selector, TURN, deadline));
        receiver.receiveSeed(TURN, epoch, bytes32(uint256(7)));
        vm.warp(deadline - 1);
        deliver(TURN, bytes32(uint256(7)));
    }

    function test_RevertWhen_ASeedIsRelayedTwiceOrIsZero() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        uint64 epoch = receiver.seedEpoch(TURN);
        vm.startPrank(receiver.relayAlias());
        vm.expectRevert(SeedReceiver.ZeroSeed.selector);
        receiver.receiveSeed(TURN, epoch, 0);
        receiver.receiveSeed(TURN, epoch, bytes32(uint256(7)));
        // A replay, even of the same seed, changes nothing.
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.AlreadyRelayed.selector, TURN));
        receiver.receiveSeed(TURN, epoch, bytes32(uint256(7)));
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.AlreadyRelayed.selector, TURN));
        receiver.receiveSeed(TURN, epoch, bytes32(uint256(8)));
        vm.stopPrank();
        assertEq(receiver.relayedSeed(TURN), bytes32(uint256(7)));
    }

    function test_ATurnWithoutASeedIsFinalOnlyFromItsDrawDeadline() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        deliver(TURN, bytes32(uint256(7)));
        uint64 deadline = receiver.drawDeadline(TURN + 1);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.SeedNotFinal.selector, TURN + 1, deadline));
        receiver.seedOf(TURN + 1);
        vm.warp(deadline);
        assertEq(receiver.seedOf(TURN + 1), keccak256(abi.encode(bytes32(uint256(7)), TURN + 1)));
    }

    function test_RevertWhen_NoEarlierTurnHasASeed() public {
        vm.warp(receiver.drawDeadline(TURN + 1));
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.NoRelayedSeed.selector, TURN));
        receiver.seedOf(TURN);
        // A later turn's seed is no fallback for an earlier one.
        vm.warp(receiver.drawDeadline(TURN + 1) - 1);
        deliver(TURN + 1, bytes32(uint256(9)));
        vm.warp(receiver.drawDeadline(TURN + 2));
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.NoRelayedSeed.selector, TURN));
        receiver.seedOf(TURN);
        assertEq(receiver.seedOf(TURN + 2), keccak256(abi.encode(bytes32(uint256(9)), TURN + 2)));
    }

    function test_SeedsArrivingOutOfOrderKeepTheEarliestTurnFirst() public {
        // Two turns' relay windows overlap by a day, so the later turn's seed can land first.
        vm.warp(receiver.drawDeadline(TURN) - 1);
        deliver(TURN + 1, bytes32(uint256(9)));
        assertEq(receiver.firstRelayedTurn(), TURN + 1);
        deliver(TURN, bytes32(uint256(7)));
        assertEq(receiver.firstRelayedTurn(), TURN);
        vm.warp(receiver.drawDeadline(TURN + 3));
        assertEq(receiver.seedOf(TURN + 2), keccak256(abi.encode(bytes32(uint256(9)), TURN + 2)));
        assertEq(receiver.seedOf(TURN + 3), keccak256(abi.encode(bytes32(uint256(9)), TURN + 3)));
    }

    /// Bit `i` of `relayed` relays turn `TURN + i`; every turn's answer from the deadline on is the
    /// reference model's.
    function testFuzz_TheFallbackIsTheLatestEarlierSeedHashedWithTheTurn(uint32 relayed) public {
        uint64 span = 32;
        for (uint64 i = 0; i < span; ++i) {
            if ((relayed >> i) & 1 == 1) {
                vm.warp(receiver.drawDeadline(TURN + i) - 1);
                deliver(TURN + i, keccak256(abi.encode("seed", i)));
            }
        }
        vm.warp(receiver.drawDeadline(TURN + span));
        bytes32 last;
        for (uint64 i = 0; i < span; ++i) {
            uint64 turn = TURN + i;
            if ((relayed >> i) & 1 == 1) {
                last = keccak256(abi.encode("seed", i));
                assertEq(receiver.seedOf(turn), last);
            } else if (last == 0) {
                vm.expectRevert(abi.encodeWithSelector(SeedReceiver.NoRelayedSeed.selector, turn));
                receiver.seedOf(turn);
            } else {
                assertEq(receiver.seedOf(turn), keccak256(abi.encode(last, turn)));
            }
        }
    }

    function test_RevertWhen_TheRelayIsZero() public {
        vm.expectRevert(SeedReceiver.ZeroAddress.selector);
        new SeedReceiver(address(0), GENESIS);
        vm.expectRevert(SeedSchedule.ZeroGenesisTime.selector);
        new SeedReceiver(relay, 0);
    }

    function deliver(uint64 turn, bytes32 seed) internal {
        uint64 epoch = receiver.seedEpoch(turn);
        vm.prank(receiver.relayAlias());
        receiver.receiveSeed(turn, epoch, seed);
    }
}
