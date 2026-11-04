// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {console} from "forge-std/console.sol";
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
    address internal candidateA = makeAddr("candidate a");
    address internal candidateB = makeAddr("candidate b");
    address internal candidateC = makeAddr("candidate c");

    function setUp() public {
        usdc = new MockUSDC();
        address[] memory proposers = new address[](1);
        proposers[0] = vm.addr(KEY);
        address[] memory executors = new address[](1);
        TimelockController governance = new TimelockController(2 days, proposers, executors, address(0));
        vault = new Vault(usdc, vm.addr(KEY), governance, proposers[0], 1e18, 365 days, 30 days, 365 days);

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

    function _stakeFor(address account, uint256 amount) internal {
        _mintMonkey(account, amount);
        vm.startPrank(account);
        monkey.approve(address(staking), amount);
        staking.stake(amount);
        vm.stopPrank();
    }

    function test_NewStakeIsUndelegatedAndAbstains() public {
        _stakeFor(staker, 1_000);
        assertEq(staking.delegatedStake(staker), 0);
        assertEq(staking.delegationsOf(staker).length, 0);
        vm.prank(staker);
        staking.delegate(candidateA, 400);
        _stakeFor(staker, 500);
        assertEq(staking.votes(candidateA), 400, "new stake adds no votes");
        assertEq(staking.delegatedStake(staker), 400);
        assertEq(staking.stakes(staker), 1_500);
    }

    function test_DelegateTakesOnlyUndelegatedStake() public {
        _stakeFor(staker, 1_000);
        vm.startPrank(staker);
        vm.expectRevert(Staking.ZeroAddress.selector);
        staking.delegate(address(0), 1);
        vm.expectRevert(abi.encodeWithSelector(Staking.InvalidAmount.selector, 0));
        staking.delegate(candidateA, 0);
        vm.expectEmit(address(staking));
        emit Staking.Delegated(staker, candidateA, 600);
        staking.delegate(candidateA, 600);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientUndelegated.selector, 400, 401));
        staking.delegate(candidateB, 401);
        staking.delegate(candidateB, 300);
        // A further amount to the same candidate adds to its delegation, not to the list.
        staking.delegate(candidateA, 100);
        vm.stopPrank();
        assertEq(staking.delegatedTo(staker, candidateA), 700);
        assertEq(staking.delegatedTo(staker, candidateB), 300);
        assertEq(staking.votes(candidateA), 700);
        assertEq(staking.votes(candidateB), 300);
        assertEq(staking.delegatedStake(staker), 1_000);
        _assertDelegations(staker, [candidateA, candidateB], [uint256(700), 300], 2);
    }

    function test_UndelegateReturnsAnAmountToUndelegatedStake() public {
        _stakeFor(staker, 1_000);
        vm.startPrank(staker);
        staking.delegate(candidateA, 600);
        staking.delegate(candidateB, 200);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientDelegation.selector, 600, 0));
        staking.undelegate(candidateA, 0);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientDelegation.selector, 600, 601));
        staking.undelegate(candidateA, 601);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientDelegation.selector, 0, 1));
        staking.undelegate(candidateC, 1);
        vm.expectEmit(address(staking));
        emit Staking.Undelegated(staker, candidateA, 250);
        staking.undelegate(candidateA, 250);
        assertEq(staking.votes(candidateA), 350);
        assertEq(staking.delegatedStake(staker), 550);
        // Taking a delegation back in full drops it from the list.
        staking.undelegate(candidateA, 350);
        // The stake taken back is undelegated again.
        staking.delegate(candidateC, 800);
        vm.stopPrank();
        assertEq(staking.votes(candidateA), 0);
        assertEq(staking.delegatedStake(staker), 1_000);
        _assertDelegations(staker, [candidateB, candidateC], [uint256(200), 800], 2);
    }

    function test_RedelegateMovesAnAmountInOneCall() public {
        _stakeFor(staker, 1_000);
        vm.startPrank(staker);
        staking.delegate(candidateA, 300);
        vm.expectRevert(Staking.ZeroAddress.selector);
        staking.redelegate(candidateA, address(0), 1);
        vm.expectRevert(abi.encodeWithSelector(Staking.SameCandidate.selector, candidateA));
        staking.redelegate(candidateA, candidateA, 1);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientDelegation.selector, 300, 0));
        staking.redelegate(candidateA, candidateB, 0);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientDelegation.selector, 300, 301));
        staking.redelegate(candidateA, candidateB, 301);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientDelegation.selector, 0, 1));
        staking.redelegate(candidateB, candidateA, 1);
        vm.expectEmit(address(staking));
        emit Staking.Redelegated(staker, candidateA, candidateB, 100);
        staking.redelegate(candidateA, candidateB, 100);
        assertEq(staking.votes(candidateA), 200);
        assertEq(staking.votes(candidateB), 100);
        assertEq(staking.delegatedStake(staker), 300);
        // Moving the rest empties the delegation, which leaves the list.
        staking.redelegate(candidateA, candidateB, 200);
        vm.stopPrank();
        assertEq(staking.votes(candidateA), 0);
        assertEq(staking.votes(candidateB), 300);
        assertEq(staking.delegatedStake(staker), 300);
        _assertDelegations(staker, [candidateB, address(0)], [uint256(300), 0], 1);
    }

    function test_AnUnstakeTakesUndelegatedStakeFirst() public {
        _stakeFor(staker, 1_000);
        vm.startPrank(staker);
        staking.delegate(candidateA, 400);
        vm.recordLogs();
        staking.startUnstake(600);
        vm.stopPrank();
        assertEq(vm.getRecordedLogs().length, 1, "only UnstakeStarted");
        assertEq(staking.votes(candidateA), 400);
        assertEq(staking.delegatedStake(staker), 400);
        assertEq(staking.stakes(staker), 400);
    }

    /// @dev The issue's scenario: 300 to A and 200 to B, 100 moved from A to B and 100 taken back
    /// from B, then an unstake of 700 that takes the 600 undelegated and 50 from each delegation.
    function test_TheRestOfAnUnstakeReducesEachDelegationInProportion() public {
        _stakeFor(staker, 1_000);
        vm.startPrank(staker);
        staking.delegate(candidateA, 300);
        staking.delegate(candidateB, 200);
        staking.redelegate(candidateA, candidateB, 100);
        staking.undelegate(candidateB, 100);
        assertEq(staking.votes(candidateA), 200);
        assertEq(staking.votes(candidateB), 200);
        // The reductions run from the last listed delegation to the first, before `UnstakeStarted`.
        vm.expectEmit(address(staking));
        emit Staking.Undelegated(staker, candidateB, 50);
        vm.expectEmit(address(staking));
        emit Staking.Undelegated(staker, candidateA, 50);
        vm.expectEmit(address(staking));
        emit Staking.UnstakeStarted(staker, 700, block.timestamp + COOLDOWN);
        staking.startUnstake(700);
        vm.stopPrank();
        assertEq(staking.votes(candidateA), 150);
        assertEq(staking.votes(candidateB), 150);
        assertEq(staking.delegatedStake(staker), 300);
        assertEq(staking.stakes(staker), 300);
        _assertDelegations(staker, [candidateA, candidateB], [uint256(150), 150], 2);
    }

    function test_TheFirstDelegationGivesTheRoundingRemainder() public {
        _stakeFor(staker, 3_000);
        vm.startPrank(staker);
        staking.delegate(candidateA, 1_000);
        staking.delegate(candidateB, 1_000);
        staking.delegate(candidateC, 1_000);
        // A third of 1,000 each: C and B give 333 rounded down, and A, first, the 334 left.
        staking.startUnstake(1_000);
        vm.stopPrank();
        assertEq(staking.votes(candidateA), 666);
        assertEq(staking.votes(candidateB), 667);
        assertEq(staking.votes(candidateC), 667);
        assertEq(staking.delegatedStake(staker), 2_000);
    }

    function test_TheTalliesStayExactWhereNoOneDelegationCouldTakeTheRemainder() public {
        _stakeFor(staker, 1_000_002);
        vm.startPrank(staker);
        staking.delegate(candidateA, 1);
        staking.delegate(candidateB, 1);
        staking.delegate(candidateC, 1_000_000);
        // Rounded down, A's and B's shares of the 1,000,001 taken are nothing and C's is 999,999, so
        // no single delegation could give the 2 left; giving each its share of what is still to take
        // empties A and B and leaves C the one unit kept.
        staking.startUnstake(1_000_001);
        vm.stopPrank();
        assertEq(staking.votes(candidateA), 0);
        assertEq(staking.votes(candidateB), 0);
        assertEq(staking.votes(candidateC), 1);
        assertEq(staking.delegatedStake(staker), 1);
        _assertDelegations(staker, [candidateC, address(0)], [uint256(1), 0], 1);
    }

    function test_AFullUnstakeEmptiesEveryDelegation() public {
        _stakeFor(staker, 1_000);
        vm.startPrank(staker);
        staking.delegate(candidateA, 500);
        staking.delegate(candidateB, 300);
        staking.delegate(candidateC, 100);
        staking.startUnstake(1_000);
        vm.stopPrank();
        assertEq(staking.votes(candidateA) + staking.votes(candidateB) + staking.votes(candidateC), 0);
        assertEq(staking.delegatedStake(staker), 0);
        assertEq(staking.delegationsOf(staker).length, 0);
    }

    function test_ALockedSelfBondStaysStakedWhileItsDelegationsShrink() public {
        _stakeFor(staker, 1_000);
        // This test deployed the contract, so it stands in for the registry.
        staking.lockBond(staker, 600);
        vm.startPrank(staker);
        staking.delegate(staker, 1_000);
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 600));
        staking.startUnstake(401);
        staking.startUnstake(400);
        vm.stopPrank();
        assertEq(staking.votes(staker), 600);
        assertEq(staking.stakes(staker), 600);
    }

    function testFuzz_AnUnstakeTakesUndelegatedStakeFirstThenEachDelegationInProportion(
        uint256[4] memory delegated,
        uint256 undelegated,
        uint256 amount
    ) public {
        address[4] memory candidates = [candidateA, candidateB, candidateC, makeAddr("candidate d")];
        uint256 total = 0;
        for (uint256 i = 0; i < candidates.length; i++) {
            delegated[i] = bound(delegated[i], 1, monkey.cap() / 8);
            total += delegated[i];
        }
        undelegated = bound(undelegated, 0, monkey.cap() / 8);
        _stakeFor(staker, total + undelegated);
        vm.startPrank(staker);
        for (uint256 i = 0; i < candidates.length; i++) {
            staking.delegate(candidates[i], delegated[i]);
        }
        amount = bound(amount, 1, total + undelegated);
        staking.startUnstake(amount);
        vm.stopPrank();

        uint256 fromDelegations = amount > undelegated ? amount - undelegated : 0;
        uint256 taken = 0;
        for (uint256 i = 0; i < candidates.length; i++) {
            uint256 left = staking.delegatedTo(staker, candidates[i]);
            uint256 reduction = delegated[i] - left;
            // Within rounding of its proportional share: at least the share rounded down, and less
            // than one unit per delegation above it.
            uint256 share = fromDelegations * delegated[i] / total;
            assertGe(reduction, share);
            assertLt(reduction, share + candidates.length);
            assertEq(staking.votes(candidates[i]), left);
            taken += reduction;
        }
        assertEq(taken, fromDelegations, "the reductions sum to the delegated part exactly");
        assertEq(staking.delegatedStake(staker), total - fromDelegations);
        assertLe(staking.delegatedStake(staker), staking.stakes(staker));
        assertEq(staking.stakes(staker), total + undelegated - amount);
    }

    function testFuzz_AMoveChangesNeitherTheDelegatedStakeNorTheVotes(uint256 toA, uint256 toB, uint256 moved, bool toC)
        public
    {
        toA = bound(toA, 1, monkey.cap() / 4);
        toB = bound(toB, 1, monkey.cap() / 4);
        moved = bound(moved, 1, toA);
        address to = toC ? candidateC : candidateB;
        _stakeFor(staker, toA + toB);
        vm.startPrank(staker);
        staking.delegate(candidateA, toA);
        staking.delegate(candidateB, toB);
        staking.redelegate(candidateA, to, moved);
        vm.stopPrank();
        assertEq(staking.delegatedStake(staker), toA + toB);
        assertEq(staking.votes(candidateA) + staking.votes(candidateB) + staking.votes(candidateC), toA + toB);
        assertEq(staking.votes(candidateA), toA - moved);
        assertEq(staking.votes(to), (toC ? 0 : toB) + moved);
        assertEq(staking.delegationsOf(staker).length, (moved == toA ? 1 : 2) + (toC ? 1 : 0));
    }

    /// @notice Reports what an unstake costs per delegation it reduces, before refunds and with cold
    /// storage as in its own transaction: partly, which keeps every delegation, and in full, which
    /// empties them and drops them from the list. About 16k and 25k gas.
    function test_GasOfAnUnstakePerDelegation() public {
        uint256[2] memory one = _unstakeGas(1);
        uint256[2] memory many = _unstakeGas(101);
        uint256 partly = (many[0] - one[0]) / 100;
        uint256 fully = (many[1] - one[1]) / 100;
        console.log("unstake gas per delegation reduced:", partly);
        console.log("unstake gas per delegation emptied:", fully);
        assertLt(partly, 20_000);
        assertLt(fully, 30_000);
    }

    /// @dev Gas, before refunds, of a fresh staker's unstake of half its stake and then of the rest,
    /// with `count` delegations of one MONKEY each holding all of it.
    function _unstakeGas(uint256 count) internal returns (uint256[2] memory gas) {
        address account = makeAddr(string.concat("staker of ", vm.toString(count)));
        _stakeFor(account, count * 2e18);
        vm.startPrank(account);
        for (uint256 i = 0; i < count; i++) {
            staking.delegate(address(uint160(uint256(keccak256(abi.encode(count, i))))), 2e18);
        }
        vm.cool(address(staking));
        staking.startUnstake(count * 1e18);
        gas[0] = vm.lastCallGas().gasTotalUsed;
        vm.cool(address(staking));
        staking.startUnstake(count * 1e18);
        gas[1] = vm.lastCallGas().gasTotalUsed;
        vm.stopPrank();
        assertEq(staking.delegationsOf(account).length, 0);
    }

    /// @dev Asserts `account`'s delegations are the first `count` of `candidates` with `amounts`,
    /// in that order.
    function _assertDelegations(address account, address[2] memory candidates, uint256[2] memory amounts, uint256 count)
        internal
        view
    {
        Staking.Delegation[] memory delegations = staking.delegationsOf(account);
        assertEq(delegations.length, count);
        for (uint256 i = 0; i < count; i++) {
            assertEq(delegations[i].candidate, candidates[i]);
            assertEq(delegations[i].amount, amounts[i]);
        }
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
