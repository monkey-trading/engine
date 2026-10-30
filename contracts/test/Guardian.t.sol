// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vault} from "../src/Vault.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";

/// The guardian, the deployment's multisig acting without the timelock's delay, may pause
/// withdrawals for a week and retire the signer, and nothing else.
contract GuardianTest is WithdrawalTest {
    uint8 private constant ENTITLEMENT_LEAF = 8;
    uint8 private constant DISTRIBUTION_LEAF = 13;

    function test_ConstructorRecordsTheGuardianAndRefusesAZeroOne() public {
        assertEq(vault.guardian(), multisig);
        vm.expectRevert(Vault.ZeroAddress.selector);
        new Vault(usdc, vm.addr(KEY), governance, address(0), 1e18);
    }

    function test_PauseRefusesClaimsAtOnceAndLeavesDepositsCheckpointsAndEmissions() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        address claimant = makeAddr("claimant");
        bytes32 distribution = _leaf(DISTRIBUTION_LEAF, claimant, 5e6);

        uint256 until = block.timestamp + 7 days;
        vm.expectEmit(address(vault));
        emit Vault.GuardianPausedWithdrawals(until, 1);
        vm.prank(multisig);
        vault.pauseWithdrawals();
        assertTrue(vault.withdrawalsPaused());
        assertEq(vault.withdrawalsPausedUntil(), until);
        assertEq(vault.guardianPauses(), 1);

        // Same block, no timelock delay.
        vm.expectRevert(Vault.WithdrawalsPaused.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(vault.usedNonces(trader, 0));
        assertEq(usdc.balanceOf(trader), 0);

        _post(vault, 11, distribution, KEY);
        vm.expectRevert(Vault.WithdrawalsPaused.selector);
        vault.claimDistribution(claimant, 5e6, new bytes32[](0));
        assertEq(vault.distributionsPaid(), 0);

        _post(vault, 12, _leaf(ENTITLEMENT_LEAF, claimant, 3e18), KEY);
        assertEq(vault.claimEmissions(claimant, 3e18, new bytes32[](0)), 3e18);
        assertEq(vault.monkey().balanceOf(claimant), 3e18);

        usdc.mint(trader, 10e6);
        vm.startPrank(trader);
        usdc.approve(address(vault), 10e6);
        assertEq(vault.deposit(10e6), 0);
        vm.stopPrank();
        assertFalse(vault.depositsPaused());
    }

    function test_PauseEndsAfterSevenDaysAndTheApprovalStillPays() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        uint256 start = block.timestamp;
        vm.prank(multisig);
        vault.pauseWithdrawals();

        vm.warp(start + 7 days - 1);
        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.WithdrawalsPaused.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);

        vm.warp(start + 7 days);
        assertFalse(vault.withdrawalsPaused());
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function testFuzz_PauseHoldsForExactlySevenDays(uint256 elapsed) public {
        elapsed = bound(elapsed, 0, 60 days);
        uint256 start = block.timestamp;
        vm.prank(multisig);
        vault.pauseWithdrawals();
        vm.warp(start + elapsed);
        assertEq(vault.withdrawalsPaused(), elapsed < 7 days);
    }

    function test_TimelockRenewsAPauseUntilItUnpauses() public {
        uint256 start = block.timestamp;
        vm.prank(multisig);
        vault.pauseWithdrawals();
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, true)));
        assertEq(vault.withdrawalsPausedUntil(), type(uint256).max);
        vm.warp(start + 365 days);
        assertTrue(vault.withdrawalsPaused());
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, false)));
        assertFalse(vault.withdrawalsPaused());
        assertEq(vault.guardianPauses(), 1);
    }

    function test_TimelockUnpauseEndsAPauseEarly() public {
        vm.prank(multisig);
        vault.pauseWithdrawals();
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, false)));
        assertFalse(vault.withdrawalsPaused());
        assertEq(vault.withdrawalsPausedUntil(), 0);
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function test_GuardianCannotExtendAPauseInForceButPausesAgainOnceItEnds() public {
        uint256 start = block.timestamp;
        vm.startPrank(multisig);
        vault.pauseWithdrawals();
        vm.warp(start + 7 days - 1);
        vm.expectRevert(Vault.WithdrawalsPaused.selector);
        vault.pauseWithdrawals();
        assertEq(vault.withdrawalsPausedUntil(), start + 7 days);

        vm.warp(start + 7 days);
        vm.expectEmit(address(vault));
        emit Vault.GuardianPausedWithdrawals(start + 14 days, 2);
        vault.pauseWithdrawals();
        vm.stopPrank();
        assertEq(vault.guardianPauses(), 2);

        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, true)));
        vm.prank(multisig);
        vm.expectRevert(Vault.WithdrawalsPaused.selector);
        vault.pauseWithdrawals();
        assertEq(vault.withdrawalsPausedUntil(), type(uint256).max);
        assertEq(vault.guardianPauses(), 2);
    }

    function test_RetiringTheSignerRefusesItsWithdrawalsAndCheckpointsUntilTheTimelockInstallsOne() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);

        vm.expectEmit(address(vault));
        emit Vault.SignerRetired(vm.addr(KEY), 1);
        vm.prank(multisig);
        vault.retireSigner();
        assertEq(vault.checkpointSigner(), address(0));
        assertEq(vault.signerEpoch(), 1);
        assertEq(vault.signerAtEpoch(1), vm.addr(KEY));
        assertFalse(vault.withdrawalsPaused());

        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        this.post(11, bytes32(uint256(11)), KEY);
        vm.prank(multisig);
        vm.expectRevert(Vault.NoSigner.selector);
        vault.retireSigner();

        // Retirement does not expire.
        vm.warp(block.timestamp + 30 days);
        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        assertFalse(vault.usedNonces(trader, 0));

        address next = vm.addr(KEY + 1);
        bytes memory install = abi.encodeCall(Vault.setSigner, (next));
        vm.prank(multisig);
        governance.schedule(address(vault), 0, install, bytes32(0), bytes32(0), GOVERNANCE_DELAY);
        vm.warp(block.timestamp + GOVERNANCE_DELAY);
        vm.expectEmit(address(vault));
        emit Vault.SignerChanged(address(0), next, 2);
        governance.execute(address(vault), 0, install, bytes32(0), bytes32(0));
        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY + 1));
        withdrawal.signerEpoch = 2;
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY + 1));
        assertEq(usdc.balanceOf(trader), 10e6);
        _post(vault, 11, bytes32(uint256(11)), KEY + 1);
        assertEq(vault.checkpointEpochAt(11), 2);
    }

    function test_RetirementOutlastsAPauseThatEnds() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        uint256 start = block.timestamp;
        vm.startPrank(multisig);
        vault.pauseWithdrawals();
        vault.retireSigner();
        vm.stopPrank();
        vm.warp(start + 7 days);
        assertFalse(vault.withdrawalsPaused());
        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
    }

    function test_GuardianCannotUnpauseOrInstallASigner() public {
        vm.startPrank(multisig);
        vault.pauseWithdrawals();
        vault.retireSigner();
        vm.expectRevert(Vault.UnauthorizedGovernance.selector);
        vault.setPauses(false, false);
        vm.expectRevert(Vault.UnauthorizedGovernance.selector);
        vault.setPauses(true, true);
        vm.expectRevert(Vault.UnauthorizedGovernance.selector);
        vault.setSigner(vm.addr(KEY));
        vm.stopPrank();
        assertTrue(vault.withdrawalsPaused());
        assertFalse(vault.depositsPaused());
        assertEq(vault.checkpointSigner(), address(0));
        assertEq(vault.signerEpoch(), 1);
    }

    function testFuzz_OnlyTheGuardianPausesOrRetires(address caller) public {
        vm.assume(caller != multisig && caller != address(vm));
        vm.startPrank(caller);
        vm.expectRevert(Vault.UnauthorizedGuardian.selector);
        vault.pauseWithdrawals();
        vm.expectRevert(Vault.UnauthorizedGuardian.selector);
        vault.retireSigner();
        vm.stopPrank();
        assertFalse(vault.withdrawalsPaused());
        assertEq(vault.checkpointSigner(), vm.addr(KEY));
        assertEq(vault.guardianPauses(), 0);
    }

    function test_TheTimelockIsNotTheGuardian() public {
        vm.startPrank(address(governance));
        vm.expectRevert(Vault.UnauthorizedGuardian.selector);
        vault.pauseWithdrawals();
        vm.expectRevert(Vault.UnauthorizedGuardian.selector);
        vault.retireSigner();
        vm.stopPrank();
    }

    /// @dev External so a test can expect its revert.
    function post(uint64 seq, bytes32 root, uint256 key) external {
        _post(vault, seq, root, key);
    }

    function _leaf(uint8 kind, address account, uint256 entitled) private pure returns (bytes32) {
        bytes32 key = keccak256(abi.encodePacked(kind, account));
        return keccak256(abi.encodePacked(bytes1(0x00), key, keccak256(abi.encode(entitled))));
    }
}
