// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {ERC20Capped} from "@openzeppelin-contracts/token/ERC20/extensions/ERC20Capped.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Vault} from "../src/Vault.sol";

/// MONKEY claims against posted checkpoint roots. `crates/core/tests/fixtures/emissions.json` holds
/// entitlement proofs the engine produced; `emissions_fixture.rs` checks them on the Rust side.
contract EmissionsTest is GovernanceTest {
    uint256 private constant KEY = 0x1234;
    uint256 private constant BUDGET = 1_000_000;
    uint8 private constant ENTITLEMENT_LEAF = 8;

    Vault private vault;
    MonkeyToken private monkey;
    string private fixture;

    struct Claim {
        uint64 seq;
        bytes32 root;
        address account;
        uint256 entitled;
        bytes32[] siblings;
    }

    function setUp() public {
        governance = _deployGovernance();
        vault = new Vault(IERC20(address(1)), vm.addr(KEY), governance, multisig, BUDGET);
        monkey = vault.monkey();
        fixture = vm.readFile(string.concat(vm.projectRoot(), "/../crates/core/tests/fixtures/emissions.json"));
    }

    function testVaultIsTheOnlyMinterUpToTheNominalSupply() public {
        assertEq(monkey.minter(), address(vault));
        assertEq(monkey.cap(), 2 * 26_280 * BUDGET);
        assertEq(monkey.decimals(), 18);
        assertEq(monkey.symbol(), "MONKEY");
        assertEq(monkey.totalSupply(), 0);
        vm.expectRevert(abi.encodeWithSelector(MonkeyToken.NotMinter.selector, address(this)));
        monkey.mint(address(this), 1);
    }

    function testRefusesABudgetOutsideTheEngineRange() public {
        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidPeriodBudget.selector, 0));
        new Vault(IERC20(address(1)), vm.addr(KEY), governance, multisig, 0);
        uint256 max = vault.MAX_PERIOD_BUDGET();
        assertEq(max, type(uint128).max / (2 * 26_280));
        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidPeriodBudget.selector, max + 1));
        new Vault(IERC20(address(1)), vm.addr(KEY), governance, multisig, max + 1);
        assertEq(
            new Vault(IERC20(address(1)), vm.addr(KEY), governance, multisig, max).monkey().cap(), 2 * 26_280 * max
        );
    }

    function testEngineProofMintsOnceToTheAccountWhoeverRelays() public {
        Claim memory first = load(".first");
        post(first.seq, first.root, false, 0, 0);
        address relayer = makeAddr("relayer");
        vm.expectEmit(address(vault));
        emit Vault.EmissionsClaimed(first.account, first.entitled, first.entitled, first.seq);
        vm.prank(relayer);
        assertEq(vault.claimEmissions(first.account, first.entitled, first.siblings), first.entitled);
        assertEq(monkey.balanceOf(first.account), first.entitled);
        assertEq(monkey.balanceOf(relayer), 0);
        assertEq(vault.emissionsClaimed(first.account), first.entitled);

        vm.expectRevert(abi.encodeWithSelector(Vault.NothingToClaim.selector, first.entitled, first.entitled));
        vault.claimEmissions(first.account, first.entitled, first.siblings);
        assertEq(monkey.totalSupply(), first.entitled);

        Claim memory other = load(".other");
        vault.claimEmissions(other.account, other.entitled, other.siblings);
        assertEq(monkey.balanceOf(other.account), other.entitled);
    }

    function testALargerEntitlementMintsOnlyTheIncrement() public {
        Claim memory first = load(".first");
        Claim memory later = load(".later");
        assertEq(later.account, first.account);
        assertGt(later.entitled, first.entitled);
        post(first.seq, first.root, false, 0, 0);
        vault.claimEmissions(first.account, first.entitled, first.siblings);
        post(later.seq, later.root, true, first.seq, first.root);

        // The earlier proof no longer matches the latest root.
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(first.account, first.entitled, first.siblings);
        assertEq(vault.claimEmissions(later.account, later.entitled, later.siblings), later.entitled - first.entitled);
        assertEq(monkey.balanceOf(first.account), later.entitled);
    }

    function testUnclaimedEntitlementStaysClaimableAcrossLaterRoots() public {
        Claim memory first = load(".first");
        Claim memory later = load(".later");
        post(first.seq, first.root, false, 0, 0);
        post(later.seq, later.root, true, first.seq, first.root);
        assertEq(vault.claimEmissions(later.account, later.entitled, later.siblings), later.entitled);
    }

    function testRefusesEveryAlteredProof() public {
        Claim memory first = load(".first");
        Claim memory other = load(".other");
        post(first.seq, first.root, false, 0, 0);
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(first.account, first.entitled + 1, first.siblings);
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(other.account, first.entitled, first.siblings);
        bytes32[] memory tampered = first.siblings;
        tampered[tampered.length - 1] ^= bytes32(uint256(1));
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(first.account, first.entitled, tampered);
        bytes32[] memory shorter = new bytes32[](first.siblings.length - 1);
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(first.account, first.entitled, shorter);
        assertEq(monkey.totalSupply(), 0);
    }

    function testRefusesClaimsBeforeAnyCheckpoint() public {
        Claim memory first = load(".first");
        vm.expectRevert(Vault.NoCheckpoint.selector);
        vault.claimEmissions(first.account, first.entitled, first.siblings);
    }

    function testFuzzSingleLeafTreeClaimsUpToTheCap(address account, uint256 entitled) public {
        vm.assume(account != address(0));
        entitled = bound(entitled, 1, monkey.cap());
        post(1, leaf(account, entitled), false, 0, 0);
        vault.claimEmissions(account, entitled, new bytes32[](0));
        assertEq(monkey.balanceOf(account), entitled);
        vm.expectRevert(abi.encodeWithSelector(Vault.NothingToClaim.selector, entitled, entitled));
        vault.claimEmissions(account, entitled, new bytes32[](0));
    }

    function testAnEntitlementPastTheCapMintsNothing() public {
        address account = makeAddr("account");
        uint256 cap = monkey.cap();
        post(1, leaf(account, cap + 1), false, 0, 0);
        vm.expectRevert(abi.encodeWithSelector(ERC20Capped.ERC20ExceededCap.selector, cap + 1, cap));
        vault.claimEmissions(account, cap + 1, new bytes32[](0));
        assertEq(vault.emissionsClaimed(account), 0);
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
        bytes32 leftLeaf = leaf(left, 5);
        bytes32 rightLeaf = leaf(right, 7);
        post(1, keccak256(abi.encodePacked(bytes1(0x01), leftLeaf, rightLeaf)), false, 0, 0);
        bytes32[] memory proof = new bytes32[](1);
        proof[0] = leftLeaf;
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(left, 5, proof);
        proof[0] = rightLeaf;
        vault.claimEmissions(left, 5, proof);
        proof[0] = leftLeaf;
        vault.claimEmissions(right, 7, proof);
        assertEq(monkey.balanceOf(left), 5);
        assertEq(monkey.balanceOf(right), 7);
        vm.expectRevert(Vault.InvalidEntitlementProof.selector);
        vault.claimEmissions(right, 8, new bytes32[](257));
    }

    function load(string memory at) private view returns (Claim memory claim) {
        claim.seq = uint64(vm.parseJsonUint(fixture, string.concat(at, ".seq")));
        claim.root = vm.parseJsonBytes32(fixture, string.concat(at, ".root"));
        claim.account = vm.parseJsonAddress(fixture, string.concat(at, ".account"));
        claim.entitled = vm.parseJsonUint(fixture, string.concat(at, ".entitled"));
        claim.siblings = vm.parseJsonBytes32Array(fixture, string.concat(at, ".siblings"));
    }

    function key(address account) private pure returns (bytes32) {
        return keccak256(abi.encodePacked(ENTITLEMENT_LEAF, account));
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
