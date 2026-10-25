// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";
import {Vault} from "../src/Vault.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";

contract SignerHistoryTest is WithdrawalTest {
    function test_OldRootReissueHasSignerHistoryWithoutAnEpochTwoCheckpoint() public {
        Vault.Withdrawal memory approval = _approval(7, 10e6);
        _postApproval(approval);
        bytes32 approvalRoot = vault.checkpointRoots(10);
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY + 1))));

        bytes memory rotation = _readyRotation(vm.addr(KEY + 2), bytes32("epoch three"));
        Vault.Withdrawal memory epochTwo = _approval(7, 10e6);
        bytes memory epochTwoSignature = _sign(vault, epochTwo, KEY + 1);
        assertEq(epochTwo.signerEpoch, 2);
        assertEq(vault.latestSeq(), 10);
        assertEq(vault.checkpointEpochAt(10), 1);
        assertFalse(vault.usedNonces(trader, 7));

        governance.execute(address(vault), 0, rotation, bytes32(0), bytes32("epoch three"));
        assertGt(epochTwo.expiresAt, block.timestamp);
        assertEq(vault.signerAtEpoch(1), vm.addr(KEY));
        assertEq(vault.signerAtEpoch(2), vm.addr(KEY + 1));
        assertEq(vault.signerAtEpoch(3), vm.addr(KEY + 2));
        assertEq(vault.signerAtEpoch(0), address(0));
        assertEq(vault.signerAtEpoch(4), address(0));
        assertEq(vault.signerAtEpoch(type(uint64).max), address(0));
        assertEq(ECDSA.recover(vault.withdrawalDigest(epochTwo), epochTwoSignature), vault.signerAtEpoch(2));
        assertTrue(vault.signerAtEpoch(2) != vault.checkpointSignerAt(10));
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(epochTwo, 10, new bytes32[](0), epochTwoSignature);

        Vault.Withdrawal memory current = _approval(7, 10e6);
        bytes memory retiredKey = _sign(vault, current, KEY + 1);
        vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
        vault.withdraw(current, 10, new bytes32[](0), retiredKey);
        _rejectRetiredCheckpoints(approvalRoot);
        _post(vault, 11, bytes32(uint256(11)), KEY + 2);
        assertEq(vault.checkpointEpochAt(10), 1);
        assertEq(vault.checkpointEpochAt(11), 3);
        assertEq(vault.checkpointSignerAt(10), vm.addr(KEY));
        assertEq(vault.signerAtEpoch(2), vm.addr(KEY + 1));
        assertEq(vault.checkpointRoots(10), approvalRoot);

        uint256 before = usdc.balanceOf(address(vault));
        bytes memory signature = _sign(vault, current, KEY + 2);
        vm.expectEmit(address(vault));
        emit Vault.WithdrawalPaid(trader, 10e6, 7, 10);
        vault.withdraw(current, 10, new bytes32[](0), signature);
        assertEq(usdc.balanceOf(trader), 10e6);
        assertEq(usdc.balanceOf(address(vault)), before - 10e6);
        assertEq(vault.totalWithdrawn(), 10e6);
        assertTrue(vault.usedNonces(trader, 7));
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(current, 10, new bytes32[](0), signature);
        assertEq(vault.totalWithdrawn(), 10e6);
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function test_ReusedSignerDoesNotReviveOldEpochAndHistorySurvivesFurtherRotations() public {
        Vault.Withdrawal memory approval = _approval(9, 25e6);
        _postApproval(approval);
        bytes32 root = vault.checkpointRoots(10);
        bytes memory rotation = _readyRotation(vm.addr(KEY), bytes32("same signer"));
        Vault.Withdrawal memory epochOne = _approval(9, 25e6);
        bytes memory old = _sign(vault, epochOne, KEY);
        governance.execute(address(vault), 0, rotation, bytes32(0), bytes32("same signer"));
        assertGt(epochOne.expiresAt, block.timestamp);
        assertEq(vault.signerAtEpoch(1), vm.addr(KEY));
        assertEq(vault.signerAtEpoch(2), vm.addr(KEY));
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(epochOne, 10, new bytes32[](0), old);

        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY + 1))));
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY))));
        assertEq(vault.signerAtEpoch(3), vm.addr(KEY + 1));
        assertEq(vault.signerAtEpoch(4), vm.addr(KEY));
        Vault.Withdrawal memory retired = _approval(9, 25e6);
        retired.signerEpoch = 2;
        old = _sign(vault, retired, KEY);
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(retired, 10, new bytes32[](0), old);
        Vault.Withdrawal memory current = _approval(9, 25e6);
        vault.withdraw(current, 10, new bytes32[](0), _sign(vault, current, KEY));
        assertEq(usdc.balanceOf(trader), 25e6);
        assertTrue(vault.usedNonces(trader, 9));

        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY))));
        for (uint64 epoch = 1; epoch <= 5; ++epoch) {
            assertEq(vault.signerAtEpoch(epoch), vm.addr(epoch == 3 ? KEY + 1 : KEY));
        }
        assertEq(vault.signerAtEpoch(0), address(0));
        assertEq(vault.signerAtEpoch(6), address(0));
        assertEq(vault.checkpointRoots(10), root);
        assertEq(vault.checkpointEpochAt(10), 1);
        assertEq(vault.latestSeq(), 10);
        current = _approval(9, 25e6);
        bytes memory reissued = _sign(vault, current, KEY);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(current, 10, new bytes32[](0), reissued);
        assertEq(vault.totalWithdrawn(), 25e6);
        assertEq(usdc.balanceOf(trader), 25e6);
    }

    function test_RefusedRotationPreservesHistoryAndCurrentPaymentAuthority() public {
        Vault.Withdrawal memory approval = _approval(1, 1e6);
        _postApproval(approval);
        vm.prank(trader);
        vm.expectRevert(Vault.UnauthorizedGovernance.selector);
        vault.setSigner(trader);
        vm.prank(address(governance));
        vm.expectRevert(Vault.ZeroAddress.selector);
        vault.setSigner(address(0));
        assertEq(vault.signerEpoch(), 1);
        assertEq(vault.checkpointSigner(), vm.addr(KEY));
        assertEq(vault.signerAtEpoch(1), vm.addr(KEY));
        assertEq(vault.signerAtEpoch(0), address(0));
        assertEq(vault.signerAtEpoch(2), address(0));
        vault.withdraw(approval, 10, new bytes32[](0), _sign(vault, approval, KEY));
        assertTrue(vault.usedNonces(trader, 1));
        assertEq(usdc.balanceOf(trader), 1e6);
        assertEq(vault.totalWithdrawn(), 1e6);
    }

    function _readyRotation(address signer, bytes32 salt) private returns (bytes memory data) {
        data = abi.encodeCall(Vault.setSigner, (signer));
        uint256 delay = governance.getMinDelay();
        vm.prank(multisig);
        governance.schedule(address(vault), 0, data, bytes32(0), salt, delay);
        vm.warp(block.timestamp + delay);
    }

    function _rejectRetiredCheckpoints(bytes32 parentRoot) private {
        for (uint256 key = KEY; key <= KEY + 1; ++key) {
            bytes32 digest = keccak256(
                abi.encodePacked(
                    "engine.checkpoint.v2",
                    uint64(block.chainid),
                    address(vault),
                    uint64(11),
                    bytes32(uint256(11)),
                    true,
                    uint64(10),
                    parentRoot
                )
            );
            (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
            bytes memory signature = abi.encodePacked(r, s, v);
            vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
            vault.postCheckpoint(11, bytes32(uint256(11)), true, 10, parentRoot, signature);
        }
        assertFalse(vault.hasCheckpointAt(11));
        assertEq(vault.checkpointSignerAt(11), address(0));
        assertEq(vault.checkpointEpochAt(11), 0);
    }
}
