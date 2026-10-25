// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;
import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {Vault} from "../src/Vault.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";

contract CheckpointsTest is GovernanceTest {
    Vault private vault;
    uint256 private constant KEY = 0x1234;

    function setUp() public {
        governance = _deployGovernance();
        vault = new Vault(IERC20(address(1)), vm.addr(KEY), governance, 1e18);
    }

    function testFuzzOnlyIncreasingSignedCheckpoints(uint64 seq, bytes32 root) public {
        vault.postCheckpoint(seq, root, false, 0, 0, sign(vault, seq, root, false, 0, 0, KEY));
        assertEq(vault.latestSeq(), seq);
        assertEq(vault.latestRoot(), root);
        bytes memory same = sign(vault, seq, root, true, seq, root, KEY);
        vm.expectRevert(Vault.CheckpointNotExtended.selector);
        vault.postCheckpoint(seq, root, true, seq, root, same);
        if (seq > 0) {
            bytes memory lower = sign(vault, seq - 1, root, true, seq, root, KEY);
            vm.expectRevert(Vault.CheckpointNotExtended.selector);
            vault.postCheckpoint(seq - 1, root, true, seq, root, lower);
        }
        if (seq < type(uint64).max) {
            vault.postCheckpoint(seq + 1, root, true, seq, root, sign(vault, seq + 1, root, true, seq, root, KEY));
        }
    }

    function testSameKeyDivergentPublishersCannotBothExtendOneParent() public {
        bytes32 root = bytes32(uint256(7));
        vault.postCheckpoint(5, root, false, 0, 0, sign(vault, 5, root, false, 0, 0, KEY));
        bytes memory left = sign(vault, 10, bytes32(uint256(10)), true, 5, root, KEY);
        bytes memory right = sign(vault, 11, bytes32(uint256(11)), true, 5, root, KEY);
        vault.postCheckpoint(10, bytes32(uint256(10)), true, 5, root, left);
        vm.expectRevert(Vault.CheckpointParentMismatch.selector);
        vault.postCheckpoint(11, bytes32(uint256(11)), true, 5, root, right);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        vault.postCheckpoint(11, bytes32(uint256(11)), true, 10, bytes32(uint256(10)), right);
        assertEq(vault.latestSeq(), 10);
        assertEq(vault.latestRoot(), bytes32(uint256(10)));
    }

    function testRefusesForgedSignerAndModifiedRootOrSeq() public {
        bytes memory signature = sign(vault, 2, bytes32(uint256(7)), false, 0, 0, KEY);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        vault.postCheckpoint(3, bytes32(uint256(7)), false, 0, 0, signature);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        vault.postCheckpoint(2, bytes32(uint256(8)), false, 0, 0, signature);
        bytes memory forged = sign(vault, 2, bytes32(uint256(7)), false, 0, 0, KEY + 1);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        vault.postCheckpoint(2, bytes32(uint256(7)), false, 0, 0, forged);
        assertFalse(vault.hasCheckpoint());
    }

    function testRefusesNoncanonicalEmptyParent() public {
        bytes memory signature = sign(vault, 2, bytes32(uint256(7)), false, 1, 0, KEY);
        vm.expectRevert(Vault.CheckpointParentMismatch.selector);
        vault.postCheckpoint(2, bytes32(uint256(7)), false, 1, 0, signature);
    }

    function testRefusesAnotherVaultOrChain() public {
        bytes memory signature = sign(vault, 2, bytes32(uint256(7)), false, 0, 0, KEY);
        Vault other = new Vault(IERC20(address(1)), vm.addr(KEY), governance, 1e18);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        other.postCheckpoint(2, bytes32(uint256(7)), false, 0, 0, signature);
        vm.chainId(block.chainid + 1);
        vm.expectRevert(Vault.InvalidCheckpointSigner.selector);
        vault.postCheckpoint(2, bytes32(uint256(7)), false, 0, 0, signature);
    }

    function testRefusesZeroSignerAndUnsupportedChain() public {
        vm.expectRevert(Vault.ZeroAddress.selector);
        new Vault(IERC20(address(1)), address(0), governance, 1e18);
        vm.chainId(uint256(type(uint64).max) + 1);
        vm.expectRevert(Vault.UnsupportedChain.selector);
        new Vault(IERC20(address(1)), address(2), governance, 1e18);
    }

    function testRejectsMissingSignature() public {
        vm.expectRevert();
        vault.postCheckpoint(0, bytes32(0), false, 0, 0, "");
    }

    function sign(
        Vault target,
        uint64 seq,
        bytes32 root,
        bool hasParent,
        uint64 parentSeq,
        bytes32 parentRoot,
        uint256 key
    ) private view returns (bytes memory) {
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
        return abi.encodePacked(r, s, v);
    }
}
