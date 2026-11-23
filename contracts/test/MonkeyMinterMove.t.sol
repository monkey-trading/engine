// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {MonkeyMinter} from "../src/MonkeyMinter.sol";
import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Vault} from "../src/Vault.sol";
import {ExitTest} from "./helpers/ExitTest.sol";
import {TreeBuilder} from "./helpers/TreeBuilder.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// Minting moved from the alpha's vault to a launch vault whose log carries each account's
/// entitlement: one record spans both, so nothing mints twice, and the alpha's exit, which pays its
/// own USDC once, has no way to mint.
contract MonkeyMinterMoveTest is ExitTest {
    uint256 private constant LAUNCH_KEY = 0x5678;
    uint8 private constant ENTITLEMENT_LEAF = 8;
    uint8 private constant DISTRIBUTION_LEAF = 13;

    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    MonkeyMinter internal minter;
    MonkeyToken internal monkey;
    Vault internal launch;

    function setUp() public override {
        super.setUp();
        minter = new MonkeyMinter(governance, 1e24);
        monkey = minter.monkey();
        // Before any checkpoint, so the timelock's delay moves no exit timing.
        _govern(address(minter), abi.encodeCall(MonkeyMinter.setVault, (vault)));
        launch = new Vault(new MockUSDC(), vm.addr(LAUNCH_KEY), governance, multisig, PERIOD, WINDOW, STALE);
    }

    function test_TheLaunchVaultMintsOnlyWhatTheCarriedEntitlementAdds() public {
        // On the alpha, bob claims 3 of what becomes 5, and carol nothing of her 1.
        _addEntitlement(bob, 3e18);
        _addEntitlement(carol, 1e18);
        _post();
        uint64 firstSeq = seq;
        bytes32[] memory firstProof = _siblings(_entitlementKey(bob));
        assertEq(minter.claimEmissions(bob, 3e18, firstSeq, firstProof), 3e18);
        _clearTree();
        _addEntitlement(bob, 5e18);
        _addEntitlement(carol, 1e18);
        _post();
        uint64 last = seq;
        bytes32[] memory lastProof = _siblings(_entitlementKey(bob));

        _move();
        // The alpha's roots mint nothing once the minter follows the launch vault.
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        minter.claimEmissions(bob, 5e18, last, lastProof);

        // The launch log's first checkpoint, at the same seq as the alpha's partial claim's, carries
        // each account's gross entitlement.
        (bytes32[] memory keys_, bytes32[] memory values_) = _carried(5e18, 1e18);
        _postLaunch(firstSeq, TreeBuilder.root(keys_, values_));
        vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
        minter.claimEmissions(bob, 3e18, firstSeq, firstProof);
        assertEq(minter.claimEmissions(bob, 5e18, firstSeq, TreeBuilder.proof(keys_, values_, 0)), 2e18);
        assertEq(minter.claimEmissions(carol, 1e18, firstSeq, TreeBuilder.proof(keys_, values_, 1)), 1e18);
        vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.NothingToClaim.selector, 5e18, 5e18));
        minter.claimEmissions(bob, 5e18, firstSeq, TreeBuilder.proof(keys_, values_, 0));
        assertEq(monkey.balanceOf(bob), 5e18);
        assertEq(monkey.totalSupply(), 6e18, "the highest entitlements, minted once");
    }

    function test_TheAlphaExitPaysItsDistributionOnceAndMintsNothing() public {
        usdc.mint(address(vault), 100e6);
        _addEntitlement(bob, 1e18);
        _addDistribution(bob, 10e6);
        _post();
        assertEq(minter.claimEmissions(bob, 1e18, seq, _siblings(_entitlementKey(bob))), 1e18);
        assertEq(vault.claimDistribution(bob, 10e6, seq, _siblings(_distributionKey(bob))), 10e6);
        _clearTree();
        ExitVerifier.ExitTerms memory terms = _addTerms(0, 0, 0);
        _addEntitlement(bob, 3e18);
        _addDistribution(bob, 30e6);
        _post();
        bytes32[] memory emission = _siblings(_entitlementKey(bob));
        bytes32[] memory distribution = _siblings(_distributionKey(bob));

        _move();
        _startExit(terms);
        vault.registerExitDistribution(bob, 30e6, distribution);
        assertEq(vault.exitDistributionClaims(bob), 20e6, "what the alpha already paid is deducted");
        vm.expectRevert(Vault.AlreadyRegistered.selector);
        vault.registerExitDistribution(bob, 30e6, distribution);
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        minter.claimEmissions(bob, 3e18, seq, emission);

        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(bob), 20e6);
        vm.expectRevert(Vault.NoExitClaim.selector);
        vault.claimExit(bob);
        assertEq(usdc.balanceOf(bob), 30e6);
        assertEq(monkey.totalSupply(), 1e18, "the exit minted nothing");
        assertEq(launch.distributionsPaid(), 0, "the launch vault pays nothing of the alpha's");
    }

    /// @dev The guardian retires the alpha's signer and the timelock moves the minter to the launch vault.
    function _move() private {
        vm.prank(multisig);
        vault.retireSigner();
        _govern(address(minter), abi.encodeCall(MonkeyMinter.setVault, (launch)));
        assertEq(address(minter.vault()), address(launch));
    }

    function _entitlementKey(address account) private pure returns (bytes32) {
        return keccak256(abi.encodePacked(ENTITLEMENT_LEAF, account));
    }

    function _distributionKey(address account) private pure returns (bytes32) {
        return keccak256(abi.encodePacked(DISTRIBUTION_LEAF, account));
    }

    function _addEntitlement(address account, uint256 entitled) private {
        _leaf(_entitlementKey(account), keccak256(abi.encode(entitled)));
    }

    function _clearTree() private {
        delete keys;
        delete values;
    }

    /// @dev Bob's and carol's entitlement leaves, in that order.
    function _carried(uint256 bobs, uint256 carols)
        private
        view
        returns (bytes32[] memory keys_, bytes32[] memory values_)
    {
        keys_ = new bytes32[](2);
        values_ = new bytes32[](2);
        (keys_[0], values_[0]) = (_entitlementKey(bob), keccak256(abi.encode(bobs)));
        (keys_[1], values_[1]) = (_entitlementKey(carol), keccak256(abi.encode(carols)));
    }

    /// @dev Posts `root` as the launch vault's first checkpoint, at `at`.
    function _postLaunch(uint64 at, bytes32 root) private {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2", uint64(block.chainid), address(launch), at, root, false, uint64(0), bytes32(0)
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(LAUNCH_KEY, digest);
        launch.postCheckpoint(at, root, false, 0, bytes32(0), abi.encodePacked(r, s, v));
    }
}
