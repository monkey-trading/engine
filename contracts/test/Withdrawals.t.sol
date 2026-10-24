// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20Errors} from "@openzeppelin-contracts/interfaces/draft-IERC6093.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import {Vault} from "../src/Vault.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";
import {FeeOnTransferToken} from "./mocks/FeeOnTransferToken.sol";
import {WithdrawalToken} from "./mocks/WithdrawalToken.sol";

contract WithdrawalsTest is WithdrawalTest {
    function test_RelayerPaysOnlyTheApprovedAccountAndEmitsReconciliationFields() public {
        Vault.Withdrawal memory withdrawal = _approval(7, 123_456_789);
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        uint256 before = usdc.balanceOf(address(vault));

        vm.expectEmit(address(vault));
        emit Vault.WithdrawalPaid(trader, withdrawal.amount, 7, withdrawal.approvedSeq);
        vm.prank(makeAddr("relayer"));
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);

        assertEq(usdc.balanceOf(trader), withdrawal.amount);
        assertEq(usdc.balanceOf(address(vault)), before - withdrawal.amount);
        assertTrue(vault.usedNonces(trader, 7));
        assertEq(vault.totalWithdrawn(), withdrawal.amount);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertEq(usdc.balanceOf(trader), withdrawal.amount);
        assertEq(vault.totalWithdrawn(), withdrawal.amount);
    }

    function test_ReissueAfterExpiryPaysOnceAndCannotReissueAPaidNonce() public {
        Vault.Withdrawal memory withdrawal = _approval(7, 10e6);
        _postApproval(withdrawal);
        bytes memory old = _sign(vault, withdrawal, KEY);
        vm.warp(withdrawal.expiresAt);
        vm.expectRevert(Vault.InvalidWithdrawalTime.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), old);
        assertFalse(vault.usedNonces(trader, 7));

        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        withdrawal.issuedAt++;
        withdrawal.expiresAt++;
        vm.warp(block.timestamp + 1);
        bytes memory reissued = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), reissued);
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function test_SameNonceIsIndependentForDifferentAccounts() public {
        Vault.Withdrawal memory first = _approval(0, 1);
        _postApproval(first);
        vault.withdraw(first, 10, new bytes32[](0), _sign(vault, first, KEY));
        Vault.Withdrawal memory second = _approval(0, 1);
        second.account = makeAddr("second account");
        _postApproval(second);
        vault.withdraw(second, 11, new bytes32[](0), _sign(vault, second, KEY));
        assertEq(usdc.balanceOf(first.account), 1);
        assertEq(usdc.balanceOf(second.account), 1);
    }

    function test_RevertWhen_AccountOrAmountIsInvalid() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 0);
        vm.expectRevert(Vault.InvalidWithdrawal.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
        withdrawal.amount = 1;
        withdrawal.account = address(0);
        vm.expectRevert(Vault.InvalidWithdrawal.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
        withdrawal.account = address(vault);
        vm.expectRevert(Vault.InvalidWithdrawal.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
    }

    function test_TimeBoundsIncludeIssueAndExcludeExpiry() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        _postApproval(withdrawal);
        uint64 issuedAt = withdrawal.issuedAt;
        withdrawal.issuedAt++;
        withdrawal.expiresAt++;
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.InvalidWithdrawalTime.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        withdrawal.issuedAt = issuedAt;
        withdrawal.expiresAt = issuedAt + 86400 - 1;
        vm.expectRevert(Vault.InvalidWithdrawalTime.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
        withdrawal.expiresAt = issuedAt + 86400 + 1;
        vm.expectRevert(Vault.InvalidWithdrawalTime.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
        withdrawal.expiresAt = issuedAt + 86400;
        vm.warp(withdrawal.expiresAt - 1);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        assertEq(usdc.balanceOf(trader), 1);
    }

    function test_TimeAdditionCannotWrapUint64() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        withdrawal.issuedAt = type(uint64).max - 1;
        withdrawal.expiresAt = 86398;
        vm.warp(withdrawal.issuedAt);
        vm.expectRevert(Vault.InvalidWithdrawalTime.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
    }

    function testFuzz_Exact24HourValidityWindow(uint64 issueSeed, uint32 deltaSeed) public {
        uint64 issuedAt = uint64(bound(issueSeed, 1, type(uint64).max - 86400));
        uint256 delta = bound(deltaSeed, 0, 86400);
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        withdrawal.issuedAt = issuedAt;
        withdrawal.expiresAt = issuedAt + 86400;
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.warp(uint256(issuedAt) + delta);
        if (delta == 86400) {
            vm.expectRevert(Vault.InvalidWithdrawalTime.selector);
            vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
            assertFalse(vault.usedNonces(trader, 0));
            assertEq(usdc.balanceOf(trader), 0);
        } else {
            vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
            assertEq(usdc.balanceOf(trader), 1);
        }
    }

    function testFuzz_OnlyCurrentEpochCanPay(uint64 epoch) public {
        vm.assume(epoch != vault.signerEpoch());
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        withdrawal.signerEpoch = epoch;
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(vault.usedNonces(trader, 0));
        assertEq(usdc.balanceOf(trader), 0);
    }

    function test_RevertWhen_SignatureIsWrongMissingOrMalleable() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        _postApproval(withdrawal);
        bytes memory forged = _sign(vault, withdrawal, KEY + 1);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), forged);
        vm.expectRevert(abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureLength.selector, 0));
        vault.withdraw(withdrawal, 10, new bytes32[](0), "");
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, _digest(vault, withdrawal));
        bytes32 highS = bytes32(0xfffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141 - uint256(s));
        bytes memory malleable = abi.encodePacked(r, highS, v == 27 ? uint8(28) : uint8(27));
        vm.expectRevert(abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureS.selector, highS));
        vault.withdraw(withdrawal, 10, new bytes32[](0), malleable);
        assertFalse(vault.usedNonces(trader, 0));
    }

    function test_RevertWhen_SignatureDomainUsesAnotherVaultOrChain() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        _postApproval(withdrawal);
        Vault other = new Vault(usdc, vm.addr(KEY), governance);
        bytes memory wrongVault = _sign(other, withdrawal, KEY);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), wrongVault);
        bytes memory wrongChain = _sign(vault, withdrawal, KEY);
        vm.chainId(block.chainid + 1);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), wrongChain);
        assertFalse(vault.usedNonces(trader, 0));
    }

    function test_InsufficientLiquidityDoesNotConsumeNonceAndCanBeRetried() public {
        uint256 amount = usdc.balanceOf(address(vault)) + 1;
        Vault.Withdrawal memory withdrawal = _approval(0, amount);
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(
            abi.encodeWithSelector(IERC20Errors.ERC20InsufficientBalance.selector, address(vault), amount - 1, amount)
        );
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(vault.usedNonces(trader, 0));
        usdc.mint(address(vault), 1);
        assertEq(vault.totalWithdrawn(), 0);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertEq(usdc.balanceOf(address(vault)), 0);
        assertEq(usdc.balanceOf(trader), amount);
        assertEq(vault.totalWithdrawn(), amount);
    }

    function test_FeeOnTransferWithdrawalRevertsWithoutBurningFundsOrNonce() public {
        FeeOnTransferToken token = new FeeOnTransferToken();
        Vault target = new Vault(token, vm.addr(KEY), governance);
        token.mint(address(target), 100e6);
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _post(target, 10, _root(withdrawal, new bytes32[](0)), KEY);
        bytes memory signature = _sign(target, withdrawal, KEY);
        vm.expectRevert(Vault.InexactWithdrawalTransfer.selector);
        target.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(target.usedNonces(trader, 0));
        assertEq(token.balanceOf(address(target)), 100e6);
        assertEq(token.balanceOf(trader), 0);
        assertEq(token.totalSupply(), 100e6);
        assertEq(target.totalWithdrawn(), 0);
    }

    function test_SenderFeeAndFailedTokenTransferPreserveNonce() public {
        WithdrawalToken token = new WithdrawalToken();
        Vault target = new Vault(token, vm.addr(KEY), governance);
        token.mint(address(target), 100e6);
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _post(target, 10, _root(withdrawal, new bytes32[](0)), KEY);
        bytes memory signature = _sign(target, withdrawal, KEY);
        token.configure(true, false, address(0), "");
        vm.expectRevert(Vault.InexactWithdrawalTransfer.selector);
        target.withdraw(withdrawal, 10, new bytes32[](0), signature);
        token.configure(false, true, address(0), "");
        vm.expectRevert(WithdrawalToken.TransferBlocked.selector);
        target.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(target.usedNonces(trader, 0));
        assertEq(token.balanceOf(address(target)), 100e6);
        assertEq(target.totalWithdrawn(), 0);
    }

    function test_OutgoingTransferCannotReenterWithdrawOrAnyDeposit() public {
        WithdrawalToken token = new WithdrawalToken();
        Vault target = new Vault(token, vm.addr(KEY), governance);
        token.mint(address(target), 100e6);
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _post(target, 10, _root(withdrawal, new bytes32[](0)), KEY);
        bytes memory signature = _sign(target, withdrawal, KEY);
        bytes[4] memory calls = [
            abi.encodeCall(Vault.withdraw, (withdrawal, uint64(10), new bytes32[](0), signature)),
            abi.encodeCall(Vault.deposit, (10e6)),
            abi.encodeCall(Vault.depositFor, (trader, 10e6)),
            abi.encodeCall(Vault.depositToHouse, (10e6))
        ];
        for (uint256 i; i < calls.length; ++i) {
            token.configure(false, false, address(target), calls[i]);
            vm.expectRevert(ReentrancyGuardTransient.ReentrancyGuardReentrantCall.selector);
            target.withdraw(withdrawal, 10, new bytes32[](0), signature);
            assertFalse(target.usedNonces(trader, 0));
            assertEq(token.balanceOf(address(target)), 100e6);
            assertEq(target.totalWithdrawn(), 0);
        }
        token.configure(false, false, address(0), "");
        target.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertEq(token.balanceOf(trader), 10e6);
        assertEq(target.totalWithdrawn(), 10e6);
    }

    function test_TotalWithdrawnAccumulatesOnlySuccessfulDistinctPayments() public {
        Vault.Withdrawal memory first = _approval(1, 10e6);
        _postApproval(first);
        vault.withdraw(first, 10, new bytes32[](0), _sign(vault, first, KEY));
        Vault.Withdrawal memory second = _approval(2, 25e6);
        _postApproval(second);
        bytes memory signature = _sign(vault, second, KEY);
        vault.withdraw(second, 11, new bytes32[](0), signature);
        assertEq(vault.totalWithdrawn(), 35e6);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(second, 11, new bytes32[](0), signature);
        assertEq(vault.totalWithdrawn(), 35e6);
        assertEq(usdc.balanceOf(trader), 35e6);
    }
}
