// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Staking} from "../src/Staking.sol";
import {Vault} from "../src/Vault.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// MONKEY staking: the contract custodies tokens, enforces the unstake cooldown, and emits the
/// events the engine mirrors.
contract StakingTest is Test {
    uint256 private constant KEY = 0x1234;
    uint256 private constant COOLDOWN = 3 days;
    /// @dev One engine distribution epoch, in seconds.
    uint256 private constant EPOCH = 30 minutes;

    MockUSDC internal usdc;
    Vault internal vault;
    MonkeyToken internal monkey;
    Staking internal staking;
    address internal staker = makeAddr("staker");

    function setUp() public {
        usdc = new MockUSDC();
        address[] memory proposers = new address[](1);
        proposers[0] = vm.addr(KEY);
        address[] memory executors = new address[](1);
        TimelockController governance = new TimelockController(2 days, proposers, executors, address(0));
        vault = new Vault(usdc, vm.addr(KEY), governance, proposers[0], 1e18);
        monkey = vault.monkey();
        staking = new Staking(monkey, COOLDOWN, EPOCH);
    }

    function _mintMonkey(address to, uint256 amount) internal {
        // The vault is MONKEY's only minter; claims are the mint path. Mint directly through it in
        // place of a proven claim: the cap is far above anything this test needs.
        vm.prank(address(vault));
        monkey.mint(to, amount);
    }

    function test_StakePullsMonkeyAndCountsIt() public {
        _mintMonkey(staker, 1_000);
        vm.startPrank(staker);
        monkey.approve(address(staking), 700);
        vm.expectEmit(address(staking));
        emit Staking.Staked(staker, 700);
        staking.stake(700);
        vm.stopPrank();
        assertEq(staking.stakes(staker), 700);
        assertEq(monkey.balanceOf(address(staking)), 700);
        assertEq(monkey.balanceOf(staker), 300);

        vm.expectRevert(abi.encodeWithSelector(Staking.InvalidAmount.selector, 0));
        vm.prank(staker);
        staking.stake(0);
    }

    function test_UnstakeCompletesOnlyAfterTheCooldown() public {
        _mintMonkey(staker, 1_000);
        vm.startPrank(staker);
        monkey.approve(address(staking), 1_000);
        staking.stake(1_000);

        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientStake.selector, 1_000, 1_001));
        staking.startUnstake(1_001);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientStake.selector, 1_000, 0));
        staking.startUnstake(0);

        vm.expectEmit(address(staking));
        emit Staking.UnstakeStarted(staker, 400, block.timestamp + COOLDOWN);
        staking.startUnstake(400);
        assertEq(staking.stakes(staker), 600);

        vm.warp(block.timestamp + COOLDOWN - 1);
        vm.expectRevert(abi.encodeWithSelector(Staking.CooldownNotPassed.selector, block.timestamp + 1));
        staking.completeUnstake();

        vm.warp(block.timestamp + 1);
        vm.expectEmit(address(staking));
        emit Staking.UnstakeCompleted(staker, 400);
        staking.completeUnstake();
        assertEq(monkey.balanceOf(staker), 400);
        assertEq(monkey.balanceOf(address(staking)), 600);

        vm.expectRevert(Staking.NoPendingUnstake.selector);
        staking.completeUnstake();
        vm.stopPrank();
    }

    function test_ASecondUnstakeAddsToThePendingAndRestartsItsCooldown() public {
        _mintMonkey(staker, 1_000);
        vm.startPrank(staker);
        monkey.approve(address(staking), 1_000);
        staking.stake(1_000);
        staking.startUnstake(400);
        vm.warp(block.timestamp + COOLDOWN);
        staking.startUnstake(100);
        // The new amount restarted the cooldown from now.
        (uint256 amount, uint256 claimableAt) = staking.unstakes(staker);
        assertEq(amount, 500);
        assertEq(claimableAt, block.timestamp + COOLDOWN);
        vm.warp(block.timestamp + COOLDOWN);
        staking.completeUnstake();
        assertEq(monkey.balanceOf(staker), 500);
        assertEq(staking.stakes(staker), 500);
        vm.stopPrank();
    }

    function test_ConstructorRefusesANullSetup() public {
        vm.expectRevert(Staking.ZeroAddress.selector);
        new Staking(IERC20(address(0)), COOLDOWN, EPOCH);
        vm.expectRevert(abi.encodeWithSelector(Staking.InvalidAmount.selector, 0));
        new Staking(monkey, 0, EPOCH);
        vm.expectRevert(abi.encodeWithSelector(Staking.CooldownBelowEpoch.selector, EPOCH - 1, EPOCH));
        new Staking(monkey, EPOCH - 1, EPOCH);
    }

    function test_OnlyTheRegistryLocksAndReleasesSelfBonds() public {
        // This test deployed the contract, so it stands in for the registry.
        assertEq(staking.registry(), address(this));
        vm.expectRevert(abi.encodeWithSelector(Staking.NotRegistry.selector, staker));
        vm.prank(staker);
        staking.lockBond(staker, 1);
        vm.expectRevert(abi.encodeWithSelector(Staking.NotRegistry.selector, staker));
        vm.prank(staker);
        staking.releaseBond(staker);
    }

    function test_ALockedSelfBondStaysStakedUntilOneCooldownAfterItsRelease() public {
        _mintMonkey(staker, 1_000);
        vm.startPrank(staker);
        monkey.approve(address(staking), 1_000);
        staking.stake(1_000);
        vm.stopPrank();
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientStake.selector, 1_000, 1_001));
        staking.lockBond(staker, 1_001);
        vm.expectEmit(address(staking));
        emit Staking.BondLocked(staker, 600);
        staking.lockBond(staker, 600);
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 600));
        staking.lockBond(staker, 100);

        vm.startPrank(staker);
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 600));
        staking.startUnstake(401);
        staking.startUnstake(400);
        vm.stopPrank();

        vm.expectEmit(address(staking));
        emit Staking.BondReleasing(staker, 600, block.timestamp + COOLDOWN);
        staking.releaseBond(staker);
        vm.warp(block.timestamp + COOLDOWN - 1);
        assertEq(staking.lockedBond(staker), 600);
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 600));
        vm.prank(staker);
        staking.startUnstake(1);
        vm.warp(block.timestamp + 1);
        assertEq(staking.lockedBond(staker), 0);
        vm.prank(staker);
        staking.startUnstake(600);
        assertEq(staking.stakes(staker), 0);
    }

    function testFuzz_VotesAreTheStakeOfTheAccountsDelegatingToEachCandidate(
        uint256 first,
        uint256 second,
        uint256 unstaked,
        bool switchFirst
    ) public {
        address other = makeAddr("other staker");
        address[2] memory candidates = [makeAddr("candidate a"), makeAddr("candidate b")];
        first = bound(first, 1, monkey.cap() / 2);
        second = bound(second, 1, monkey.cap() / 2);
        unstaked = bound(unstaked, 1, first);
        _stakeFor(staker, first);
        vm.prank(staker);
        staking.delegate(candidates[0]);
        vm.prank(other);
        staking.delegate(candidates[1]);
        _stakeFor(other, second);
        assertEq(staking.votes(candidates[0]), first);
        assertEq(staking.votes(candidates[1]), second);

        vm.startPrank(staker);
        staking.startUnstake(unstaked);
        if (switchFirst) staking.delegate(candidates[1]);
        vm.stopPrank();
        uint256 remaining = first - unstaked;
        assertEq(staking.votes(candidates[0]), switchFirst ? 0 : remaining);
        assertEq(staking.votes(candidates[1]), second + (switchFirst ? remaining : 0));
        assertEq(staking.totalStaked(), remaining + second);
    }

    function _stakeFor(address account, uint256 amount) internal {
        _mintMonkey(account, amount);
        vm.startPrank(account);
        monkey.approve(address(staking), amount);
        staking.stake(amount);
        vm.stopPrank();
    }

    function testFuzz_StakeThenUnstakeReturnsWhatWasStaked(uint256 amount) public {
        amount = bound(amount, 1, monkey.cap());
        _mintMonkey(staker, amount);
        vm.startPrank(staker);
        monkey.approve(address(staking), amount);
        staking.stake(amount);
        staking.startUnstake(amount);
        vm.warp(block.timestamp + COOLDOWN);
        staking.completeUnstake();
        assertEq(monkey.balanceOf(staker), amount);
        assertEq(monkey.balanceOf(address(staking)), 0);
        vm.stopPrank();
    }
}
