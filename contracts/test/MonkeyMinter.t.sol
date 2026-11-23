// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {ERC20Capped} from "@openzeppelin-contracts/token/ERC20/extensions/ERC20Capped.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {MonkeyMinter} from "../src/MonkeyMinter.sol";
import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Vault} from "../src/Vault.sol";

/// MONKEY claims through the minter against the posted checkpoint roots of the vault it follows, and
/// the timelock's moves of that vault. `crates/core/tests/fixtures/emissions.json` holds entitlement
/// proofs the engine produced; `emissions_fixture.rs` checks them on the Rust side.
contract MonkeyMinterTest is GovernanceTest {
    uint256 private constant KEY = 0x1234;
    uint256 private constant BUDGET = 1_000_000;
    uint8 private constant ENTITLEMENT_LEAF = 8;

    Vault private vault;
    MonkeyMinter private minter;
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
        vault =
            new Vault(IERC20(address(1)), vm.addr(KEY), governance, multisig, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER);
        minter = new MonkeyMinter(governance, BUDGET);
        monkey = minter.monkey();
        _govern(address(minter), abi.encodeCall(MonkeyMinter.setVault, (vault)));
        fixture = vm.readFile(string.concat(vm.projectRoot(), "/../crates/core/tests/fixtures/emissions.json"));
    }

    function testTheMinterIsTheOnlyMinterUpToTheNominalSupply() public {
        assertEq(monkey.minter(), address(minter));
        assertEq(monkey.cap(), 2 * 26_280 * BUDGET);
        assertEq(monkey.decimals(), 18);
        assertEq(monkey.symbol(), "MONKEY");
        assertEq(monkey.totalSupply(), 0);
        vm.expectRevert(abi.encodeWithSelector(MonkeyToken.NotMinter.selector, address(this)));
        monkey.mint(address(this), 1);
        vm.expectRevert(abi.encodeWithSelector(MonkeyToken.NotMinter.selector, address(vault)));
        vm.prank(address(vault));
        monkey.mint(address(this), 1);
    }

    function testRefusesABudgetOutsideTheEngineRange() public {
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.InvalidPeriodBudget.selector, 0));
        new MonkeyMinter(governance, 0);
        uint256 max = minter.MAX_PERIOD_BUDGET();
        assertEq(max, type(uint128).max / (2 * 26_280));
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.InvalidPeriodBudget.selector, max + 1));
        new MonkeyMinter(governance, max + 1);
        assertEq(new MonkeyMinter(governance, max).monkey().cap(), 2 * 26_280 * max);
    }

    function testRefusesATimelockWithNoCodeOrNoDelay() public {
        vm.expectRevert(MonkeyMinter.InvalidGovernance.selector);
        new MonkeyMinter(TimelockController(payable(multisig)), BUDGET);
        address[] memory roles = new address[](0);
        TimelockController zeroDelay = new TimelockController(0, roles, roles, address(0));
        vm.expectRevert(MonkeyMinter.InvalidGovernance.selector);
        new MonkeyMinter(zeroDelay, BUDGET);
    }

    function testOnlyTheTimelockNamesTheVaultAndOnlyOneWithCode() public {
        MonkeyMinter fresh = new MonkeyMinter(governance, BUDGET);
        vm.expectRevert(MonkeyMinter.UnauthorizedGovernance.selector);
        fresh.setVault(vault);
        vm.expectRevert(MonkeyMinter.UnauthorizedGovernance.selector);
        vm.prank(multisig);
        fresh.setVault(vault);
        vm.expectRevert(MonkeyMinter.InvalidVault.selector);
        vm.prank(address(governance));
        fresh.setVault(Vault(makeAddr("no code")));
        assertEq(address(fresh.vault()), address(0));

        vm.expectEmit(address(fresh));
        emit MonkeyMinter.VaultSet(address(0), address(vault));
        vm.prank(address(governance));
        fresh.setVault(vault);
        assertEq(address(fresh.vault()), address(vault));
    }

    function testNothingMintsBeforeTheTimelockNamesAVault() public {
        MonkeyMinter fresh = new MonkeyMinter(governance, BUDGET);
        post(1, leaf(address(this), 5), false, 0, 0);
        vm.expectRevert(MonkeyMinter.VaultNotSet.selector);
        fresh.claimEmissions(address(this), 5, 1, new bytes32[](0));
    }

    function testTheVaultMovesOnlyOnceItsSignerIsRetired() public {
        Vault next = new Vault(
            IERC20(address(1)), vm.addr(KEY + 1), governance, multisig, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER
        );
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.SignerLive.selector, vm.addr(KEY)));
        vm.prank(address(governance));
        minter.setVault(next);

        // A rotation leaves a live signer, which still refuses the move.
        vm.prank(address(governance));
        vault.setSigner(vm.addr(KEY + 2));
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.SignerLive.selector, vm.addr(KEY + 2)));
        vm.prank(address(governance));
        minter.setVault(next);

        vm.prank(multisig);
        vault.retireSigner();
        vm.expectEmit(address(minter));
        emit MonkeyMinter.VaultSet(address(vault), address(next));
        vm.prank(address(governance));
        minter.setVault(next);
        assertEq(address(minter.vault()), address(next));

        // And back, once the new vault's signer is retired too.
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.SignerLive.selector, vm.addr(KEY + 1)));
        vm.prank(address(governance));
        minter.setVault(vault);
        vm.prank(multisig);
        next.retireSigner();
        vm.prank(address(governance));
        minter.setVault(vault);
        assertEq(address(minter.vault()), address(vault));
    }

    function testEngineProofMintsOnceToTheAccountWhoeverRelays() public {
        Claim memory first = load(".first");
        post(first.seq, first.root, false, 0, 0);
        address relayer = makeAddr("relayer");
        vm.expectEmit(address(minter));
        emit MonkeyMinter.EmissionsClaimed(first.account, first.entitled, first.entitled, first.seq);
        vm.prank(relayer);
        assertEq(minter.claimEmissions(first.account, first.entitled, first.seq, first.siblings), first.entitled);
        assertEq(monkey.balanceOf(first.account), first.entitled);
        assertEq(monkey.balanceOf(relayer), 0);
        assertEq(minter.emissionsClaimed(first.account), first.entitled);

        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.NothingToClaim.selector, first.entitled, first.entitled));
        minter.claimEmissions(first.account, first.entitled, first.seq, first.siblings);
        assertEq(monkey.totalSupply(), first.entitled);

        Claim memory other = load(".other");
        minter.claimEmissions(other.account, other.entitled, other.seq, other.siblings);
        assertEq(monkey.balanceOf(other.account), other.entitled);
    }

    function testALargerEntitlementMintsOnlyTheIncrement() public {
        Claim memory first = load(".first");
        Claim memory later = load(".later");
        assertEq(later.account, first.account);
        assertGt(later.entitled, first.entitled);
        post(first.seq, first.root, false, 0, 0);
        minter.claimEmissions(first.account, first.entitled, first.seq, first.siblings);
        post(later.seq, later.root, true, first.seq, first.root);

        // The earlier proof still checks against its root, and has nothing left to pay.
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.NothingToClaim.selector, first.entitled, first.entitled));
        minter.claimEmissions(first.account, first.entitled, first.seq, first.siblings);
        assertEq(
            minter.claimEmissions(later.account, later.entitled, later.seq, later.siblings),
            later.entitled - first.entitled
        );
        assertEq(monkey.balanceOf(first.account), later.entitled);
    }

    function testUnclaimedEntitlementStaysClaimableAcrossLaterRoots() public {
        Claim memory first = load(".first");
        Claim memory later = load(".later");
        post(first.seq, first.root, false, 0, 0);
        post(later.seq, later.root, true, first.seq, first.root);
        assertEq(minter.claimEmissions(later.account, later.entitled, later.seq, later.siblings), later.entitled);
    }

    function testRefusesEveryAlteredProof() public {
        Claim memory first = load(".first");
        Claim memory other = load(".other");
        post(first.seq, first.root, false, 0, 0);
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(first.account, first.entitled + 1, first.seq, first.siblings);
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(other.account, first.entitled, first.seq, first.siblings);
        bytes32[] memory tampered = first.siblings;
        tampered[tampered.length - 1] ^= bytes32(uint256(1));
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(first.account, first.entitled, first.seq, tampered);
        bytes32[] memory shorter = new bytes32[](first.siblings.length - 1);
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(first.account, first.entitled, first.seq, shorter);
        assertEq(monkey.totalSupply(), 0);
    }

    function testRefusesClaimsBeforeAnyCheckpoint() public {
        Claim memory first = load(".first");
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        minter.claimEmissions(first.account, first.entitled, first.seq, first.siblings);
    }

    function testFuzzSingleLeafTreeClaimsUpToTheCap(address account, uint256 entitled) public {
        vm.assume(account != address(0));
        entitled = bound(entitled, 1, monkey.cap());
        post(1, leaf(account, entitled), false, 0, 0);
        minter.claimEmissions(account, entitled, 1, new bytes32[](0));
        assertEq(monkey.balanceOf(account), entitled);
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.NothingToClaim.selector, entitled, entitled));
        minter.claimEmissions(account, entitled, 1, new bytes32[](0));
    }

    function testAnEntitlementPastTheCapMintsNothing() public {
        address account = makeAddr("account");
        uint256 cap = monkey.cap();
        post(1, leaf(account, cap + 1), false, 0, 0);
        vm.expectRevert(abi.encodeWithSelector(ERC20Capped.ERC20ExceededCap.selector, cap + 1, cap));
        minter.claimEmissions(account, cap + 1, 1, new bytes32[](0));
        assertEq(minter.emissionsClaimed(account), 0);
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
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(left, 5, 1, proof);
        proof[0] = rightLeaf;
        minter.claimEmissions(left, 5, 1, proof);
        proof[0] = leftLeaf;
        minter.claimEmissions(right, 7, 1, proof);
        assertEq(monkey.balanceOf(left), 5);
        assertEq(monkey.balanceOf(right), 7);
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(right, 8, 1, new bytes32[](257));
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
