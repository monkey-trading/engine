// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";
import {Vault} from "../src/Vault.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";

contract GovernanceTestCases is WithdrawalTest {
    function test_ConstructorRejectsMissingUndeployedOrZeroDelayTimelock() public {
        vm.expectRevert(Vault.InvalidGovernance.selector);
        new Vault(
            usdc,
            vm.addr(KEY),
            TimelockController(payable(address(0))),
            multisig,
            1e18,
            EXIT_PERIOD,
            CLAIM_WINDOW,
            STALE_AFTER
        );
        vm.expectRevert(Vault.InvalidGovernance.selector);
        new Vault(usdc, vm.addr(KEY), TimelockController(payable(multisig)), multisig, 1e18, 7 days, 3 days, 1 days);
        address[] memory roles = new address[](0);
        TimelockController zeroDelay = new TimelockController(0, roles, roles, address(0));
        vm.expectRevert(Vault.InvalidGovernance.selector);
        new Vault(usdc, vm.addr(KEY), zeroDelay, multisig, 1e18, 7 days, 3 days, 1 days);
    }

    function test_OnlyTimelockCanSetSignerOrPausesEvenForMultisigProposer() public {
        address[2] memory callers = [trader, multisig];
        for (uint256 i; i < callers.length; ++i) {
            vm.prank(callers[i]);
            vm.expectRevert(Vault.UnauthorizedGovernance.selector);
            vault.setSigner(vm.addr(KEY + 1));
            vm.prank(callers[i]);
            vm.expectRevert(Vault.UnauthorizedGovernance.selector);
            vault.setPauses(true, true);
        }
        assertEq(vault.checkpointSigner(), vm.addr(KEY));
        assertEq(vault.signerEpoch(), 1);
        assertFalse(vault.depositsPaused());
        assertFalse(vault.withdrawalsPaused());
    }

    function test_ProposerMustScheduleAndWaitDeploymentSuppliedDelay() public {
        bytes memory data = abi.encodeCall(Vault.setPauses, (true, true));
        vm.prank(trader);
        vm.expectRevert();
        governance.schedule(address(vault), 0, data, bytes32(0), bytes32(0), GOVERNANCE_DELAY);
        vm.prank(multisig);
        governance.schedule(address(vault), 0, data, bytes32(0), bytes32(0), GOVERNANCE_DELAY);
        vm.warp(block.timestamp + GOVERNANCE_DELAY - 1);
        vm.expectRevert();
        governance.execute(address(vault), 0, data, bytes32(0), bytes32(0));
        assertFalse(vault.depositsPaused());
        assertFalse(vault.withdrawalsPaused());
        vm.warp(block.timestamp + 1);
        vm.expectEmit(address(vault));
        emit Vault.PausesChanged(true, true);
        governance.execute(address(vault), 0, data, bytes32(0), bytes32(0));
        assertTrue(vault.depositsPaused());
        assertTrue(vault.withdrawalsPaused());
    }

    function test_RotationImmediatelyRetiresLiveOldSignaturesAndAllowsNewSignerReissue() public {
        address nextSigner = vm.addr(KEY + 1);
        bytes memory data = abi.encodeCall(Vault.setSigner, (nextSigner));
        vm.prank(multisig);
        governance.schedule(address(vault), 0, data, bytes32(0), bytes32(0), GOVERNANCE_DELAY);
        vm.warp(block.timestamp + GOVERNANCE_DELAY);
        Vault.Withdrawal memory withdrawal = _approval(7, 10e6);
        _postApproval(withdrawal);
        bytes memory old = _sign(vault, withdrawal, KEY);
        vm.expectEmit(address(vault));
        emit Vault.SignerChanged(vm.addr(KEY), nextSigner, 2);
        governance.execute(address(vault), 0, data, bytes32(0), bytes32(0));
        assertGt(withdrawal.expiresAt, block.timestamp);
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), old);
        withdrawal.signerEpoch = 2;
        bytes memory retiredKey = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), retiredKey);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY + 1));
        assertEq(usdc.balanceOf(trader), 10e6);

        withdrawal.signerEpoch = 3;
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY + 2))));
        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        bytes memory reissued = _sign(vault, withdrawal, KEY + 2);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), reissued);
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function test_RotationChangesCheckpointAuthorityAndPreservesTheExactParentFence() public {
        _post(vault, 10, bytes32(uint256(10)), KEY);
        address firstSigner = vm.addr(KEY);
        assertEq(vault.checkpointSignerAt(10), firstSigner);
        assertEq(vault.checkpointEpochAt(10), 1);
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY + 1))));
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(vault),
                uint64(11),
                bytes32(uint256(11)),
                true,
                uint64(10),
                bytes32(uint256(10))
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, digest);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        vault.postCheckpoint(11, bytes32(uint256(11)), true, 10, bytes32(uint256(10)), abi.encodePacked(r, s, v));
        assertFalse(vault.hasCheckpointAt(11));
        assertEq(vault.checkpointSignerAt(11), address(0));
        assertEq(vault.checkpointEpochAt(11), 0);
        _post(vault, 11, bytes32(uint256(11)), KEY + 1);
        assertEq(vault.checkpointRoots(10), bytes32(uint256(10)));
        assertEq(vault.checkpointRoots(11), bytes32(uint256(11)));
        assertEq(vault.latestSeq(), 11);
        assertEq(vault.checkpointSignerAt(10), firstSigner);
        assertEq(vault.checkpointEpochAt(10), 1);
        assertEq(vault.checkpointSignerAt(11), vm.addr(KEY + 1));
        assertEq(vault.checkpointEpochAt(11), 2);
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY + 2))));
        assertEq(vault.checkpointSignerAt(10), firstSigner);
        assertEq(vault.checkpointEpochAt(10), 1);
        assertEq(vault.checkpointSignerAt(11), vm.addr(KEY + 1));
        assertEq(vault.checkpointEpochAt(11), 2);
        vm.expectRevert(Vault.CheckpointParentMismatch.selector);
        vault.postCheckpoint(12, bytes32(uint256(12)), false, 0, bytes32(0), "");
    }

    function test_ReusingSignerStillIncrementsEpochAndZeroSignerIsRejected() public {
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY))));
        assertEq(vault.signerEpoch(), 2);
        assertEq(vault.checkpointSigner(), vm.addr(KEY));
        vm.prank(address(governance));
        vm.expectRevert(Vault.ZeroAddress.selector);
        vault.setSigner(address(0));
        assertEq(vault.signerEpoch(), 2);
    }

    function test_DepositPauseCoversAllDepositRoutesAndLeavesClaimsAndCheckpointsAvailable() public {
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (true, false)));
        usdc.mint(trader, 30e6);
        vm.prank(trader);
        usdc.approve(address(vault), 30e6);
        vm.startPrank(trader);
        vm.expectRevert(Vault.DepositsPaused.selector);
        vault.deposit(10e6);
        vm.expectRevert(Vault.DepositsPaused.selector);
        vault.depositFor(trader, 10e6);
        vm.expectRevert(Vault.DepositsPaused.selector);
        vault.depositToHouse(10e6);
        vm.stopPrank();
        assertEq(vault.depositCount(), 0);
        assertEq(usdc.balanceOf(trader), 30e6);
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        _postApproval(withdrawal);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        assertEq(usdc.balanceOf(trader), 30e6 + 1);
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, false)));
        vm.prank(trader);
        vault.deposit(10e6);
        assertEq(vault.depositCount(), 1);
    }

    function test_WithdrawalPausePreservesApprovalAndNonceAndLeavesDepositsAvailable() public {
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, true)));
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.WithdrawalsPaused.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(vault.usedNonces(trader, 0));
        usdc.mint(trader, 10e6);
        vm.startPrank(trader);
        usdc.approve(address(vault), 10e6);
        vault.depositToHouse(10e6);
        vm.stopPrank();
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (false, false)));
        withdrawal.issuedAt = uint64(block.timestamp);
        withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        assertEq(usdc.balanceOf(trader), 10e6);
        assertTrue(vault.usedNonces(trader, 0));
    }

    function test_OnlyTimelockSetsTheCurrentReleaseAfterItsDelay() public {
        bytes memory digest = _launchDigest(0x1d);
        bytes32 config = bytes32(uint256(0xc0));
        address[3] memory callers = [trader, multisig, vm.addr(KEY)];
        for (uint256 i; i < callers.length; ++i) {
            vm.prank(callers[i]);
            vm.expectRevert(Vault.UnauthorizedGovernance.selector);
            vault.setCurrentRelease(digest, config);
        }
        bytes memory data = abi.encodeCall(Vault.setCurrentRelease, (digest, config));
        vm.prank(multisig);
        governance.schedule(address(vault), 0, data, bytes32(0), bytes32(0), GOVERNANCE_DELAY);
        vm.warp(block.timestamp + GOVERNANCE_DELAY - 1);
        vm.expectRevert();
        governance.execute(address(vault), 0, data, bytes32(0), bytes32(0));
        assertEq(vault.currentRelease(), bytes32(0));
        assertEq(vault.releaseGeneration(), 0);
        vm.warp(block.timestamp + 1);
        bytes32 release = keccak256(abi.encodePacked(digest, config));
        vm.expectEmit(address(vault));
        emit Vault.CurrentReleaseChanged(release, 1, digest, config);
        governance.execute(address(vault), 0, data, bytes32(0), bytes32(0));
        assertEq(vault.currentRelease(), release);
        assertEq(vault.releaseGeneration(), 1);
        // The release moves no signer: the epoch and its withdrawals are untouched.
        assertEq(vault.checkpointSigner(), vm.addr(KEY));
        assertEq(vault.signerEpoch(), 1);
    }

    function test_ANewReleaseSupersedesTheCurrentOneAndTakesExactlyALaunchMeasurement() public {
        bytes memory digest = _launchDigest(0x1d);
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (digest, bytes32(uint256(1)))));
        bytes32 first = vault.currentRelease();
        // The same code under another config is another release.
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (digest, bytes32(uint256(2)))));
        assertTrue(vault.currentRelease() != first, "a config change kept the release");
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_launchDigest(0x1e), bytes32(uint256(2)))));
        assertEq(vault.currentRelease(), keccak256(abi.encodePacked(_launchDigest(0x1e), bytes32(uint256(2)))));
        assertEq(vault.releaseGeneration(), 3);
        uint256[3] memory lengths = [uint256(0), 47, 49];
        for (uint256 i; i < lengths.length; ++i) {
            vm.prank(address(governance));
            vm.expectRevert(Vault.InvalidLaunchDigest.selector);
            vault.setCurrentRelease(new bytes(lengths[i]), bytes32(uint256(1)));
        }
        assertEq(vault.releaseGeneration(), 3, "a refused call raised the generation");
    }

    function test_SettingAReleaseCurrentAgainStartsANewGeneration() public {
        bytes memory digest = _launchDigest(0x1d);
        bytes memory data = abi.encodeCall(Vault.setCurrentRelease, (digest, bytes32(uint256(1))));
        _govern(address(vault), data);
        bytes32 release = vault.currentRelease();
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_launchDigest(0x1e), bytes32(uint256(1)))));
        // Setting the first release again, or the current one again, starts a new generation.
        vm.prank(multisig);
        governance.schedule(address(vault), 0, data, bytes32(0), bytes32("again"), GOVERNANCE_DELAY);
        vm.warp(block.timestamp + GOVERNANCE_DELAY);
        vm.expectEmit(address(vault));
        emit Vault.CurrentReleaseChanged(release, 3, digest, bytes32(uint256(1)));
        governance.execute(address(vault), 0, data, bytes32(0), bytes32("again"));
        assertEq(vault.currentRelease(), release);
        assertEq(vault.releaseGeneration(), 3);
        _govern(address(vault), data);
        assertEq(vault.releaseGeneration(), 4);
    }

    function testFuzz_TheReleaseKeysOnTheExactMeasurementAndConfig(
        bytes32 high,
        bytes16 low,
        bytes32 config,
        uint8 flipped
    ) public {
        bytes memory digest = abi.encodePacked(high, low);
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (digest, config)));
        assertEq(vault.currentRelease(), keccak256(abi.encodePacked(digest, config)));
        bytes memory other = abi.encodePacked(digest);
        other[flipped % 48] ^= 0x01;
        assertTrue(vault.currentRelease() != keccak256(abi.encodePacked(other, config)), "a measurement one bit away");
        assertTrue(
            vault.currentRelease() != keccak256(abi.encodePacked(digest, config ^ bytes32(uint256(1)))),
            "a config one bit away"
        );
    }

    function _launchDigest(bytes1 fill) private pure returns (bytes memory digest) {
        digest = new bytes(48);
        for (uint256 i; i < 48; ++i) {
            digest[i] = fill;
        }
    }
}
