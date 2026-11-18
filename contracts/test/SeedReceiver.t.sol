// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {SeedSchedule} from "../src/SeedSchedule.sol";
import {MockArbSys} from "./mocks/MockArbSys.sol";

contract SeedReceiverTest is Test {
    /// Mainnet's beacon genesis time.
    uint64 internal constant GENESIS = 1606824023;
    /// Turn 20763 starts at 22:30 UTC on 2026-11-06.
    uint64 internal constant TURN = 20763;

    MockArbSys internal constant ARB = MockArbSys(address(0x64));

    address internal relay = makeAddr("relay");
    SeedReceiver internal receiver;

    function setUp() public {
        receiver = new SeedReceiver(relay, GENESIS, 1 days);
        vm.etch(address(ARB), address(new MockArbSys()).code);
        ARB.setBlockNumber(100_000_000);
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
        // Turns whose boundary is within three days of the Unix epoch are refused the same way.
        for (uint64 turn = 0; turn < 3; ++turn) {
            vm.expectRevert(abi.encodeWithSelector(SeedSchedule.TurnBeforeGenesis.selector, turn));
            receiver.seedEpoch(turn);
        }
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
        SeedReceiver high = new SeedReceiver(address(type(uint160).max), GENESIS, 1 days);
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
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.SeedNotFinal.selector, TURN + 1, deadline));
        receiver.fixFallback(TURN + 1);
        vm.warp(deadline);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.FallbackNotFixed.selector, TURN + 1));
        receiver.seedOf(TURN + 1);
        bytes32 blockHash = fix(TURN + 1);
        assertEq(receiver.seedOf(TURN + 1), keccak256(abi.encode(bytes32(uint256(7)), TURN + 1, blockHash)));
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
        bytes32 blockHash = fix(TURN + 2);
        assertEq(receiver.seedOf(TURN + 2), keccak256(abi.encode(bytes32(uint256(9)), TURN + 2, blockHash)));
    }

    function test_SeedsArrivingOutOfOrderKeepTheEarliestTurnFirst() public {
        // Two turns' relay windows overlap by a day, so the later turn's seed can land first.
        vm.warp(receiver.drawDeadline(TURN) - 1);
        deliver(TURN + 1, bytes32(uint256(9)));
        assertEq(receiver.firstRelayedTurn(), TURN + 1);
        deliver(TURN, bytes32(uint256(7)));
        assertEq(receiver.firstRelayedTurn(), TURN);
        vm.warp(receiver.drawDeadline(TURN + 3));
        (bytes32 second, bytes32 third) = (fix(TURN + 2), fix(TURN + 3));
        assertEq(receiver.seedOf(TURN + 2), keccak256(abi.encode(bytes32(uint256(9)), TURN + 2, second)));
        assertEq(receiver.seedOf(TURN + 3), keccak256(abi.encode(bytes32(uint256(9)), TURN + 3, third)));
    }

    /// Bit `i` of `relayed` relays turn `TURN + i`; every turn's answer from the deadline on is the
    /// reference model's.
    function testFuzz_TheFallbackIsTheLatestEarlierSeedHashedWithTheTurnAndItsBlock(uint32 relayed) public {
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
                bytes32 blockHash = fix(turn);
                assertEq(receiver.seedOf(turn), keccak256(abi.encode(last, turn, blockHash)));
            }
        }
    }

    function test_RevertWhen_TheTurnLengthIsUnderFiveMinutesOrNoDivisorOfADay() public {
        uint64[5] memory refused = [uint64(0), 4 minutes, 299, 7 minutes, 2 days];
        for (uint256 i = 0; i < refused.length; ++i) {
            vm.expectRevert(abi.encodeWithSelector(SeedSchedule.InvalidTurnLength.selector, refused[i]));
            new SeedReceiver(relay, GENESIS, refused[i]);
        }
        assertEq(new SeedReceiver(relay, GENESIS, 5 minutes).turnLength(), 5 minutes);
        assertEq(receiver.turnLength(), 1 days);
        assertEq(receiver.boundary(0), 22 hours + 30 minutes);
    }

    /// A five-minute turn starts every five minutes, 22:30 UTC among them; it is drawn a turn ahead from
    /// the mix of the epoch two turns before that, and a relay is refused from its deadline.
    function test_FiveMinuteTurnsKeepTheScheduleInTurns() public {
        SeedReceiver short = new SeedReceiver(relay, GENESIS, 5 minutes);
        assertEq(short.boundary(0), 0);
        uint64 turn = 1794004200 / 5 minutes;
        assertEq(short.boundary(turn), 1794004200, "22:30 UTC on 2026-11-06 is a boundary");
        assertEq(short.drawDeadline(turn), 1794004200 - 5 minutes);
        assertEq(short.seedEpoch(turn), (1794004200 - 15 minutes - GENESIS) / 384);
        vm.warp(short.drawDeadline(turn) - 1);
        (uint64 epoch, address aliased) = (short.seedEpoch(turn), short.relayAlias());
        vm.prank(aliased);
        short.receiveSeed(turn, epoch, bytes32(uint256(7)));
        assertEq(short.seedOf(turn), bytes32(uint256(7)));
        vm.warp(short.drawDeadline(turn + 1));
        epoch = short.seedEpoch(turn + 1);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.DrawDeadlinePassed.selector, turn + 1, block.timestamp));
        vm.prank(aliased);
        short.receiveSeed(turn + 1, epoch, bytes32(uint256(8)));
    }

    /// At every turn length a deployment may take, a turn ends where the next starts, a turn later, and
    /// each 22:30 UTC is a boundary.
    function testFuzz_TurnsTileEachDayAtEveryLength(uint256 divisor, uint64 turn, uint64 day) public {
        uint64[3] memory lengths = [uint64(5 minutes), 1 hours, 1 days];
        uint64 length = lengths[bound(divisor, 0, lengths.length - 1)];
        SeedReceiver schedule = new SeedReceiver(relay, GENESIS, length);
        turn = uint64(bound(turn, 0, type(uint64).max / 1 days - 2));
        assertEq(schedule.boundary(turn + 1) - schedule.boundary(turn), length);
        assertEq(schedule.drawDeadline(turn + 1), schedule.boundary(turn));
        day = uint64(bound(day, 0, 1_000_000));
        uint64 halfPastTen = day * 1 days + 22 hours + 30 minutes;
        uint64 named = (halfPastTen - schedule.boundary(0)) / length;
        assertEq(schedule.boundary(named), halfPastTen);
    }

    function test_RevertWhen_TheRelayIsZero() public {
        vm.expectRevert(SeedReceiver.ZeroAddress.selector);
        new SeedReceiver(address(0), GENESIS, 1 days);
        vm.expectRevert(SeedSchedule.ZeroGenesisTime.selector);
        new SeedReceiver(relay, 0, 1 days);
    }

    /// A fallback's block is the one its first `fixFallback` call lands in, and a call from a later
    /// block within `arbBlockHash`'s reach records that block's hash; a relayed turn needs neither.
    function test_AFallbackFoldsInTheHashOfTheBlockItsFirstFixCallLandedIn() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        deliver(TURN, bytes32(uint256(7)));
        assertTrue(receiver.fixFallback(TURN), "a relayed seed is final");
        assertEq(receiver.fallbackBlock(TURN), 0);
        vm.warp(receiver.drawDeadline(TURN + 1));
        uint256 committed = ARB.arbBlockNumber();
        vm.expectEmit(address(receiver));
        emit SeedReceiver.FallbackBlockFixed(TURN + 1, committed);
        assertFalse(receiver.fixFallback(TURN + 1));
        assertFalse(receiver.fixFallback(TURN + 1), "not from the same block");
        assertEq(receiver.fallbackHash(TURN + 1), 0);
        ARB.setBlockNumber(committed + 256);
        vm.expectEmit(address(receiver));
        emit SeedReceiver.FallbackHashFixed(TURN + 1, ARB.hashOf(committed));
        assertTrue(receiver.fixFallback(TURN + 1));
        // Later calls change nothing.
        ARB.setBlockNumber(committed + 1000);
        assertTrue(receiver.fixFallback(TURN + 1));
        assertEq(receiver.fallbackBlock(TURN + 1), committed);
        assertEq(receiver.fallbackHash(TURN + 1), ARB.hashOf(committed));
        assertEq(receiver.seedOf(TURN + 1), keccak256(abi.encode(bytes32(uint256(7)), TURN + 1, ARB.hashOf(committed))));
    }

    /// Past `arbBlockHash`'s reach the block's hash is never recorded and no later block replaces it,
    /// so the turn has no final seed.
    function test_RevertWhen_TheFallbacksBlockIsOutOfReach() public {
        vm.warp(receiver.drawDeadline(TURN) - 1);
        deliver(TURN, bytes32(uint256(7)));
        vm.warp(receiver.drawDeadline(TURN + 1));
        uint256 committed = ARB.arbBlockNumber();
        assertFalse(receiver.fixFallback(TURN + 1));
        ARB.setBlockNumber(committed + 257);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.FallbackExpired.selector, TURN + 1, committed));
        receiver.fixFallback(TURN + 1);
        vm.expectRevert(abi.encodeWithSelector(SeedReceiver.FallbackNotFixed.selector, TURN + 1));
        receiver.seedOf(TURN + 1);
        assertEq(receiver.fallbackBlock(TURN + 1), committed);
    }

    /// Fixes `turn`'s fallback over two blocks, as two calls a block apart do, and returns its hash.
    function fix(uint64 turn) internal returns (bytes32) {
        uint256 committed = ARB.arbBlockNumber();
        assertFalse(receiver.fixFallback(turn));
        ARB.setBlockNumber(committed + 1);
        assertTrue(receiver.fixFallback(turn));
        return ARB.hashOf(committed);
    }

    function deliver(uint64 turn, bytes32 seed) internal {
        uint64 epoch = receiver.seedEpoch(turn);
        vm.prank(receiver.relayAlias());
        receiver.receiveSeed(turn, epoch, seed);
    }
}
