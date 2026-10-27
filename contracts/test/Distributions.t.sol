// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {Vault} from "../src/Vault.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// USDC distribution claims against posted checkpoint roots, as the engine's staking dev scenario
/// also exercises end to end.
contract DistributionsTest is Test {
    uint256 private constant KEY = 0x1234;
    uint8 private constant DISTRIBUTION_LEAF = 13;

    MockUSDC private usdc;
    Vault private vault;

    function setUp() public {
        usdc = new MockUSDC();
        address[] memory proposers = new address[](1);
        proposers[0] = vm.addr(KEY);
        address[] memory executors = new address[](1);
        TimelockController governance = new TimelockController(2 days, proposers, executors, address(0));
        vault = new Vault(usdc, vm.addr(KEY), governance, 1e18);
    }

    function test_AProofPaysUsdcOnceToTheAccountWhoeverRelays() public {
        address account = makeAddr("account");
        uint256 entitled = 250 * 1e6;
        usdc.mint(address(vault), entitled);
        post(1, leaf(account, entitled), false, 0, 0);

        address relayer = makeAddr("relayer");
        vm.expectEmit(address(vault));
        emit Vault.DistributionClaimed(account, entitled, entitled, 1);
        vm.prank(relayer);
        assertEq(vault.claimDistribution(account, entitled, new bytes32[](0)), entitled);
        assertEq(usdc.balanceOf(account), entitled);
        assertEq(usdc.balanceOf(relayer), 0);
        assertEq(vault.distributionsClaimed(account), entitled);
        assertEq(vault.distributionsPaid(), entitled);

        vm.expectRevert(abi.encodeWithSelector(Vault.NothingToClaim.selector, entitled, entitled));
        vault.claimDistribution(account, entitled, new bytes32[](0));
        assertEq(usdc.balanceOf(address(vault)), 0);
    }

    function testFuzz_SingleLeafTreeClaimsUpToTheBalance(address account, uint256 entitled) public {
        vm.assume(account != address(0));
        entitled = bound(entitled, 1, 1e15);
        usdc.mint(address(vault), entitled);
        post(1, leaf(account, entitled), false, 0, 0);
        vault.claimDistribution(account, entitled, new bytes32[](0));
        assertEq(usdc.balanceOf(account), entitled);
        vm.expectRevert(abi.encodeWithSelector(Vault.NothingToClaim.selector, entitled, entitled));
        vault.claimDistribution(account, entitled, new bytes32[](0));
    }

    function test_AProofAgainstAnOlderRootStopsWorkingAndTheIncrementIsPaid() public {
        address account = makeAddr("account");
        usdc.mint(address(vault), 250);
        post(1, leaf(account, 100), false, 0, 0);
        vault.claimDistribution(account, 100, new bytes32[](0));
        post(2, leaf(account, 250), true, 1, leaf(account, 100));

        bytes32[] memory earlier = new bytes32[](0);
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimDistribution(account, 100, earlier);
        assertEq(vault.claimDistribution(account, 250, new bytes32[](0)), 150);
        assertEq(usdc.balanceOf(account), 250);
        assertEq(vault.distributionsPaid(), 250);
    }

    function testTwoLeafTreeTakesEachSideByTheKeyBit() public {
        // Two accounts whose keys part at the first bit sit directly under the root.
        address left = makeAddr("left");
        address right = makeAddr("right");
        for (uint256 i; uint256(key(left)) >> 255 != 0; ++i) {
            left = makeAddr(vm.toString(i));
        }
        for (uint256 i; uint256(key(right)) >> 255 != 1; ++i) {
            right = makeAddr(vm.toString(i));
        }
        usdc.mint(address(vault), 12);
        bytes32 leftLeaf = leaf(left, 5);
        bytes32 rightLeaf = leaf(right, 7);
        post(1, keccak256(abi.encodePacked(bytes1(0x01), leftLeaf, rightLeaf)), false, 0, 0);
        bytes32[] memory proof = new bytes32[](1);
        proof[0] = leftLeaf;
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimDistribution(left, 5, proof);
        proof[0] = rightLeaf;
        vault.claimDistribution(left, 5, proof);
        proof[0] = leftLeaf;
        vault.claimDistribution(right, 7, proof);
        assertEq(usdc.balanceOf(left), 5);
        assertEq(usdc.balanceOf(right), 7);
        assertEq(vault.distributionsPaid(), 12);
    }

    function testRefusesEveryAlteredProofAndClaimsBeforeAnyCheckpoint() public {
        address account = makeAddr("account");
        vm.expectRevert(Vault.NoCheckpoint.selector);
        vault.claimDistribution(account, 100, new bytes32[](0));
        post(1, leaf(account, 100), false, 0, 0);
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimDistribution(account, 101, new bytes32[](0));
        bytes32[] memory tampered = new bytes32[](1);
        tampered[0] = bytes32(uint256(1));
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimDistribution(account, 100, tampered);
        assertEq(usdc.balanceOf(account), 0);
    }

    function key(address account) private pure returns (bytes32) {
        return keccak256(abi.encodePacked(DISTRIBUTION_LEAF, account));
    }

    function leaf(address account, uint256 entitled) private pure returns (bytes32) {
        return keccak256(abi.encodePacked(bytes1(0x00), key(account), keccak256(abi.encode(entitled))));
    }

    function post(uint64 seq, bytes32 root, bool hasParent, uint64 parentSeq, bytes32 parentRoot) private {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(vault),
                seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, digest);
        vault.postCheckpoint(seq, root, hasParent, parentSeq, parentRoot, abi.encodePacked(r, s, v));
    }
}
