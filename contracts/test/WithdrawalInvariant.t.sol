// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vault} from "../src/Vault.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";
import {WithdrawalTest} from "./helpers/WithdrawalTest.sol";

contract WithdrawalHandler is WithdrawalTest {
    struct Claim {
        Vault.Withdrawal withdrawal;
        uint64 checkpointSeq;
        uint256 key;
        bool paid;
    }

    address[3] private actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("carol")];
    mapping(address => uint64) private nextNonce;
    Claim[] private claims;
    uint256 private currentKey = KEY;
    uint256 public deposited;
    uint256 public booked;
    uint256 public paid;
    uint64 public rotations;

    constructor(MockUSDC token, Vault target, TimelockController timelock) {
        usdc = token;
        vault = target;
        governance = timelock;
    }

    function deposit(uint256 actorSeed, uint256 amount, bool house) external {
        if (vault.depositsPaused()) return;
        address actor = actors[actorSeed % actors.length];
        amount = bound(amount, vault.MIN_DEPOSIT(), vault.MAX_DEPOSIT());
        usdc.mint(actor, amount);
        vm.startPrank(actor);
        usdc.approve(address(vault), amount);
        if (house) vault.depositToHouse(amount);
        else vault.deposit(amount);
        vm.stopPrank();
        deposited += amount;
    }

    /// @dev Books the surplus, which starts as the vault's opening balance.
    function book(uint256 actorSeed) external {
        address actor = actors[actorSeed % actors.length];
        uint256 held = usdc.balanceOf(address(vault)) + vault.totalWithdrawn();
        uint256 surplus = held - vault.totalDeposited();
        uint256 count = vault.depositCount();
        vm.prank(actor);
        (bool ok,) = address(vault).call(abi.encodeCall(Vault.bookSurplus, ()));
        assertEq(ok, !vault.depositsPaused() && surplus >= vault.MIN_DEPOSIT());
        if (!ok) return;
        uint256 amount = surplus < vault.MAX_DEPOSIT() ? surplus : vault.MAX_DEPOSIT();
        booked += amount;
        assertEq(vault.depositedThrough(count), vault.totalDeposited());
        assertEq(vault.totalDeposited() + surplus, held + amount);
    }

    function approve(uint256 actorSeed, uint256 amount) external {
        address actor = actors[actorSeed % actors.length];
        amount = bound(amount, 1, 1_000_000e6);
        uint64 seq = vault.hasCheckpoint() ? vault.latestSeq() + 1 : 10;
        Vault.Withdrawal memory withdrawal = Vault.Withdrawal(
            actor,
            amount,
            nextNonce[actor]++,
            seq,
            vault.signerEpoch(),
            uint64(block.timestamp),
            uint64(block.timestamp + 1 days)
        );
        _post(vault, seq, _root(withdrawal, new bytes32[](0)), currentKey);
        claims.push(Claim(withdrawal, seq, currentKey, false));
    }

    function claim(uint256 seed) external {
        if (claims.length == 0) return;
        Claim storage item = claims[seed % claims.length];
        Vault.Withdrawal memory withdrawal = item.withdrawal;
        uint256 before = usdc.balanceOf(address(vault));
        uint256 accountBefore = usdc.balanceOf(withdrawal.account);
        bool expected = !item.paid && !vault.withdrawalsPaused() && withdrawal.signerEpoch == vault.signerEpoch()
            && withdrawal.issuedAt <= block.timestamp && withdrawal.expiresAt > block.timestamp
            && withdrawal.amount <= before;
        bytes memory signature = _sign(vault, withdrawal, item.key);
        (bool ok,) = address(vault)
            .call(abi.encodeCall(Vault.withdraw, (withdrawal, item.checkpointSeq, new bytes32[](0), signature)));
        assertEq(ok, expected);
        if (ok) {
            item.paid = true;
            paid += withdrawal.amount;
            assertEq(usdc.balanceOf(address(vault)), before - withdrawal.amount);
            assertEq(usdc.balanceOf(withdrawal.account), accountBefore + withdrawal.amount);
        } else {
            assertEq(usdc.balanceOf(address(vault)), before);
            assertEq(usdc.balanceOf(withdrawal.account), accountBefore);
        }
    }

    function reissue(uint256 seed) external {
        if (claims.length == 0) return;
        Claim storage item = claims[seed % claims.length];
        item.withdrawal.signerEpoch = vault.signerEpoch();
        item.withdrawal.issuedAt = uint64(block.timestamp);
        item.withdrawal.expiresAt = uint64(block.timestamp + 1 days);
        item.key = currentKey;
    }

    function rotate() external {
        ++currentKey;
        ++rotations;
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(currentKey))));
    }

    function pauses(bool deposits, bool withdrawals) external {
        _govern(address(vault), abi.encodeCall(Vault.setPauses, (deposits, withdrawals)));
    }

    function elapse(uint256 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 0, 2 days));
    }

    function assertClaimsAndAcceptedRoots() external view {
        uint256 paidSum;
        for (uint256 i; i < claims.length; ++i) {
            Claim storage item = claims[i];
            assertEq(vault.usedNonces(item.withdrawal.account, item.withdrawal.nonce), item.paid);
            assertTrue(vault.hasCheckpointAt(item.checkpointSeq));
            assertEq(vault.checkpointRoots(item.checkpointSeq), _root(item.withdrawal, new bytes32[](0)));
            if (item.paid) paidSum += item.withdrawal.amount;
        }
        assertEq(paidSum, paid);
        uint256 received;
        for (uint256 i; i < actors.length; ++i) {
            received += usdc.balanceOf(actors[i]);
        }
        assertEq(received, paid);
    }
}

contract WithdrawalInvariantTest is WithdrawalTest {
    WithdrawalHandler private handler;
    uint256 private initialBalance;

    function setUp() public override {
        super.setUp();
        initialBalance = usdc.balanceOf(address(vault));
        handler = new WithdrawalHandler(usdc, vault, governance);
        bytes4[] memory selectors = new bytes4[](8);
        selectors[0] = WithdrawalHandler.deposit.selector;
        selectors[1] = WithdrawalHandler.approve.selector;
        selectors[2] = WithdrawalHandler.claim.selector;
        selectors[3] = WithdrawalHandler.reissue.selector;
        selectors[4] = WithdrawalHandler.rotate.selector;
        selectors[5] = WithdrawalHandler.pauses.selector;
        selectors[6] = WithdrawalHandler.elapse.selector;
        selectors[7] = WithdrawalHandler.book.selector;
        targetContract(address(handler));
        targetSelector(FuzzSelector({addr: address(handler), selectors: selectors}));
    }

    function invariant_TokensAreConservedAndEachApprovalPaysAtMostOnceAcrossEpochs() public view {
        assertEq(usdc.balanceOf(address(vault)) + handler.paid(), initialBalance + handler.deposited());
        assertEq(vault.totalWithdrawn(), handler.paid());
        // Only the opening balance arrived outside a deposit, and booking moves it into the totals.
        assertEq(vault.totalDeposited(), handler.deposited() + handler.booked());
        assertEq(
            usdc.balanceOf(address(vault)) + vault.totalWithdrawn(),
            vault.totalDeposited() + initialBalance - handler.booked()
        );
        assertEq(vault.signerEpoch(), uint256(handler.rotations()) + 1);
        handler.assertClaimsAndAcceptedRoots();
    }
}
