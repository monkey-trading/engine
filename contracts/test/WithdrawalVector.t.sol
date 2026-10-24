// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";
import {Vault} from "../src/Vault.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";

contract WithdrawalVectorTest is WithdrawalTest {
    function test_SharedRustSolidityDigestSignatureAndApprovalProofVector() public {
        string memory fixture = vm.readFile("test/fixtures/withdrawal.json");
        vm.chainId(vm.parseJsonUint(fixture, ".chainId"));
        address verifier = vm.parseJsonAddress(fixture, ".verifyingContract");
        Vault.Withdrawal memory withdrawal = Vault.Withdrawal({
            account: vm.parseJsonAddress(fixture, ".withdrawal.account"),
            amount: vm.parseJsonUint(fixture, ".withdrawal.amount"),
            nonce: uint64(vm.parseJsonUint(fixture, ".withdrawal.nonce")),
            approvedSeq: uint64(vm.parseJsonUint(fixture, ".withdrawal.approvedSeq")),
            signerEpoch: uint64(vm.parseJsonUint(fixture, ".withdrawal.signerEpoch")),
            issuedAt: uint64(vm.parseJsonUint(fixture, ".withdrawal.issuedAt")),
            expiresAt: uint64(vm.parseJsonUint(fixture, ".withdrawal.expiresAt"))
        });
        {
            bytes32 key = keccak256(abi.encodePacked(uint8(32), withdrawal.account, withdrawal.nonce));
            bytes32 value =
                keccak256(abi.encode(withdrawal.account, withdrawal.amount, withdrawal.nonce, withdrawal.approvedSeq));
            assertEq(key, vm.parseJsonBytes32(fixture, ".approvalKey"));
            assertEq(value, vm.parseJsonBytes32(fixture, ".valueHash"));
            assertEq(keccak256(abi.encodePacked(hex"00", key, value)), vm.parseJsonBytes32(fixture, ".leafHash"));
        }
        bytes32[] memory siblings = abi.decode(vm.parseJson(fixture, ".siblings"), (bytes32[]));
        bytes32 root = vm.parseJsonBytes32(fixture, ".root");
        assertEq(_root(withdrawal, siblings), root);
        address signer = vm.parseJsonAddress(fixture, ".signer");
        {
            Vault template = new Vault(usdc, signer, governance);
            vm.etch(verifier, address(template).code);
        }
        Vault target = Vault(verifier);
        // Etching runtime code omits constructor storage; seed the signer and epoch by governance.
        vm.prank(address(governance));
        target.setSigner(signer);
        assertEq(target.signerEpoch(), withdrawal.signerEpoch);
        bytes32 digest = vm.parseJsonBytes32(fixture, ".digest");
        assertEq(target.withdrawalDigest(withdrawal), digest);
        assertEq(_digest(target, withdrawal), digest);
        assertEq(target.WITHDRAWAL_TYPEHASH(), vm.parseJsonBytes32(fixture, ".typeHash"));
        bytes memory signature = vm.parseJsonBytes(fixture, ".signature");
        assertEq(ECDSA.recover(digest, signature), signer);
        assertEq(signature, _sign(target, withdrawal, KEY));
        uint64 checkpointSeq = uint64(vm.parseJsonUint(fixture, ".checkpointSeq"));
        _post(target, checkpointSeq, root, KEY);
        usdc.mint(verifier, withdrawal.amount);
        vm.warp(withdrawal.issuedAt);
        target.withdraw(withdrawal, checkpointSeq, siblings, signature);
        assertEq(usdc.balanceOf(withdrawal.account), withdrawal.amount);
        assertTrue(target.usedNonces(withdrawal.account, withdrawal.nonce));
    }
}
