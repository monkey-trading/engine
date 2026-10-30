// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test, Vm} from "forge-std/Test.sol";
import {GovernanceTest} from "./helpers/GovernanceTest.sol";

import {Vault} from "../src/Vault.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// @notice Random deposits from a few actors through every entry point, tracking what the events
/// report and checking each trader deposit names the account it was made for.
contract VaultHandler is Test {
    MockUSDC internal usdc;
    Vault internal vault;
    address[3] internal actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("house funder")];

    uint256 public depositsMade;
    uint256 public amountReported;

    constructor(MockUSDC usdc_, Vault vault_) {
        usdc = usdc_;
        vault = vault_;
    }

    function deposit(uint256 actorSeed, uint256 accountSeed, uint256 amount, uint8 entry) external {
        address actor = actors[actorSeed % actors.length];
        address account = actors[accountSeed % actors.length];
        amount = bound(amount, vault.MIN_DEPOSIT(), vault.MAX_DEPOSIT());
        entry = entry % 3;
        usdc.mint(actor, amount);
        vm.startPrank(actor);
        usdc.approve(address(vault), amount);
        vm.recordLogs();
        uint256 id;
        if (entry == 0) id = vault.deposit(amount);
        else if (entry == 1) id = vault.depositFor(account, amount);
        else id = vault.depositToHouse(amount);
        vm.stopPrank();

        assertEq(id, depositsMade);
        depositsMade++;
        // The vault emits one event per deposit; its only data word is the amount credited, and
        // its second topic the trader account, or the funder for the house.
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].emitter != address(vault)) continue;
            amountReported += abi.decode(logs[i].data, (uint256));
            address named = address(uint160(uint256(logs[i].topics[2])));
            assertEq(named, entry == 1 ? account : actor);
        }
    }
}

contract VaultInvariantTest is GovernanceTest {
    MockUSDC internal usdc;
    Vault internal vault;
    VaultHandler internal handler;

    function setUp() public {
        governance = _deployGovernance();
        usdc = new MockUSDC();
        vault = new Vault(usdc, address(0x1234), governance, multisig, 1e18, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER);

        handler = new VaultHandler(usdc, vault);
        targetContract(address(handler));
    }

    /// @notice Every deposit event is backed by USDC the vault holds, and none is missing.
    function invariant_VaultHoldsExactlyTheReportedDeposits() public view {
        assertEq(usdc.balanceOf(address(vault)), handler.amountReported());
    }

    /// @notice The deposit counter matches the deposits made, so ids have no gaps.
    function invariant_DepositCountMatchesDepositsMade() public view {
        assertEq(vault.depositCount(), handler.depositsMade());
    }

    /// @notice The running deposit total is the reported deposits, through the latest deposit too.
    function invariant_DepositTotalsMatchTheReportedDeposits() public view {
        assertEq(vault.totalDeposited(), handler.amountReported());
        if (handler.depositsMade() > 0) {
            assertEq(vault.depositedThrough(handler.depositsMade() - 1), vault.totalDeposited());
        }
    }
}
