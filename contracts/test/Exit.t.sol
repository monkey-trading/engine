// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {Vault} from "../src/Vault.sol";
import {ExitTest} from "./helpers/ExitTest.sol";

/// The exit against the last posted root: when it may start, what it stops, and how claims,
/// refunds and the house remainder are paid.
contract ExitTestCases is ExitTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal funder = makeAddr("funder");
    address internal carol = makeAddr("carol");

    function testRefusesAnExitScheduleThatCouldTripBeforeGovernanceOrNeverRefuseDeposits() public {
        uint64 delay = uint64(GOVERNANCE_DELAY);
        vm.expectRevert(Vault.InvalidExitSchedule.selector);
        new Vault(usdc, vm.addr(KEY), governance, multisig, 1e18, delay, WINDOW, STALE);
        vm.expectRevert(Vault.InvalidExitSchedule.selector);
        new Vault(usdc, vm.addr(KEY), governance, multisig, 1e18, PERIOD, 0, STALE);
        vm.expectRevert(Vault.InvalidExitSchedule.selector);
        new Vault(usdc, vm.addr(KEY), governance, multisig, 1e18, PERIOD, WINDOW, 0);
        vm.expectRevert(Vault.InvalidExitSchedule.selector);
        new Vault(usdc, vm.addr(KEY), governance, multisig, 1e18, PERIOD, WINDOW, PERIOD + 1);
        new Vault(usdc, vm.addr(KEY), governance, multisig, 1e18, delay + 1, WINDOW, delay + 1);
    }

    function testTheExitStartsOnlyOnceNoCheckpointHasBeenPostedForTheExitPeriod() public {
        ExitVerifier.ExitTerms memory terms = _addTerms(0, 0, 0);
        _post();
        bytes32[] memory siblings = _siblings(keccak256(abi.encodePacked(uint8(24))));
        uint256 due = block.timestamp + PERIOD;
        vm.warp(due - 1);
        vm.expectRevert(abi.encodeWithSelector(Vault.ExitNotDue.selector, due));
        vault.startExit(terms, siblings);

        // A checkpoint posted in time restarts the period.
        _post();
        vm.warp(due);
        vm.expectRevert(abi.encodeWithSelector(Vault.ExitNotDue.selector, due - 1 + PERIOD));
        vault.startExit(terms, siblings);
        vm.warp(due - 1 + PERIOD);
        vm.prank(makeAddr("anyone"));
        vault.startExit(terms, siblings);
        assertEq(vault.exitStartedAt(), block.timestamp);
        vm.expectRevert(Vault.ExitActive.selector);
        vault.startExit(terms, siblings);
    }

    function testTheExitTermsMustProveAgainstTheLatestRoot() public {
        _addTerms(2, 5 * UNIT, 0);
        _post();
        vm.warp(block.timestamp + PERIOD);
        bytes32[] memory siblings = _siblings(keccak256(abi.encodePacked(uint8(24))));
        vm.expectRevert(Vault.InvalidExitProof.selector);
        vault.startExit(ExitVerifier.ExitTerms(3, 5 * UNIT, 0, 0, new uint64[](0)), siblings);
        vault.startExit(ExitVerifier.ExitTerms(2, 5 * UNIT, 0, 0, new uint64[](0)), siblings);
    }

    function testTheExitPermanentlyStopsSignedPathsAndDeposits() public {
        _deposit(funder, alice, 100e6);
        _addTerms(1, 0, 0);
        _addAccount(alice, 100e6 * UNIT);
        _post();
        _startExit(ExitVerifier.ExitTerms(1, 0, 0, 0, new uint64[](0)));

        bytes32 latest = vault.latestRoot();
        vm.expectRevert(Vault.ExitActive.selector);
        vault.postCheckpoint(seq + 1, bytes32(uint256(1)), true, seq, latest, "");
        Vault.Withdrawal memory withdrawal = Vault.Withdrawal(alice, 1e6, 1, 1, 1, 0, 86400);
        vm.expectRevert(Vault.ExitActive.selector);
        vault.withdraw(withdrawal, seq, new bytes32[](0), "");
        vm.expectRevert(Vault.ExitActive.selector);
        vault.claimDistribution(alice, 1, new bytes32[](0));
        usdc.mint(funder, 10e6);
        vm.startPrank(funder);
        usdc.approve(address(vault), 10e6);
        vm.expectRevert(Vault.CheckpointsStale.selector);
        vault.deposit(10e6);
        vm.expectRevert(Vault.CheckpointsStale.selector);
        vault.depositToHouse(10e6);
        vm.stopPrank();
        vm.expectRevert(Vault.CheckpointsStale.selector);
        vault.bookSurplus();
    }

    function testDepositsAreRefusedOnceCheckpointsAreStaleUntilTheNextOne() public {
        vm.warp(block.timestamp + STALE - 1);
        _deposit(funder, alice, 10e6);
        vm.warp(block.timestamp + 1);
        assertTrue(vault.checkpointsStale());
        usdc.mint(funder, 10e6);
        vm.prank(funder);
        usdc.approve(address(vault), 10e6);
        vm.prank(funder);
        vm.expectRevert(Vault.CheckpointsStale.selector);
        vault.depositFor(alice, 10e6);
        _addTerms(0, 0, 0);
        _post();
        assertFalse(vault.checkpointsStale());
        _deposit(funder, alice, 10e6);
    }

    function testAnAccountRegistersItsEquityOnceAndIsPaidAfterTheWindowWhoeverRelays() public {
        _deposit(funder, alice, 300e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        ExitVerifier.AccountState memory state = _addAccount(alice, 250e6 * UNIT + 999);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        state = _withAccountProof(alice, state);
        vm.expectEmit(address(vault));
        emit Vault.ExitClaimRegistered(alice, 250e6);
        vm.prank(makeAddr("relayer"));
        vault.registerExit(alice, state, positions, markets);
        vm.expectRevert(Vault.AlreadyRegistered.selector);
        vault.registerExit(alice, state, positions, markets);

        vm.expectRevert(abi.encodeWithSelector(Vault.ClaimWindowOpen.selector, block.timestamp + WINDOW));
        vault.claimExit(alice);
        vm.warp(block.timestamp + WINDOW);
        vm.expectRevert(Vault.ClaimWindowClosed.selector);
        vault.registerExit(bob, state, positions, markets);
        vm.prank(makeAddr("relayer"));
        assertEq(vault.claimExit(alice), 250e6);
        assertEq(usdc.balanceOf(alice), 250e6);
        vm.expectRevert(Vault.NoExitClaim.selector);
        vault.claimExit(alice);
        assertEq(vault.houseRemainder(), 50e6);
    }

    function testAForgedBalanceOrAnotherAccountsLeafRegistersNothing() public {
        ExitVerifier.ExitTerms memory terms = _addTerms(0, 0, 0);
        ExitVerifier.AccountState memory state = _addAccount(alice, 10e6 * UNIT);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        state = _withAccountProof(alice, state);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        vault.registerExit(bob, state, positions, markets);
        state.balance += 1;
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        vault.registerExit(alice, state, positions, markets);
        state.balance -= 1;
        state.openPositions = 1;
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        vault.registerExit(alice, state, positions, markets);
        assertFalse(vault.exitAccountRegistered(alice));
    }

    function testEveryPositionMustBeProvenSoNoLosingOneCanBeLeftOut() public {
        // A winner in market 1 (+50) and a loser in market 2 (-80) on a balance of 100.
        ExitVerifier.ExitTerms memory terms = _addTerms(0, 0, 0);
        ExitVerifier.AccountState memory state;
        state.balance = 100e6 * UNIT;
        state.openPositions = 2;
        _leaf(_accountKey(alice), _accountHash(state));
        ExitVerifier.PositionState[] memory both = new ExitVerifier.PositionState[](2);
        ExitVerifier.MarketState[] memory marks = new ExitVerifier.MarketState[](2);
        for (uint256 i; i < 2; ++i) {
            uint32 market = uint32(i + 1);
            both[i] = ExitVerifier.PositionState(market, 1e9, 100e9, new bytes32[](0));
            _leaf(
                keccak256(abi.encodePacked(uint8(7), alice, market)), keccak256(abi.encode(int128(1e9), int128(100e9)))
            );
            marks[i].market = market;
            marks[i].marked = true;
            marks[i].mark = i == 0 ? int128(150e9) : int128(20e9);
            _leaf(keccak256(abi.encodePacked(uint8(2), market)), _usdMarketHash(marks[i].mark));
        }
        bytes32 root = _post();
        state = _withAccountProof(alice, state);
        for (uint256 i; i < 2; ++i) {
            both[i].siblings = _siblings(keccak256(abi.encodePacked(uint8(7), alice, uint32(i + 1))));
            marks[i].siblings = _siblings(keccak256(abi.encodePacked(uint8(2), uint32(i + 1))));
        }
        ExitVerifier verifier = vault.exitVerifier();
        assertEq(verifier.accountClaim(root, alice, state, both, marks), 70e6);

        ExitVerifier.PositionState[] memory winner = new ExitVerifier.PositionState[](1);
        ExitVerifier.MarketState[] memory winnerMark = new ExitVerifier.MarketState[](1);
        (winner[0], winnerMark[0]) = (both[0], marks[0]);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.accountClaim(root, alice, state, winner, winnerMark);
        ExitVerifier.PositionState[] memory twice = new ExitVerifier.PositionState[](2);
        ExitVerifier.MarketState[] memory twiceMarks = new ExitVerifier.MarketState[](2);
        (twice[0], twice[1], twiceMarks[0], twiceMarks[1]) = (both[0], both[0], marks[0], marks[0]);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.accountClaim(root, alice, state, twice, twiceMarks);
        (twice[0], twice[1], twiceMarks[0], twiceMarks[1]) = (both[1], both[0], marks[1], marks[0]);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.accountClaim(root, alice, state, twice, twiceMarks);
        (twiceMarks[0], twiceMarks[1]) = (marks[0], marks[0]);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.accountClaim(root, alice, state, both, twiceMarks);

        _startExit(terms);
        vault.registerExit(alice, state, both, marks);
        assertEq(vault.exitClaims(alice), 70e6);
    }

    function testUnpaidApprovalsAndQueuedRemaindersAddToTheClaimOnceAndPaidOnesNever() public {
        _deposit(funder, alice, 1_000e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        ExitVerifier.AccountState memory state = _addAccount(alice, -5 * UNIT);
        ExitVerifier.Approval memory unpaid = _addApproval(alice, 40e6, 2);
        ExitVerifier.QueuedWithdrawal memory queued = _addQueued(alice, 60e6 * UNIT, 9);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, state), positions, markets);
        assertEq(vault.exitClaims(alice), 0, "a negative equity claims nothing");

        bytes32[] memory approvalProof = _siblings(keccak256(abi.encodePacked(uint8(32), alice, uint64(2))));
        ExitVerifier.Approval memory forged = ExitVerifier.Approval(alice, 41e6, 2, 1);
        vm.expectRevert(Vault.InvalidApprovalProof.selector);
        vault.registerExitWithdrawal(forged, approvalProof);
        vault.registerExitWithdrawal(unpaid, approvalProof);
        assertTrue(vault.usedNonces(alice, 2));
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.registerExitWithdrawal(unpaid, approvalProof);

        bytes32[] memory queueProof = _siblings(keccak256(abi.encodePacked(uint8(33), uint64(9))));
        vault.registerExitQueued(queued, queueProof);
        vm.expectRevert(Vault.AlreadyRegistered.selector);
        vault.registerExitQueued(queued, queueProof);
        assertEq(vault.exitClaims(alice), 100e6);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 100e6);
    }

    function testAnApprovalTheVaultAlreadyPaidCannotRegister() public {
        _deposit(funder, alice, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        ExitVerifier.Approval memory approval = _addApproval(alice, 40e6, 1);
        _post();
        uint64 issued = uint64(block.timestamp);
        Vault.Withdrawal memory signed = Vault.Withdrawal(alice, 40e6, 1, 1, 1, issued, issued + 86400);
        bytes32[] memory proof = _siblings(keccak256(abi.encodePacked(uint8(32), alice, uint64(1))));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, vault.withdrawalDigest(signed));
        vault.withdraw(signed, seq, proof, abi.encodePacked(r, s, v));
        _startExit(terms);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.registerExitWithdrawal(approval, proof);
    }

    function testAShortfallPaysEveryClientTheSameFractionAndStakersOnlyWhatClientsLeave() public {
        // The vault holds 150 against client claims of 100 and 200 and a distribution of 30.
        _deposit(funder, alice, 150e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        ExitVerifier.AccountState memory a = _addAccount(alice, 100e6 * UNIT);
        ExitVerifier.AccountState memory b = _addAccount(bob, 200e6 * UNIT);
        _addDistribution(alice, 30e6);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vault.registerExit(bob, _withAccountProof(bob, b), positions, markets);
        vault.registerExitDistribution(alice, 30e6, _siblings(keccak256(abi.encodePacked(uint8(13), alice))));
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 50e6);
        assertEq(vault.claimExit(bob), 100e6);
        assertEq(vault.exitClientPool(), 150e6);
        assertEq(vault.exitDistributionPool(), 0);
        assertEq(vault.houseRemainder(), 0);
    }

    function testASolventExitPaysClaimsInFullThenDistributionsThenReleasesTheRestThroughTheTimelock() public {
        _deposit(funder, alice, 500e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        ExitVerifier.AccountState memory a = _addAccount(alice, 100e6 * UNIT);
        _addDistribution(bob, 80e6);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vault.registerExitDistribution(bob, 80e6, _siblings(keccak256(abi.encodePacked(uint8(13), bob))));
        vm.warp(block.timestamp + WINDOW);

        vm.expectRevert(Vault.UnauthorizedGovernance.selector);
        vault.releaseHouseRemainder(funder);
        address treasury = makeAddr("treasury");
        _govern(address(vault), abi.encodeCall(Vault.releaseHouseRemainder, (treasury)));
        assertEq(usdc.balanceOf(treasury), 320e6);
        assertEq(vault.claimExit(alice), 100e6);
        assertEq(vault.claimExit(bob), 80e6);
        assertEq(usdc.balanceOf(address(vault)), 0);
    }

    function testNeitherThePauseNorASignerRotationStopsTheExit() public {
        _deposit(funder, alice, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        ExitVerifier.AccountState memory a = _addAccount(alice, 100e6 * UNIT);
        _post();
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (true, true)));
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (makeAddr("new signer"))));
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 100e6);
    }

    function testDepositsAfterTheCursorAreRefundedToTheirPayerOnceAndSurplusesNever() public {
        _deposit(funder, alice, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0);
        _post();
        uint256 late = _deposit(funder, bob, 30e6);
        usdc.mint(address(vault), 10e6);
        uint256 surplus = vault.bookSurplus();
        _startExit(terms);
        assertEq(vault.exitRefundReserve(), 30e6);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 0));
        vault.refundDeposit(0);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, surplus));
        vault.refundDeposit(surplus);
        vault.refundDeposit(late);
        assertEq(usdc.balanceOf(funder), 30e6, "the payer, not the beneficiary, is refunded");
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, late));
        vault.refundDeposit(late);
        assertEq(vault.exitRefundReserve(), 0);
    }

    function testADepositIdTheRootHoldsPastAGapIsNeverRefundedAndTheGapIs() public {
        // A reorganisation moved alice's credited deposit to id 1, behind bob's id 0, which the root
        // does not hold yet; a pending deposit at id 2 is past the gap too.
        uint256 gap = _deposit(funder, bob, 40e6);
        uint256 moved = _deposit(alice, alice, 100e6);
        uint256 pendingId = _deposit(funder, bob, 20e6);
        uint64[] memory gaps = new uint64[](1);
        gaps[0] = uint64(gap);
        ExitVerifier.ExitTerms memory terms = _addTerms(3, 20e6 * UNIT, 0, 40e6 * UNIT, gaps);
        ExitVerifier.AccountState memory a = _addAccount(alice, 100e6 * UNIT);
        bytes memory chainRef = bytes("pending past the gap");
        ExitVerifier.PendingDeposit memory pending = _addPending(chainRef, uint64(pendingId), bob, funder, 20e6 * UNIT);
        _post();
        _startExit(terms);
        assertTrue(
            vault.exitRefundableGap(gap) && !vault.exitRefundableGap(moved) && !vault.exitRefundableGap(pendingId)
        );
        assertEq(vault.exitRefundReserve(), 60e6, "the gap and the pending deposit, once each");
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, moved));
        vault.refundDeposit(moved);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, pendingId));
        vault.refundDeposit(pendingId);
        vault.refundDeposit(gap);
        vault.refundPendingDeposit(chainRef, pending, _siblings(keccak256(abi.encodePacked(uint8(34), chainRef))));
        assertEq(usdc.balanceOf(funder), 60e6);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 100e6, "the moved deposit is paid once, as the balance it is");
        assertEq(usdc.balanceOf(address(vault)), 0);
    }

    function testUnheldPrincipalPastTheNamedGapsStaysInTheVault() public {
        // The root names one gap and commits more unheld principal than it holds: the gap refunds by
        // id out of the reserve, and the unnamed principal stays reserved, never entering the pools.
        uint256 gap = _deposit(funder, bob, 40e6);
        _deposit(alice, alice, 100e6);
        uint64[] memory gaps = new uint64[](1);
        gaps[0] = uint64(gap);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 0, 0, 70e6 * UNIT, gaps);
        ExitVerifier.AccountState memory a = _addAccount(alice, 100e6 * UNIT);
        _post();
        _startExit(terms);
        assertEq(vault.exitRefundReserve(), 70e6);
        vault.refundDeposit(gap);
        assertEq(vault.exitRefundReserve(), 30e6, "the unnamed principal stays reserved");
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 70e6, "the claim pays from what the unnamed principal left");
        vm.prank(address(governance));
        vm.expectRevert(abi.encodeWithSelector(Vault.NoHouseRemainder.selector));
        vault.releaseHouseRemainder(makeAddr("treasury"));
        assertEq(usdc.balanceOf(address(vault)), 30e6, "the unnamed principal is never paid out");
    }

    function testADistributionPaidBeforeTheExitIsDeductedFromItsClaim() public {
        // Bob is paid 30 of a 50 distribution while the engine runs; carol all of her 30.
        usdc.mint(address(vault), 200e6);
        _addTerms(0, 0, 0);
        _addDistribution(bob, 30e6);
        _addDistribution(carol, 30e6);
        _post();
        vault.claimDistribution(bob, 30e6, _siblings(keccak256(abi.encodePacked(uint8(13), bob))));
        vault.claimDistribution(carol, 30e6, _siblings(keccak256(abi.encodePacked(uint8(13), carol))));
        delete keys;
        delete values;
        ExitVerifier.ExitTerms memory terms = _addTerms(0, 0, 0);
        _addDistribution(bob, 50e6);
        _addDistribution(carol, 30e6);
        _post();
        _startExit(terms);
        vault.registerExitDistribution(bob, 50e6, _siblings(keccak256(abi.encodePacked(uint8(13), bob))));
        vault.registerExitDistribution(carol, 30e6, _siblings(keccak256(abi.encodePacked(uint8(13), carol))));
        assertEq(vault.exitDistributionClaims(bob), 20e6);
        assertEq(vault.exitDistributionClaims(carol), 0);
        assertEq(vault.totalExitDistributionClaims(), 20e6);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(bob), 20e6);
        vm.expectRevert(Vault.NoExitClaim.selector);
        vault.claimExit(carol);
        assertEq(usdc.balanceOf(bob), 50e6);
        assertEq(usdc.balanceOf(carol), 30e6);
    }

    function testWithNoCheckpointEveryDepositIsRefundedAndNothingRegisters() public {
        uint256 first = _deposit(funder, alice, 50e6);
        uint256 second = _deposit(alice, alice, 20e6);
        vm.warp(block.timestamp + PERIOD);
        vault.startExit(ExitVerifier.ExitTerms(9, 9, 9, 9, new uint64[](0)), new bytes32[](0));
        assertEq(vault.exitDepositCursor(), 0);
        assertEq(vault.exitRefundReserve(), 70e6);
        assertEq(vault.exitHeldReserve(), 0);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        ExitVerifier.AccountState memory state;
        vm.expectRevert(Vault.NoCheckpoint.selector);
        vault.registerExit(alice, state, positions, markets);
        vault.refundDeposit(first);
        vault.refundDeposit(second);
        assertEq(usdc.balanceOf(funder), 50e6);
        assertEq(usdc.balanceOf(alice), 20e6);
    }

    function testAPendingDepositIsRefundedOnlyAsTheRootAndTheVaultRecordBothHoldIt() public {
        _deposit(funder, alice, 100e6);
        uint256 pendingId = _deposit(funder, bob, 40e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 40e6 * UNIT, 0);
        bytes memory chainRef = bytes("00000000000000000101:0xblock:0xtx:3");
        ExitVerifier.PendingDeposit memory pending = _addPending(chainRef, uint64(pendingId), bob, funder, 40e6 * UNIT);
        _post();
        _startExit(terms);
        assertEq(vault.exitRefundReserve(), 40e6);
        bytes32[] memory proof = _siblings(keccak256(abi.encodePacked(uint8(34), chainRef)));
        ExitVerifier.PendingDeposit memory other = ExitVerifier.PendingDeposit(uint64(pendingId), bob, bob, 40e6 * UNIT);
        vm.expectRevert(Vault.InvalidExitProof.selector);
        vault.refundPendingDeposit(chainRef, other, proof);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, pendingId));
        vault.refundDeposit(pendingId);
        vault.refundPendingDeposit(chainRef, pending, proof);
        assertEq(usdc.balanceOf(funder), 40e6);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, pendingId));
        vault.refundPendingDeposit(chainRef, pending, proof);
    }

    function testAPendingLeafWhoseAmountOrPayerDiffersFromTheVaultsRecordRefundsNothing() public {
        _deposit(funder, alice, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 141e6 * UNIT, 0);
        (bytes memory amountRef, bytes memory payerRef) = (bytes("another amount"), bytes("another payer"));
        ExitVerifier.PendingDeposit memory amount = _addPending(amountRef, 0, alice, funder, 41e6 * UNIT);
        ExitVerifier.PendingDeposit memory payer = _addPending(payerRef, 0, alice, bob, 100e6 * UNIT);
        _post();
        _startExit(terms);
        vm.expectRevert(Vault.PendingDepositMismatch.selector);
        vault.refundPendingDeposit(amountRef, amount, _siblings(keccak256(abi.encodePacked(uint8(34), amountRef))));
        vm.expectRevert(Vault.PendingDepositMismatch.selector);
        vault.refundPendingDeposit(payerRef, payer, _siblings(keccak256(abi.encodePacked(uint8(34), payerRef))));
        assertFalse(vault.depositRefunded(0));
    }

    function testHeldPrincipalAndRefundsStayOutOfThePoolsAndTheRemainder() public {
        // 100 credited, 30 held, 20 pending, and 10 deposited after the root.
        _deposit(funder, alice, 100e6);
        _deposit(makeAddr("sanctioned"), alice, 30e6);
        uint256 pendingId = _deposit(funder, bob, 20e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(3, 20e6 * UNIT, 30e6 * UNIT);
        ExitVerifier.AccountState memory a = _addAccount(alice, 60e6 * UNIT);
        bytes memory chainRef = bytes("pending");
        ExitVerifier.PendingDeposit memory pending = _addPending(chainRef, uint64(pendingId), bob, funder, 20e6 * UNIT);
        _post();
        uint256 late = _deposit(bob, bob, 10e6);
        _startExit(terms);
        assertEq(vault.exitRefundReserve(), 30e6);
        assertEq(vault.exitHeldReserve(), 30e6);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 60e6);
        assertEq(vault.houseRemainder(), 40e6);
        _govern(address(vault), abi.encodeCall(Vault.releaseHouseRemainder, (makeAddr("treasury"))));
        vault.refundDeposit(late);
        vault.refundPendingDeposit(chainRef, pending, _siblings(keccak256(abi.encodePacked(uint8(34), chainRef))));
        assertEq(usdc.balanceOf(address(vault)), 30e6, "only the held principal stays");
    }

    function testRegistrationAndPaymentNeedTheExitToHaveStarted() public {
        _addTerms(0, 0, 0);
        _post();
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        ExitVerifier.AccountState memory state;
        vm.expectRevert(Vault.ExitNotStarted.selector);
        vault.registerExit(alice, state, positions, markets);
        vm.expectRevert(Vault.ExitNotStarted.selector);
        vault.claimExit(alice);
        vm.expectRevert(Vault.ExitNotStarted.selector);
        vault.refundDeposit(0);
        TimelockController timelock = governance;
        vm.prank(address(timelock));
        vm.expectRevert(Vault.ExitNotStarted.selector);
        vault.releaseHouseRemainder(alice);
    }

    /// @dev A USD market leaf marked at `mark`, with no client aggregates or state hash.
    function _usdMarketHash(int128 mark) private pure returns (bytes32) {
        return
            keccak256(abi.encode(true, mark, uint8(0), uint32(0), false, int128(0), int128(0), int128(0), bytes32(0)));
    }
}
