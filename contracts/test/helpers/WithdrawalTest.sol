// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vault} from "../../src/Vault.sol";
import {MockUSDC} from "../mocks/MockUSDC.sol";
import {GovernanceTest} from "./GovernanceTest.sol";

abstract contract WithdrawalTest is GovernanceTest {
    uint256 internal constant KEY = 0x1234;
    MockUSDC internal usdc;
    Vault internal vault;
    address internal trader = makeAddr("trader");

    function setUp() public virtual {
        vm.warp(1_800_000_000);
        governance = _deployGovernance();
        usdc = new MockUSDC();
        vault = new Vault(usdc, vm.addr(KEY), governance);
        usdc.mint(address(vault), 1_000_000e6);
    }

    function _approval(uint64 nonce, uint256 amount) internal view returns (Vault.Withdrawal memory) {
        return Vault.Withdrawal(
            trader, amount, nonce, 10, vault.signerEpoch(), uint64(block.timestamp), uint64(block.timestamp + 1 days)
        );
    }

    function _digest(Vault target, Vault.Withdrawal memory withdrawal) internal view returns (bytes32) {
        bytes32 domain = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("EngineWithdrawal"),
                keccak256("1"),
                block.chainid,
                address(target)
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(
                keccak256(
                    "Withdrawal(address account,uint256 amount,uint64 nonce,uint64 approvedSeq,uint64 signerEpoch,uint64 issuedAt,uint64 expiresAt)"
                ),
                withdrawal.account,
                withdrawal.amount,
                withdrawal.nonce,
                withdrawal.approvedSeq,
                withdrawal.signerEpoch,
                withdrawal.issuedAt,
                withdrawal.expiresAt
            )
        );
        return keccak256(abi.encodePacked(hex"1901", domain, structHash));
    }

    function _sign(Vault target, Vault.Withdrawal memory withdrawal, uint256 key) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, _digest(target, withdrawal));
        return abi.encodePacked(r, s, v);
    }

    function _root(Vault.Withdrawal memory withdrawal, bytes32[] memory siblings) internal pure returns (bytes32 hash) {
        bytes32 key = keccak256(abi.encodePacked(uint8(32), withdrawal.account, withdrawal.nonce));
        bytes32 value =
            keccak256(abi.encode(withdrawal.account, withdrawal.amount, withdrawal.nonce, withdrawal.approvedSeq));
        hash = keccak256(abi.encodePacked(hex"00", key, value));
        for (uint256 i = siblings.length; i > 0;) {
            --i;
            bool right = (uint8(key[i / 8]) & (uint8(0x80) >> (i % 8))) != 0;
            hash = right
                ? keccak256(abi.encodePacked(hex"01", siblings[i], hash))
                : keccak256(abi.encodePacked(hex"01", hash, siblings[i]));
        }
    }

    function _post(Vault target, uint64 seq, bytes32 root, uint256 key) internal {
        bool hasParent = target.hasCheckpoint();
        uint64 parentSeq = target.latestSeq();
        bytes32 parentRoot = target.latestRoot();
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(target),
                seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        target.postCheckpoint(seq, root, hasParent, parentSeq, parentRoot, abi.encodePacked(r, s, v));
    }

    function _postApproval(Vault.Withdrawal memory withdrawal) internal {
        uint64 seq = vault.hasCheckpoint() ? vault.latestSeq() + 1 : withdrawal.approvedSeq;
        _post(vault, seq, _root(withdrawal, new bytes32[](0)), KEY);
    }
}
