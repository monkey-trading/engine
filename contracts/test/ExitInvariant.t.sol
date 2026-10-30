// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Math} from "@openzeppelin-contracts/utils/math/Math.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {Vault} from "../src/Vault.sol";
import {ExitTest} from "./helpers/ExitTest.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// @notice Deposits, surpluses and checkpoints of random roots, then the exit: registrations of
/// every kind, refunds, payouts and the house remainder, in any order the fuzzer picks.
contract ExitHandler is ExitTest {
    address[3] private actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("carol")];
    address public treasury = makeAddr("treasury");
    ExitVerifier.ExitTerms private terms;
    ExitVerifier.AccountState[3] private states;
    ExitVerifier.Approval[3] private approvals;
    ExitVerifier.QueuedWithdrawal[3] private queue;
    uint256[3] private entitlements;
    ExitVerifier.PendingDeposit private pending;
    bytes private chainRef;
    bool private hasPending;

    uint256 public tokensIn;
    uint256 public tokensOut;
    uint256 public clientPaid;
    uint256 public distributionPaid;
    mapping(uint256 id => uint256 count) public refunds;
    mapping(address account => uint256 count) public payouts;
    /// @dev The distribution each account was paid by the exit.
    mapping(address account => uint256 paid) public exitDistributionPaid;

    constructor(MockUSDC token, Vault target, TimelockController timelock) {
        usdc = token;
        vault = target;
        governance = timelock;
    }

    function deposit(uint256 actorSeed, uint256 amount, bool house) external {
        if (vault.checkpointsStale() || vault.exitStartedAt() != 0) return;
        address actor = actors[actorSeed % actors.length];
        amount = bound(amount, vault.MIN_DEPOSIT(), 1_000_000e6);
        usdc.mint(actor, amount);
        vm.startPrank(actor);
        usdc.approve(address(vault), amount);
        if (house) vault.depositToHouse(amount);
        else vault.depositFor(actors[(actorSeed % actors.length + 1) % actors.length], amount);
        vm.stopPrank();
        tokensIn += amount;
    }

    function transferSurplus(uint256 amount) external {
        amount = bound(amount, 1, 1_000_000e6);
        usdc.mint(address(vault), amount);
        tokensIn += amount;
    }

    /// @dev Posts a fresh root naming any balances, approvals, queued requests and distributions,
    /// a cursor at or past the vault's deposits, up to two ids below it the root does not hold,
    /// one existing deposit pending, and held principal within what the vault holds.
    function post(uint256 seed) external {
        if (vault.exitStartedAt() != 0) return;
        delete keys;
        delete values;
        uint256 count = vault.depositCount();
        uint64 cursor = uint64(bound(seed, 0, count + 1));
        uint256 covered = cursor < count ? cursor : count;
        hasPending = covered > 0 && seed % 3 != 0;
        int128 pendingAmount;
        if (hasPending) {
            uint256 id = seed % covered;
            (address payer, uint96 amount) = vault.depositRecords(id);
            chainRef = abi.encodePacked("ref", seq, id);
            pendingAmount = int128(uint128(amount)) * UNIT;
            pending = _addPending(chainRef, uint64(id), actors[0], payer, pendingAmount);
        }
        // Ids below the cursor the root does not hold, as a reorganisation in flight would leave them.
        uint256 refundable = vault.totalRefundable() - (covered == 0 ? 0 : vault.refundableThrough(covered - 1));
        uint64[] memory gaps = new uint64[](covered > 1 ? (seed >> 16) % 3 : 0);
        uint256 pool = covered - (hasPending ? 1 : 0);
        if (gaps.length > pool) gaps = new uint64[](pool);
        uint256 taken;
        for (uint256 id; id < covered && taken < gaps.length; ++id) {
            if (hasPending && id == pending.depositId) continue;
            gaps[taken++] = uint64(id);
        }
        // The named gaps reserve their canonical vault amounts; the terms' `unheld` names none here.
        for (uint256 i; i < gaps.length; ++i) {
            uint256 id = gaps[i];
            refundable += vault.refundableThrough(id) - (id == 0 ? 0 : vault.refundableThrough(id - 1));
        }
        // Held principal is neither pending nor refundable, so it fits in what remains.
        uint256 spare = usdc.balanceOf(address(vault)) - uint256(int256(pendingAmount / UNIT)) - refundable;
        int128 held = int128(uint128(bound(seed >> 8, 0, spare))) * UNIT;
        // Attribute every refundable deposit below the cursor: the trader receipts the root holds,
        // then the named gaps, so the exit reserves no canonical remainder.
        int128 heldReceipts =
            covered == 0 ? int128(0) : int128(uint128(vault.refundableThrough(covered - 1))) * int128(UNIT);
        terms = _addTerms(cursor, 0, pendingAmount, held, 0, heldReceipts, gaps);
        for (uint256 i; i < actors.length; ++i) {
            uint256 draw = uint256(keccak256(abi.encode(seed, i)));
            states[i] = _addAccount(actors[i], int128(int256(bound(draw, 0, 200_000e6))) * UNIT - 100e6 * UNIT);
            approvals[i] = _addApproval(actors[i], bound(draw >> 64, 1, 50_000e6), seq + 1);
            queue[i] =
                _addQueued(actors[i], int128(uint128(bound(draw >> 128, 0, 50_000e6))) * UNIT, seq * 3 + uint64(i));
            // A cumulative distribution only grows.
            entitlements[i] += bound(draw >> 192, 0, 20_000e6);
            _addDistribution(actors[i], entitlements[i]);
        }
        _post();
    }

    /// @dev Pays a distribution the latest root proves while the engine runs, out of house money: what
    /// the vault holds beyond every payer-funded deposit and the root's held principal, as an
    /// engine's distributions come out of house NAV.
    function claimDistribution(uint256 actorSeed) external {
        if (vault.exitStartedAt() != 0 || !vault.hasCheckpoint()) return;
        uint256 i = actorSeed % actors.length;
        uint256 claimed = vault.distributionsClaimed(actors[i]);
        uint256 house = vault.totalRefundable() + uint256(int256(terms.held / UNIT));
        uint256 balance = usdc.balanceOf(address(vault));
        if (entitlements[i] <= claimed || balance < house || entitlements[i] - claimed > balance - house) return;
        bytes32 key = keccak256(abi.encodePacked(uint8(13), actors[i]));
        tokensOut += vault.claimDistribution(actors[i], entitlements[i], _siblings(key));
    }

    function warp(uint256 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 0, PERIOD));
    }

    function start() external {
        if (vault.exitStartedAt() != 0) return;
        vm.warp(Math.max(block.timestamp, uint256(vault.lastCheckpointAt()) + PERIOD));
        bytes32[] memory siblings =
            vault.hasCheckpoint() ? _siblings(keccak256(abi.encodePacked(uint8(24)))) : new bytes32[](0);
        vault.startExit(terms, siblings);
    }

    function register(uint256 kind, uint256 actorSeed) external {
        if (!_inWindow() || !vault.hasCheckpoint()) return;
        uint256 i = actorSeed % actors.length;
        address actor = actors[i];
        kind %= 4;
        if (kind == 0 && !vault.exitAccountRegistered(actor)) {
            (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
            vault.registerExit(actor, _withAccountProof(actor, states[i]), positions, markets);
        } else if (kind == 1 && !vault.usedNonces(actor, approvals[i].nonce)) {
            bytes32 key = keccak256(abi.encodePacked(uint8(32), actor, approvals[i].nonce));
            vault.registerExitWithdrawal(approvals[i], _siblings(key));
        } else if (kind == 2 && !vault.exitQueuedRegistered(queue[i].requestedSeq)) {
            bytes32 key = keccak256(abi.encodePacked(uint8(33), queue[i].requestedSeq));
            vault.registerExitQueued(queue[i], _siblings(key));
        } else if (kind == 3 && !vault.exitDistributionRegistered(actor)) {
            bytes32 key = keccak256(abi.encodePacked(uint8(13), actor));
            vault.registerExitDistribution(actor, entitlements[i], _siblings(key));
        }
    }

    /// @dev Refunds a deposit, or attempts it again once refunded, which the vault must refuse.
    function refund(uint256 idSeed, bool pendingRefund) external {
        if (vault.exitStartedAt() == 0 || vault.depositCount() == 0) return;
        uint256 id = idSeed % vault.depositCount();
        bytes memory call;
        if (pendingRefund) {
            if (!hasPending || !vault.hasCheckpoint()) return;
            id = pending.depositId;
            bytes32[] memory siblings = _siblings(keccak256(abi.encodePacked(uint8(34), chainRef)));
            call = abi.encodeCall(Vault.refundPendingDeposit, (chainRef, pending, siblings));
        } else {
            (address payer,) = vault.depositRecords(id);
            if ((id < vault.exitDepositCursor() && !vault.exitRefundableGap(id)) || payer == address(0)) return;
            call = abi.encodeCall(Vault.refundDeposit, (id));
        }
        bool repeat = vault.depositRefunded(id);
        (bool ok,) = address(vault).call(call);
        assertEq(ok, !repeat, "a deposit refunds exactly once");
        if (!ok) return;
        (, uint96 amount) = vault.depositRecords(id);
        refunds[id]++;
        tokensOut += amount;
    }

    function closeWindow() external {
        if (vault.exitStartedAt() == 0) return;
        vm.warp(Math.max(block.timestamp, uint256(vault.exitStartedAt()) + WINDOW));
    }

    function claim(uint256 actorSeed) external {
        address actor = actors[actorSeed % actors.length];
        if (!_closed() || (vault.exitClaims(actor) == 0 && vault.exitDistributionClaims(actor) == 0)) return;
        uint256 claimShare = vault.exitClaims(actor);
        uint256 distributionShare = vault.exitDistributionClaims(actor);
        uint256 amount = vault.claimExit(actor);
        if (claimShare != 0) clientPaid += Math.mulDiv(claimShare, vault.exitClientPool(), vault.totalExitClaims());
        if (distributionShare != 0) {
            uint256 share =
                Math.mulDiv(distributionShare, vault.exitDistributionPool(), vault.totalExitDistributionClaims());
            distributionPaid += share;
            exitDistributionPaid[actor] += share;
        }
        payouts[actor]++;
        tokensOut += amount;
    }

    function release() external {
        if (!_closed()) return;
        uint256 before = usdc.balanceOf(treasury);
        vm.prank(address(governance));
        (bool ok,) = address(vault).call(abi.encodeCall(Vault.releaseHouseRemainder, (treasury)));
        // A release reverts only once nothing is left to release.
        if (!ok) assertEq(vault.houseRemainder(), 0);
        tokensOut += usdc.balanceOf(treasury) - before;
    }

    function actor(uint256 i) external view returns (address) {
        return actors[i];
    }

    /// @dev Actor `i`'s cumulative distribution in the latest posted root, which is the exit root once
    /// the exit has started.
    function entitlement(uint256 i) external view returns (uint256) {
        return entitlements[i];
    }

    function _inWindow() private view returns (bool) {
        uint256 started = vault.exitStartedAt();
        return started != 0 && block.timestamp < started + WINDOW;
    }

    function _closed() private view returns (bool) {
        uint256 started = vault.exitStartedAt();
        return started != 0 && block.timestamp >= started + WINDOW;
    }
}

/// Whatever the roots say, the exit pays only what the vault holds beyond its refunds and held
/// principal, each claim and refund at most once, and the pools never more than they were fixed at.
contract ExitInvariantTest is ExitTest {
    ExitHandler private handler;

    function setUp() public override {
        super.setUp();
        handler = new ExitHandler(usdc, vault, governance);
        bytes4[] memory actions = new bytes4[](11);
        actions[0] = ExitHandler.deposit.selector;
        actions[1] = ExitHandler.transferSurplus.selector;
        actions[2] = ExitHandler.post.selector;
        actions[3] = ExitHandler.warp.selector;
        actions[4] = ExitHandler.start.selector;
        actions[5] = ExitHandler.register.selector;
        actions[6] = ExitHandler.refund.selector;
        actions[7] = ExitHandler.closeWindow.selector;
        actions[8] = ExitHandler.claim.selector;
        actions[9] = ExitHandler.release.selector;
        actions[10] = ExitHandler.claimDistribution.selector;
        targetContract(address(handler));
        targetSelector(FuzzSelector({addr: address(handler), selectors: actions}));
    }

    function invariant_theVaultHoldsEveryOutstandingObligation() public view {
        assertEq(usdc.balanceOf(address(vault)), handler.tokensIn() - handler.tokensOut(), "only the exit moved tokens");
        if (vault.exitStartedAt() == 0) return;
        uint256 owed = vault.exitRefundReserve() + vault.exitHeldReserve();
        if (vault.exitSettled()) {
            owed += vault.houseRemainder();
            for (uint256 i; i < 3; ++i) {
                address account = handler.actor(i);
                if (vault.exitClaims(account) != 0) {
                    owed += Math.mulDiv(vault.exitClaims(account), vault.exitClientPool(), vault.totalExitClaims());
                }
                if (vault.exitDistributionClaims(account) != 0) {
                    owed += Math.mulDiv(
                        vault.exitDistributionClaims(account),
                        vault.exitDistributionPool(),
                        vault.totalExitDistributionClaims()
                    );
                }
            }
            assertLe(vault.exitClientPool(), vault.totalExitClaims());
            assertLe(vault.exitDistributionPool(), vault.totalExitDistributionClaims());
        }
        assertGe(usdc.balanceOf(address(vault)), owed);
    }

    function invariant_poolsAreNeverOverpaid() public view {
        assertLe(handler.clientPaid(), vault.exitClientPool());
        assertLe(handler.distributionPaid(), vault.exitDistributionPool());
    }

    function invariant_noDistributionIsPaidPastItsEntitlement() public view {
        if (vault.exitStartedAt() == 0) return;
        for (uint256 i; i < 3; ++i) {
            address account = handler.actor(i);
            assertLe(
                vault.distributionsClaimed(account) + handler.exitDistributionPaid(account), handler.entitlement(i)
            );
        }
    }

    function invariant_eachDepositRefundsAndEachAccountIsPaidAtMostOnce() public view {
        for (uint256 id; id < vault.depositCount(); ++id) {
            assertLe(handler.refunds(id), 1);
        }
        for (uint256 i; i < 3; ++i) {
            assertLe(handler.payouts(handler.actor(i)), 1);
        }
    }
}
