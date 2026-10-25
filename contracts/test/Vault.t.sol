// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {IERC20Errors} from "@openzeppelin-contracts/interfaces/draft-IERC6093.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";

import {Vault} from "../src/Vault.sol";
import {FeeOnTransferToken} from "./mocks/FeeOnTransferToken.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";
import {MockWallet} from "./mocks/MockWallet.sol";
import {ReentrantToken} from "./mocks/ReentrantToken.sol";

contract VaultTest is GovernanceTest {
    uint256 internal constant ONE_USDC = 1e6;

    MockUSDC internal usdc;
    Vault internal vault;
    uint256 internal min;
    address internal trader = makeAddr("trader");
    address internal funder = makeAddr("funder");

    function setUp() public {
        governance = _deployGovernance();
        usdc = new MockUSDC();
        vault = new Vault(usdc, address(0x1234), governance, 1e18);
        min = vault.MIN_DEPOSIT();
    }

    function test_DepositMovesUsdcAndEmitsTheCallersAccount() public {
        _fund(trader, 250 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.Deposited(0, trader, 250 * ONE_USDC);
        vm.prank(trader);
        uint256 id = vault.deposit(250 * ONE_USDC);

        assertEq(id, 0);
        assertEq(usdc.balanceOf(address(vault)), 250 * ONE_USDC);
        assertEq(usdc.balanceOf(trader), 0);
        assertEq(vault.depositCount(), 1);
    }

    function test_DepositForPullsFromTheCallerAndEmitsTheNamedAccount() public {
        _fund(funder, 250 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.Deposited(0, trader, 250 * ONE_USDC);
        vm.prank(funder);
        uint256 id = vault.depositFor(trader, 250 * ONE_USDC);

        assertEq(id, 0);
        assertEq(usdc.balanceOf(address(vault)), 250 * ONE_USDC);
        assertEq(usdc.balanceOf(funder), 0);
        assertEq(usdc.balanceOf(trader), 0);
    }

    function test_AContractWalletFundsTheKeyThatSignsForIt() public {
        MockWallet wallet = new MockWallet(trader);
        usdc.mint(address(wallet), 100 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.Deposited(0, trader, 100 * ONE_USDC);
        uint256 id = wallet.fund(vault, 100 * ONE_USDC);

        assertEq(id, 0);
        assertEq(usdc.balanceOf(address(wallet)), 0);
        assertEq(usdc.balanceOf(address(vault)), 100 * ONE_USDC);
    }

    function test_RevertWhen_DepositForNamesTheZeroAddress() public {
        _fund(funder, min);
        vm.expectRevert(Vault.ZeroAddress.selector);
        vm.prank(funder);
        vault.depositFor(address(0), min);
        assertEq(vault.depositCount(), 0);
    }

    function test_DepositToHouseMovesUsdcAndEmitsTheFunder() public {
        _fund(funder, 5_000_000 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.HouseDeposited(0, funder, 5_000_000 * ONE_USDC);
        vm.prank(funder);
        uint256 id = vault.depositToHouse(5_000_000 * ONE_USDC);

        assertEq(id, 0);
        assertEq(usdc.balanceOf(address(vault)), 5_000_000 * ONE_USDC);
        assertEq(vault.depositCount(), 1);
    }

    function test_IdsRunInOneSequenceAcrossEveryEntryPoint() public {
        _fund(trader, min);
        _fund(funder, 2 * min);

        vm.prank(trader);
        assertEq(vault.deposit(min), 0);
        vm.expectEmit(address(vault));
        emit Vault.HouseDeposited(1, funder, min);
        vm.prank(funder);
        assertEq(vault.depositToHouse(min), 1);
        vm.expectEmit(address(vault));
        emit Vault.Deposited(2, trader, min);
        vm.prank(funder);
        assertEq(vault.depositFor(trader, min), 2);

        assertEq(vault.depositCount(), 3);
    }

    function test_DepositTotalsRunAcrossEveryEntryPoint() public {
        _fund(trader, min);
        _fund(funder, 5 * min);

        vm.prank(trader);
        vault.deposit(min);
        vm.prank(funder);
        vault.depositToHouse(2 * min);
        vm.prank(funder);
        vault.depositFor(trader, 3 * min);

        assertEq(vault.totalDeposited(), 6 * min);
        assertEq(vault.depositedThrough(0), min);
        assertEq(vault.depositedThrough(1), 3 * min);
        assertEq(vault.depositedThrough(2), 6 * min);
        assertEq(vault.depositedThrough(3), 0);
    }

    function test_BookSurplusRecordsADirectTransferAsAHouseDeposit() public {
        _fund(trader, min);
        vm.prank(trader);
        vault.deposit(min);
        usdc.mint(address(vault), 15 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.HouseDeposited(1, funder, 15 * ONE_USDC);
        vm.prank(funder);
        assertEq(vault.bookSurplus(), 1);

        assertEq(vault.depositCount(), 2);
        assertEq(vault.totalDeposited(), min + 15 * ONE_USDC);
        assertEq(vault.depositedThrough(1), min + 15 * ONE_USDC);
        assertEq(usdc.balanceOf(address(vault)), vault.totalDeposited());
        vm.expectRevert(abi.encodeWithSelector(Vault.NoSurplus.selector, 0));
        vault.bookSurplus();
    }

    function test_RevertWhen_SurplusIsBelowMinDeposit() public {
        vm.expectRevert(abi.encodeWithSelector(Vault.NoSurplus.selector, 0));
        vault.bookSurplus();
        usdc.mint(address(vault), min - 1);
        vm.expectRevert(abi.encodeWithSelector(Vault.NoSurplus.selector, min - 1));
        vault.bookSurplus();
        assertEq(vault.depositCount(), 0);

        usdc.mint(address(vault), 1);
        vm.expectEmit(address(vault));
        emit Vault.HouseDeposited(0, address(this), min);
        vault.bookSurplus();
        assertEq(vault.totalDeposited(), min);
    }

    function test_BookSurplusTakesAtMostMaxDepositPerCall() public {
        uint256 max = vault.MAX_DEPOSIT();
        usdc.mint(address(vault), max + min);

        vault.bookSurplus();
        assertEq(vault.depositedThrough(0), max);
        vault.bookSurplus();
        assertEq(vault.depositedThrough(1), max + min);
        assertEq(vault.totalDeposited(), usdc.balanceOf(address(vault)));
    }

    function test_RevertWhen_BookSurplusWhileDepositsArePaused() public {
        usdc.mint(address(vault), min);
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (true, false)));

        vm.expectRevert(Vault.DepositsPaused.selector);
        vault.bookSurplus();
        assertEq(vault.depositCount(), 0);
    }

    function test_TheMinimumIsTenUsdc() public view {
        assertEq(vault.MIN_DEPOSIT(), 10 * ONE_USDC);
    }

    function test_SmallestAndLargestDepositsAreAccepted() public {
        uint256 max = vault.MAX_DEPOSIT();
        _fund(trader, max + min);

        vm.startPrank(trader);
        vault.deposit(min);
        vault.deposit(max);
        vm.stopPrank();

        assertEq(usdc.balanceOf(address(vault)), max + min);
    }

    function test_RevertWhen_AmountIsBelowMinDeposit() public {
        _fund(trader, min);
        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, min - 1));
        vm.prank(trader);
        vault.deposit(min - 1);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, min - 1));
        vm.prank(trader);
        vault.depositFor(funder, min - 1);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, 0));
        vm.prank(funder);
        vault.depositToHouse(0);
    }

    function test_RevertWhen_AmountExceedsMaxDeposit() public {
        uint256 over = vault.MAX_DEPOSIT() + 1;
        _fund(trader, over);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, over));
        vm.prank(trader);
        vault.deposit(over);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, over));
        vm.prank(trader);
        vault.depositFor(funder, over);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, over));
        vm.prank(trader);
        vault.depositToHouse(over);
    }

    function test_RevertWhen_AllowanceIsShort() public {
        usdc.mint(trader, 20 * ONE_USDC);
        vm.prank(trader);
        usdc.approve(address(vault), 15 * ONE_USDC);

        vm.expectRevert(
            abi.encodeWithSelector(
                IERC20Errors.ERC20InsufficientAllowance.selector, address(vault), 15 * ONE_USDC, 20 * ONE_USDC
            )
        );
        vm.prank(trader);
        vault.deposit(20 * ONE_USDC);
        assertEq(vault.depositCount(), 0);
    }

    function test_RevertWhen_TokenIsZeroAddress() public {
        vm.expectRevert(Vault.ZeroAddress.selector);
        new Vault(IERC20(address(0)), address(0x1234), governance, 1e18);
    }

    function test_FeeOnTransferTokenCreditsOnlyWhatArrived() public {
        FeeOnTransferToken token = new FeeOnTransferToken();
        Vault feeVault = new Vault(token, address(0x1234), governance, 1e18);
        token.mint(trader, 100 * ONE_USDC);
        vm.prank(trader);
        token.approve(address(feeVault), 100 * ONE_USDC);

        vm.expectEmit(address(feeVault));
        emit Vault.Deposited(0, trader, 99 * ONE_USDC);
        vm.prank(trader);
        feeVault.deposit(100 * ONE_USDC);

        assertEq(token.balanceOf(address(feeVault)), 99 * ONE_USDC);
        assertEq(feeVault.totalDeposited(), 99 * ONE_USDC);
        assertEq(feeVault.depositedThrough(0), 99 * ONE_USDC);
    }

    function test_RevertWhen_TokenReentersAnyEntryPoint() public {
        ReentrantToken.Entry[3] memory entries =
            [ReentrantToken.Entry.Deposit, ReentrantToken.Entry.DepositFor, ReentrantToken.Entry.DepositToHouse];
        for (uint256 i = 0; i < entries.length; i++) {
            ReentrantToken token = new ReentrantToken();
            Vault target = new Vault(token, address(0x1234), governance, 1e18);
            token.mint(trader, min);
            vm.prank(trader);
            token.approve(address(target), min);
            token.arm(target, entries[i]);

            vm.expectRevert(ReentrancyGuardTransient.ReentrancyGuardReentrantCall.selector);
            vm.prank(trader);
            target.deposit(min);
        }
    }

    function testFuzz_DepositCreditsExactlyWhatWasPulled(uint256 amount, uint8 entry, address account) public {
        amount = bound(amount, min, vault.MAX_DEPOSIT());
        vm.assume(account != address(0));
        entry = entry % 3;
        _fund(funder, amount);

        vm.expectEmit(address(vault));
        if (entry == 0) emit Vault.Deposited(0, funder, amount);
        else if (entry == 1) emit Vault.Deposited(0, account, amount);
        else emit Vault.HouseDeposited(0, funder, amount);
        vm.prank(funder);
        if (entry == 0) vault.deposit(amount);
        else if (entry == 1) vault.depositFor(account, amount);
        else vault.depositToHouse(amount);

        assertEq(usdc.balanceOf(address(vault)), amount);
        assertEq(usdc.balanceOf(funder), 0);
        assertEq(vault.depositCount(), 1);
    }

    function testFuzz_RevertWhen_AmountIsBelowMinDeposit(uint256 amount) public {
        amount = bound(amount, 0, min - 1);
        _fund(trader, amount);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, amount));
        vm.prank(trader);
        vault.deposit(amount);
    }

    function testFuzz_RevertWhen_AmountIsAboveMaxDeposit(uint256 amount) public {
        amount = bound(amount, vault.MAX_DEPOSIT() + 1, type(uint256).max);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, amount));
        vm.prank(trader);
        vault.deposit(amount);
    }

    // ── Helpers ──

    /// @dev Mints `amount` to `who` and approves the vault for it.
    function _fund(address who, uint256 amount) internal {
        usdc.mint(who, amount);
        vm.prank(who);
        usdc.approve(address(vault), amount);
    }
}
