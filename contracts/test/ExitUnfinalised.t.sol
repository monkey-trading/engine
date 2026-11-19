// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {Vault} from "../src/Vault.sol";
import {ExitTest} from "./helpers/ExitTest.sol";

/// The exit pays an account's unfinalised principal only where the vault's own deposit records hold
/// it, and refunds the vault's deposit at an id the root's receipt there does not match. Each root
/// here is one the engine commits from `V77`: an unfinalised credit leaf per credited receipt still
/// awaiting finality, and receipt-backed held deposits in the same ABI form, keyed by reference.
contract ExitUnfinalisedTest is ExitTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal mallory = makeAddr("mallory");
    address internal dave = makeAddr("dave");

    /// Mallory's deposit at id 1 was credited on sequencer confirmation and the root holding it was
    /// posted; a reorganisation then orphaned it for good, and the engine stopped before it sequenced
    /// the reversal. The vault holds only carol's finalised 100 USDC, and pays mallory none of it.
    function testTheExitPaysNothingForUnfinalisedPrincipalTheVaultDoesNotHold() public {
        assertEq(_deposit(carol, carol, 100e6), 0);
        // Base 1 (carol's deposit is final), cursor 2 and mallory's 100 live above the frontier.
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 1, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory c = _addAccount(carol, 100e6 * UNIT);
        ExitVerifier.AccountState memory m = _addUnfinalised(mallory, 100e6 * UNIT, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory orphan = _addCredit(bytes("orphaned"), 1, mallory, mallory, 100e6 * UNIT);
        _post();
        assertEq(vault.depositCount(), 1, "the canonical vault never received mallory's deposit");
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(carol, _withAccountProof(carol, c), positions, markets);
        vault.registerExit(mallory, _withAccountProof(mallory, m), positions, markets);
        vault.registerExitCredits(mallory, _one(_proven(orphan, CREDIT)));
        assertEq(vault.exitClaims(mallory), 0, "principal the vault does not hold is not paid");
        vm.warp(block.timestamp + WINDOW);
        vm.expectRevert(Vault.NoExitClaim.selector);
        vault.claimExit(mallory);
        assertEq(vault.claimExit(carol), 100e6, "carol is paid in full");
    }

    /// As above, but dave deposited after the root, before checkpoints went stale, and his deposit took
    /// id 1, the id the root holds mallory's orphaned credit at. It is below the cursor, so the cursor
    /// rule does not refund it, and the root credits nobody with it: it is refunded to dave as an
    /// unmatched receipt's id, ahead of the claims.
    function testADepositAtTheIdAnOrphanedCreditHeldIsRefundedToItsPayer() public {
        _deposit(carol, carol, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 1, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory c = _addAccount(carol, 100e6 * UNIT);
        ExitVerifier.AccountState memory m = _addUnfinalised(mallory, 100e6 * UNIT, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory orphan = _addCredit(bytes("orphaned"), 1, mallory, mallory, 100e6 * UNIT);
        _post();
        assertEq(_deposit(dave, dave, 100e6), 1);
        _startExit(terms);
        assertEq(vault.exitDepositCursor(), 2);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 1));
        vault.refundDeposit(1);
        orphan = _proven(orphan, CREDIT);
        vault.refundReceipt(orphan, CREDIT);
        assertEq(usdc.balanceOf(dave), 100e6, "dave's deposit goes back to him");
        assertEq(vault.exitRefundReserve(), 0);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 1));
        vault.refundReceipt(orphan, CREDIT);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(carol, _withAccountProof(carol, c), positions, markets);
        vault.registerExit(mallory, _withAccountProof(mallory, m), positions, markets);
        vault.registerExitCredits(mallory, _one(orphan));
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(carol), 100e6);
        assertEq(vault.exitClaims(mallory), 0);
        assertEq(usdc.balanceOf(address(vault)), 0);
    }

    /// The account's unfinalised principal joins its claim only through its own credit leaves, each
    /// once, after its equity is registered: one the vault's records match pays, while another
    /// account's credit or a pending leaf offered as a credit is refused. A registrant that leaves the
    /// credits out lowers nothing, since anyone may add them later.
    function testUnfinalisedPrincipalTheVaultHoldsIsPaidOnlyThroughItsOwnCreditsEachOnce() public {
        _deposit(alice, alice, 100e6);
        _deposit(bob, bob, 40e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 0, 0, 0, 0, 140e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 100e6 * UNIT, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory credit = _addCredit(bytes("alice's"), 0, alice, alice, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory other = _addCredit(bytes("bob's"), 1, bob, bob, 40e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory pending = _addPending(bytes("pending"), 0, alice, alice, 100e6 * UNIT);
        _post();
        (credit, other, pending) = (_proven(credit, CREDIT), _proven(other, CREDIT), _proven(pending, PENDING));
        _startExit(terms);
        vm.expectRevert(abi.encodeWithSelector(ExitVerifier.UnregisteredAccount.selector, alice));
        vault.registerExitCredits(alice, _one(credit));
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        assertEq(vault.exitClaims(alice), 0, "the principal waits for its credit");
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        vault.registerExitCredits(alice, _one(other));
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        vault.registerExitCredits(alice, _one(pending));
        vault.registerExitCredits(alice, _one(credit));
        assertEq(vault.exitClaims(alice), 100e6);
        vault.registerExitCredits(alice, _one(credit));
        assertEq(vault.exitClaims(alice), 100e6, "a credit already counted adds nothing again");
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 0));
        vault.refundReceipt(credit, CREDIT);
        vm.warp(block.timestamp + WINDOW);
        vm.expectRevert(Vault.ClaimWindowClosed.selector);
        vault.registerExitCredits(bob, _one(other));
    }

    /// Alice's realised losses left her balance 80 below the 100 of unfinalised principal it holds.
    /// Her two credits register in separate batches: the first recovers the shortfall, the second
    /// brings her claim to the equity the engine values, 20.
    function testCreditsRegisterInBatchesAndFirstRecoverTheEquitysShortfall() public {
        _deposit(alice, alice, 60e6);
        _deposit(alice, alice, 40e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 0, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 20e6 * UNIT, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory first = _addCredit(bytes("first"), 0, alice, alice, 60e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory second = _addCredit(bytes("second"), 1, alice, alice, 40e6 * UNIT);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vault.registerExitCredits(alice, _one(_proven(first, CREDIT)));
        assertEq(vault.exitClaims(alice), 0, "60 recovers 60 of the 80 short");
        vault.registerExitCredits(alice, _one(_proven(second, CREDIT)));
        assertEq(vault.exitClaims(alice), 20e6);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(alice), 20e6);
    }

    /// A relayer funded alice's credited deposit at id 0, which a reorganisation orphaned; its next
    /// deposit, of the same amount for bob, took id 0. The record matches alice's credit in payer and
    /// amount but not in beneficiary, so alice's claim does not take it, and it goes back to the
    /// relayer that paid it.
    function testACreditDoesNotTakeTheSamePayersDepositForAnotherAccount() public {
        address relayer = makeAddr("relayer");
        assertEq(_deposit(relayer, bob, 100e6), 0);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 100e6 * UNIT, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory credit = _addCredit(bytes("orphaned"), 0, alice, relayer, 100e6 * UNIT);
        _post();
        _startExit(terms);
        credit = _proven(credit, CREDIT);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vault.registerExitCredits(alice, _one(credit));
        assertEq(vault.exitClaims(alice), 0);
        vault.refundReceipt(credit, CREDIT);
        assertEq(usdc.balanceOf(relayer), 100e6);
    }

    /// A reorganisation moved alice's credited deposit from id 1 to id 0, orphaning bob's at id 0, and
    /// the node stopped before it applied any of it. Neither credit's record matches: neither claim
    /// pays its principal, the vault's deposit at id 0 goes back to alice once, and the id past the
    /// vault's count refunds nothing.
    function testAMovedCreditIsRefundedOnceAtItsNewIdAndNeitherClaimPaysIt() public {
        assertEq(_deposit(alice, alice, 100e6), 0);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 0, 0, 0, 0, 140e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 100e6 * UNIT, 100e6 * UNIT);
        ExitVerifier.AccountState memory b = _addUnfinalised(bob, 40e6 * UNIT, 40e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory moved = _addCredit(bytes("alice's"), 1, alice, alice, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory orphan = _addCredit(bytes("bob's"), 0, bob, bob, 40e6 * UNIT);
        _post();
        _startExit(terms);
        assertEq(vault.exitDepositCursor(), 1, "the cursor stops at the vault's count");
        (moved, orphan) = (_proven(moved, CREDIT), _proven(orphan, CREDIT));
        // Id 1 is past the vault's count: no deposit there to refund, and nothing reserved for a credit.
        vault.refundReceipt(moved, CREDIT);
        assertEq(usdc.balanceOf(alice), 0);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 1));
        vault.refundReceipt(moved, CREDIT);
        vault.refundReceipt(orphan, CREDIT);
        assertEq(usdc.balanceOf(alice), 100e6);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vault.registerExit(bob, _withAccountProof(bob, b), positions, markets);
        vault.registerExitCredits(alice, _one(moved));
        vault.registerExitCredits(bob, _one(orphan));
        assertEq(vault.totalExitClaims(), 0);
        assertEq(usdc.balanceOf(address(vault)), 0);
    }

    /// A held receipt at id 1, whose payer was sanctioned, was orphaned by a reorganisation, and dave's
    /// later deposit took id 1. The held reserve kept the receipt's principal; dave's deposit is
    /// refunded and that share of the held reserve released, while a held receipt the vault does hold
    /// stays held.
    function testADepositAtTheIdAnOrphanedHeldReceiptHeldIsRefundedAndItsHeldShareReleased() public {
        address sanctioned = makeAddr("sanctioned");
        _deposit(carol, carol, 100e6);
        _deposit(sanctioned, alice, 30e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(3, 1, 0, 60e6 * UNIT, 0, 60e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory c = _addAccount(carol, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory kept = _addHeld(bytes("kept"), 1, alice, sanctioned, 30e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory orphan = _addHeld(bytes("orphaned"), 2, alice, sanctioned, 30e6 * UNIT);
        _post();
        assertEq(_deposit(dave, dave, 30e6), 2);
        _startExit(terms);
        assertEq(vault.exitHeldReserve(), 60e6);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 1));
        vault.refundReceipt(_proven(kept, HELD), HELD);
        vault.refundReceipt(_proven(orphan, HELD), HELD);
        assertEq(usdc.balanceOf(dave), 30e6);
        assertEq(vault.exitHeldReserve(), 30e6, "only the held receipt the vault holds stays held");
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(carol, _withAccountProof(carol, c), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(carol), 100e6);
        assertEq(usdc.balanceOf(address(vault)), 30e6);
    }

    /// Credits register in overlapping batches: one already counted is passed over, so a batch that
    /// repeats it still counts the rest.
    function testABatchRepeatingACountedCreditStillCountsTheRest() public {
        _deposit(alice, alice, 10e6);
        _deposit(alice, alice, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 0, 0, 0, 0, 110e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 110e6 * UNIT, 110e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory small = _addCredit(bytes("small"), 0, alice, alice, 10e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory large = _addCredit(bytes("large"), 1, alice, alice, 100e6 * UNIT);
        _post();
        _startExit(terms);
        (small, large) = (_proven(small, CREDIT), _proven(large, CREDIT));
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(alice, _withAccountProof(alice, a), positions, markets);
        vault.registerExitCredits(alice, _one(small));
        ExitVerifier.ReceiptLeaf[] memory both = new ExitVerifier.ReceiptLeaf[](2);
        (both[0], both[1]) = (small, large);
        vault.registerExitCredits(alice, both);
        assertEq(vault.exitClaims(alice), 110e6);
    }

    /// A booked surplus carries no payer, so the engine holds it for good, and the vault records it
    /// too: its held receipt above the finalised frontier matches the vault's record, and its
    /// principal stays held.
    function testABookedSurplussHeldReceiptAboveTheFrontierStaysHeld() public {
        _surplusStaysHeld(1);
    }

    /// As above, with the surplus below the finalised frontier.
    function testABookedSurplussHeldReceiptBelowTheFrontierStaysHeld() public {
        _surplusStaysHeld(2);
    }

    /// The root holds a booked surplus at id 1 that a reorganisation removed with its funding, so the
    /// vault never assigned id 1. Its held leaf names no payer and no account and matches no record:
    /// its held share is released, and carol is paid in full.
    function testAnOrphanedSurplussHeldShareIsReleased() public {
        _deposit(carol, carol, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 1, 0, 100e6 * UNIT, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory c = _addAccount(carol, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory orphan = _addHeld(bytes("surplus"), 1, address(0), address(0), 100e6 * UNIT);
        _post();
        _startExit(terms);
        assertEq(vault.exitHeldReserve(), 100e6);
        vault.refundReceipt(_proven(orphan, HELD), HELD);
        assertEq(vault.exitHeldReserve(), 0, "the phantom held share is released");
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(carol, _withAccountProof(carol, c), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(carol), 100e6, "carol in full");
    }

    /// Mallory's orphaned credit held id 1, and a booked surplus took it. The surplus is no refundable
    /// deposit, so nothing is paid out; the id is marked done, and the surplus stays in what the
    /// vault holds beyond its reserves.
    function testAnOrphansIdABookedSurplusTookRefundsNothing() public {
        _deposit(carol, carol, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(2, 1, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.ReceiptLeaf memory orphan = _addCredit(bytes("orphaned"), 1, mallory, mallory, 100e6 * UNIT);
        _post();
        usdc.mint(address(vault), 30e6);
        assertEq(vault.bookSurplus(), 1);
        _startExit(terms);
        orphan = _proven(orphan, CREDIT);
        vault.refundReceipt(orphan, CREDIT);
        assertEq(vault.exitRefundReserve(), 0);
        assertTrue(vault.depositRefunded(1));
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, 1));
        vault.refundReceipt(orphan, CREDIT);
        assertEq(usdc.balanceOf(address(vault)), 130e6, "nothing left the vault");
    }

    /// An unmatched receipt's refund ranks ahead of claims, so it is taken only while they register.
    function testAnUnmatchedReceiptIsRefundedOnlyWhileClaimsRegister() public {
        _deposit(dave, dave, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 0, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.ReceiptLeaf memory orphan = _addCredit(bytes("orphaned"), 0, mallory, mallory, 100e6 * UNIT);
        _post();
        orphan = _proven(orphan, CREDIT);
        vm.expectRevert(Vault.ExitNotStarted.selector);
        vault.refundReceipt(orphan, CREDIT);
        _startExit(terms);
        vm.expectRevert(Vault.InvalidExitProof.selector);
        vault.refundReceipt(orphan, 24);
        vm.warp(block.timestamp + WINDOW);
        vm.expectRevert(Vault.ClaimWindowClosed.selector);
        vault.refundReceipt(orphan, CREDIT);
        assertEq(usdc.balanceOf(dave), 0);
    }

    /// Alice's balance is 80 below her 150 of unfinalised principal: credits at ids 0 and 1 the vault
    /// holds, and an orphaned one at id 3 it never received. Anyone may preview her claim,
    /// before the exit and after she has registered, and it is what registering comes to: 20. It
    /// registers nothing, and refuses credits out of order or another account's.
    function testAnyoneMayPreviewTheClaimRegistrationGivesBeforeAndDuringTheExit() public {
        _deposit(alice, alice, 60e6);
        _deposit(alice, alice, 40e6);
        _deposit(bob, bob, 10e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(3, 0, 0, 0, 0, 110e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 70e6 * UNIT, 150e6 * UNIT);
        ExitVerifier.ReceiptLeaf[] memory credits = new ExitVerifier.ReceiptLeaf[](3);
        credits[0] = _addCredit(bytes("first"), 0, alice, alice, 60e6 * UNIT);
        credits[1] = _addCredit(bytes("second"), 1, alice, alice, 40e6 * UNIT);
        credits[2] = _addCredit(bytes("orphaned"), 3, alice, alice, 50e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory other = _addCredit(bytes("bob's"), 2, bob, bob, 10e6 * UNIT);
        bytes32 root = _post();
        for (uint256 i; i < 3; ++i) {
            credits[i] = _proven(credits[i], CREDIT);
        }
        a = _withAccountProof(alice, a);
        ExitVerifier verifier = vault.exitVerifier();
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();

        vm.prank(makeAddr("wallet"));
        assertEq(verifier.previewClaim(root, alice, a, positions, markets, credits), 20e6);
        assertEq(verifier.previewClaim(root, alice, a, positions, markets, _one(credits[0])), 0, "60 of 80 short");
        assertFalse(verifier.exitRegistered(alice), "a preview registers nothing");
        ExitVerifier.ReceiptLeaf[] memory swapped = new ExitVerifier.ReceiptLeaf[](2);
        (swapped[0], swapped[1]) = (credits[1], credits[0]);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, swapped);
        (swapped[0], swapped[1]) = (credits[0], credits[0]);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, swapped);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, _one(_proven(other, CREDIT)));
        a.balance += 1;
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, credits);
        a.balance -= 1;

        _startExit(terms);
        vault.registerExit(alice, a, positions, markets);
        assertEq(vault.exitClaims(alice), 0, "her equity alone is 80 short, which only the preview sees past");
        vault.registerExitCredits(alice, credits);
        assertEq(vault.exitClaims(alice), 20e6);
        assertEq(verifier.previewClaim(root, alice, a, positions, markets, credits), 20e6, "it still checks after");
    }

    /// A preview refuses the account's own credit unless its leaf proves: a changed amount, a wrong
    /// sibling, or its pending leaf offered as a credit.
    function testAPreviewRefusesAnAccountsOwnCreditWhoseLeafDoesNotProve() public {
        _deposit(alice, alice, 60e6);
        _addTerms(1, 0, 0, 0, 0, 60e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addUnfinalised(alice, 60e6 * UNIT, 60e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory credit = _addCredit(bytes("alice's"), 0, alice, alice, 60e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory pending = _addPending(bytes("pending"), 0, alice, alice, 60e6 * UNIT);
        bytes32 root = _post();
        a = _withAccountProof(alice, a);
        ExitVerifier verifier = vault.exitVerifier();
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        assertEq(verifier.previewClaim(root, alice, a, positions, markets, _one(_proven(credit, CREDIT))), 60e6);

        ExitVerifier.ReceiptLeaf memory forged = _proven(credit, CREDIT);
        forged.amount += 1;
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, _one(forged));
        forged = _proven(credit, CREDIT);
        forged.siblings[0] = bytes32(uint256(forged.siblings[0]) ^ 1);
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, _one(forged));
        vm.expectRevert(ExitVerifier.InvalidAccountProof.selector);
        verifier.previewClaim(root, alice, a, positions, markets, _one(_proven(pending, PENDING)));
    }

    /// Every read a wallet makes of an account's exit proof answers any caller, as an `eth_call` with
    /// no sender does: the terms, the claim's preview, an approval, a queued request, a distribution and
    /// a pending deposit's receipt.
    function testAWalletReadsEveryPartOfAnExitProofFromAnyAddress() public {
        _deposit(alice, alice, 100e6);
        ExitVerifier.ExitTerms memory terms = _addTerms(1, 1, 0, 0, 0, 100e6 * UNIT, new uint64[](0));
        ExitVerifier.AccountState memory a = _addAccount(alice, 100e6 * UNIT);
        ExitVerifier.Approval memory approval = _addApproval(alice, 5e6, 1);
        ExitVerifier.QueuedWithdrawal memory queued = _addQueued(alice, 3e6 * UNIT, 7);
        _addDistribution(alice, 2e6);
        ExitVerifier.ReceiptLeaf memory pending = _addPending(bytes("pending"), 1, alice, alice, 10e6 * UNIT);
        bytes32 root = _post();
        ExitVerifier verifier = vault.exitVerifier();
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        a = _withAccountProof(alice, a);
        address wallet = makeAddr("wallet");

        vm.startPrank(wallet);
        assertTrue(verifier.provesTerms(root, terms, _siblings(keccak256(abi.encodePacked(uint8(24))))));
        assertEq(verifier.previewClaim(root, alice, a, positions, markets, new ExitVerifier.ReceiptLeaf[](0)), 100e6);
        assertTrue(
            verifier.provesApproval(root, approval, _siblings(keccak256(abi.encodePacked(uint8(32), alice, uint64(1)))))
        );
        assertTrue(verifier.provesQueued(root, queued, _siblings(keccak256(abi.encodePacked(uint8(33), uint64(7))))));
        assertTrue(
            verifier.provesDistribution(root, alice, 2e6, _siblings(keccak256(abi.encodePacked(uint8(13), alice))))
        );
        (bool proven,) = verifier.provesReceipt(root, _proven(pending, PENDING), PENDING);
        assertTrue(proven);
        vm.stopPrank();
    }

    /// The verifier keeps each account's running equity for the vault, which alone may change it.
    function testOnlyTheVaultRegistersClaimsThroughTheVerifier() public {
        ExitVerifier verifier = vault.exitVerifier();
        ExitVerifier.AccountState memory state;
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vm.expectRevert(ExitVerifier.NotVault.selector);
        verifier.accountClaim(bytes32(0), alice, state, positions, markets);
        vm.expectRevert(ExitVerifier.NotVault.selector);
        verifier.creditClaim(bytes32(0), alice, new ExitVerifier.ReceiptLeaf[](0));
    }

    /// A held, pending or credited receipt at dave's id that his deposit does not match, or at an id
    /// the vault never assigned, of any amount, beside an unnamed gap or none: dave is refunded his own
    /// deposit, if he made one, carol is paid in full, and the vault keeps exactly the unnamed gap's
    /// principal, whatever share of dave's deposit the terms had reserved as unattributed or held for
    /// the receipt as pending or held principal.
    function testFuzzAnUnmatchedReceiptsRefundLeavesEveryOtherObligationWhole(
        uint64 canonical,
        uint64 receipt,
        uint64 unnamed,
        uint8 kindSeed,
        bool assigned
    ) public {
        canonical = uint64(bound(canonical, 10e6, 1e12));
        receipt = uint64(bound(receipt, 10e6, 1e12));
        unnamed = uint64(bound(unnamed, 0, 1e12));
        uint8 kind = HELD + kindSeed % 3;
        _deposit(carol, carol, 100e6);
        if (unnamed >= 10e6) _deposit(makeAddr("gina"), makeAddr("gina"), unnamed);
        else unnamed = 0;
        uint64 id = assigned ? uint64(_deposit(dave, dave, canonical)) : uint64(vault.depositCount());
        if (!assigned) canonical = 0;
        int128 amount = int128(uint128(receipt)) * UNIT;
        ExitVerifier.ExitTerms memory terms = _addTerms(
            id + 1,
            1,
            kind == PENDING ? amount : int128(0),
            kind == HELD ? amount : int128(0),
            int128(uint128(unnamed)) * UNIT,
            amount,
            new uint64[](0)
        );
        ExitVerifier.AccountState memory c = _addAccount(carol, 100e6 * UNIT);
        ExitVerifier.ReceiptLeaf memory orphan = kind == HELD
            ? _addHeld(bytes("orphaned"), id, mallory, mallory, amount)
            : kind == PENDING
                ? _addPending(bytes("orphaned"), id, mallory, mallory, amount)
                : _addCredit(bytes("orphaned"), id, mallory, mallory, amount);
        _post();
        _startExit(terms);
        orphan = _proven(orphan, kind);
        vault.refundReceipt(orphan, kind);
        assertEq(usdc.balanceOf(dave), canonical, "dave's own deposit");
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, id));
        vault.refundReceipt(orphan, kind);
        assertEq(vault.exitRefundReserve(), unnamed, "only the unnamed gap stays reserved");
        assertEq(vault.exitHeldReserve(), 0, "no held share stays for the orphan");
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        vault.registerExit(carol, _withAccountProof(carol, c), positions, markets);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(carol), 100e6, "carol in full");
        assertEq(usdc.balanceOf(address(vault)), unnamed);
    }

    /// @dev Books a 50 USDC surplus at id 1 behind carol's deposit and posts a root holding it, with the
    /// finalised frontier's count `base`; its held receipt refunds nothing and stays held.
    function _surplusStaysHeld(uint64 base) private {
        _deposit(carol, carol, 100e6);
        usdc.mint(address(vault), 50e6);
        uint256 surplus = vault.bookSurplus();
        ExitVerifier.ExitTerms memory terms = _addTerms(2, base, 0, 50e6 * UNIT, 0, 50e6 * UNIT, new uint64[](0));
        ExitVerifier.ReceiptLeaf memory held =
            _addHeld(bytes("surplus"), uint64(surplus), address(0), address(0), 50e6 * UNIT);
        _post();
        _startExit(terms);
        vm.expectRevert(abi.encodeWithSelector(Vault.NotRefundable.selector, surplus));
        vault.refundReceipt(_proven(held, HELD), HELD);
        assertEq(vault.exitHeldReserve(), 50e6, "the surplus stays held");
    }

    /// @dev An account leaf with `balance`, of which `unfinalised` is unfinalised principal.
    function _addUnfinalised(address account, int128 balance, int128 unfinalised)
        private
        returns (ExitVerifier.AccountState memory state)
    {
        state.balance = balance;
        state.unfinalisedDeposits = unfinalised;
        _leaf(_accountKey(account), _accountHash(state));
    }
}
