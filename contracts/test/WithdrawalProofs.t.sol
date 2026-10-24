// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vault} from "../src/Vault.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

contract WithdrawalProofsTest is WithdrawalTest {
    function test_MissingRootAndCheckpointBeforeApprovalCannotPay() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        _post(vault, 9, _root(withdrawal, new bytes32[](0)), KEY);
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        vault.withdraw(withdrawal, 9, new bytes32[](0), signature);
        assertFalse(vault.usedNonces(trader, 0));
        assertEq(usdc.balanceOf(trader), 0);
    }

    function test_OlderAcceptedRootRemainsClaimableAfterManyCheckpoints() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        bytes32 rightfulRoot = vault.latestRoot();
        for (uint64 seq = 11; seq <= 42; ++seq) {
            _post(vault, seq, bytes32(uint256(seq)), KEY);
        }
        assertEq(vault.checkpointRoots(10), rightfulRoot);
        vault.withdraw(withdrawal, 10, new bytes32[](0), _sign(vault, withdrawal, KEY));
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function test_ForkRootRejectedByParentFenceCannotAuthorizeAClaim() public {
        _post(vault, 5, bytes32(uint256(5)), KEY);
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        bytes32 forkRoot = _root(withdrawal, new bytes32[](0));
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(vault),
                uint64(11),
                forkRoot,
                true,
                uint64(5),
                bytes32(uint256(5))
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, digest);
        bytes memory staleParent = abi.encodePacked(r, s, v);
        _post(vault, 10, bytes32(uint256(10)), KEY);
        vm.expectRevert(Vault.CheckpointParentMismatch.selector);
        vault.postCheckpoint(11, forkRoot, true, 5, bytes32(uint256(5)), staleParent);
        assertFalse(vault.hasCheckpointAt(11));
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        vault.withdraw(withdrawal, 11, new bytes32[](0), signature);
        vm.expectRevert(Vault.InvalidApprovalProof.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertFalse(vault.usedNonces(trader, 0));
    }

    function test_UnacceptedSequenceCannotAliasAnAcceptedZeroRoot() public {
        _post(vault, 0, bytes32(0), KEY);
        assertTrue(vault.hasCheckpointAt(0));
        assertFalse(vault.hasCheckpointAt(1));
        Vault.Withdrawal memory withdrawal = _approval(0, 1);
        withdrawal.approvedSeq = 0;
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        vault.withdraw(withdrawal, 1, new bytes32[](0), signature);
    }

    function test_ProofCannotChangeAccountAmountNonceOrApprovalSeq() public {
        Vault.Withdrawal memory withdrawal = _approval(0, 10e6);
        _postApproval(withdrawal);
        for (uint256 field; field < 4; ++field) {
            Vault.Withdrawal memory changed = _approval(0, 10e6);
            if (field == 0) changed.account = makeAddr("wrong account");
            else if (field == 1) changed.amount++;
            else if (field == 2) changed.nonce++;
            else changed.approvedSeq--;
            bytes memory signature = _sign(vault, changed, KEY);
            vm.expectRevert(Vault.InvalidApprovalProof.selector);
            vault.withdraw(changed, 10, new bytes32[](0), signature);
            assertFalse(vault.usedNonces(changed.account, changed.nonce));
        }
    }

    function test_SignatureBindsEveryFieldEvenWithARightfulProof() public {
        Vault.Withdrawal memory original = _approval(7, 10e6);
        bytes memory signature = _sign(vault, original, KEY);
        for (uint256 field; field < 6; ++field) {
            Vault.Withdrawal memory changed = _approval(7, 10e6);
            if (field == 0) {
                changed.account = makeAddr("different account");
            } else if (field == 1) {
                changed.amount++;
            } else if (field == 2) {
                changed.nonce++;
            } else if (field == 3) {
                changed.approvedSeq--;
            } else if (field == 4) {
                changed.issuedAt--;
                changed.expiresAt--;
            } else {
                changed.issuedAt += 1;
                changed.expiresAt += 1;
                vm.warp(block.timestamp + 1);
            }
            _postApproval(changed);
            uint64 checkpointSeq = vault.latestSeq();
            vm.expectRevert(Vault.InvalidWithdrawalSigner.selector);
            vault.withdraw(changed, checkpointSeq, new bytes32[](0), signature);
        }
        original.signerEpoch++;
        uint64 latestSeq = vault.latestSeq();
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        vault.withdraw(original, latestSeq, new bytes32[](0), signature);
        assertEq(usdc.balanceOf(trader), 0);
    }

    function test_ProofDepthIsBoundedAndAll256KeyBitsAreSupported() public {
        Vault.Withdrawal memory withdrawal = _approval(type(uint64).max, 1);
        bytes32[] memory siblings = new bytes32[](256);
        for (uint256 i; i < siblings.length; ++i) {
            siblings[i] = keccak256(abi.encode(i));
        }
        _post(vault, 10, _root(withdrawal, siblings), KEY);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        vm.expectRevert(Vault.InvalidApprovalProof.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](257), signature);
        vault.withdraw(withdrawal, 10, siblings, signature);
        assertEq(usdc.balanceOf(trader), 1);
    }

    function testFuzz_ProofTamperAndTruncationCannotPay(uint8 depthSeed, bytes32 seed) public {
        uint256 depth = bound(depthSeed, 2, 32);
        Vault.Withdrawal memory withdrawal = _approval(7, 10e6);
        bytes32[] memory siblings = new bytes32[](depth);
        for (uint256 i; i < depth; ++i) {
            siblings[i] = keccak256(abi.encode(seed, i));
        }
        _post(vault, 10, _root(withdrawal, siblings), KEY);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        uint256 index = uint256(seed) % depth;
        siblings[index] ^= bytes32(uint256(1));
        vm.expectRevert(Vault.InvalidApprovalProof.selector);
        vault.withdraw(withdrawal, 10, siblings, signature);
        siblings[index] ^= bytes32(uint256(1));
        bytes32[] memory truncated = new bytes32[](depth - 1);
        for (uint256 i; i < truncated.length; ++i) {
            truncated[i] = siblings[i];
        }
        vm.expectRevert(Vault.InvalidApprovalProof.selector);
        vault.withdraw(withdrawal, 10, truncated, signature);
        bytes32 first = siblings[0];
        siblings[0] = siblings[1];
        siblings[1] = first;
        vm.expectRevert(Vault.InvalidApprovalProof.selector);
        vault.withdraw(withdrawal, 10, siblings, signature);
        siblings[1] = siblings[0];
        siblings[0] = first;
        assertFalse(vault.usedNonces(trader, 7));
        vault.withdraw(withdrawal, 10, siblings, signature);
        assertEq(usdc.balanceOf(trader), 10e6);
    }

    function testFuzz_WholeBaseUnitAmountsPayExactlyOnce(uint256 amount, uint64 nonce) public {
        amount = bound(amount, 1, usdc.balanceOf(address(vault)));
        Vault.Withdrawal memory withdrawal = _approval(nonce, amount);
        _postApproval(withdrawal);
        bytes memory signature = _sign(vault, withdrawal, KEY);
        uint256 before = usdc.balanceOf(address(vault));
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        vm.expectRevert(Vault.WithdrawalAlreadyPaid.selector);
        vault.withdraw(withdrawal, 10, new bytes32[](0), signature);
        assertEq(usdc.balanceOf(trader), amount);
        assertEq(usdc.balanceOf(address(vault)), before - amount);
    }

    function testFuzz_ArbitrarySignatureCannotPay(bytes calldata signature) public {
        Vault.Withdrawal memory withdrawal = _approval(7, 10e6);
        _postApproval(withdrawal);
        (address recovered,,) = ECDSA.tryRecover(vault.withdrawalDigest(withdrawal), signature);
        vm.assume(recovered != vault.checkpointSigner());
        (bool ok,) =
            address(vault).call(abi.encodeCall(Vault.withdraw, (withdrawal, uint64(10), new bytes32[](0), signature)));
        assertFalse(ok);
        assertFalse(vault.usedNonces(trader, 7));
        assertEq(usdc.balanceOf(trader), 0);
    }
}
