// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";

/// @title Staking
/// @notice MONKEY staking for USDC distributions. Holders stake the MONKEY the vault minted them;
/// each epoch the engine distributes house NAV above its liquidity floor to stakers pro-rata from
/// the staked snapshot, paid in USDC by the vault against a checkpoint proof. This contract is
/// custody only: it holds MONKEY, enforces the unstake cooldown, and emits the events the engine
/// mirrors. `UnstakeCompleted` fires only once the cooldown has passed, and the engine keeps the
/// stake through the cooldown and the snapshot, so exiting stake keeps earning until its mirror
/// lands: the cooldown is the lock-up that prices jumping in just before a snapshot.
/// @dev Not upgradeable. One pending unstake per account; a further `startUnstake` adds to it and
/// restarts its cooldown.
contract Staking is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @notice The token staked: MONKEY, minted only by the vault.
    IERC20 public immutable monkey;

    /// @notice How long an unstake waits before it completes, in seconds.
    uint256 public immutable cooldown;

    struct Unstake {
        uint256 amount;
        uint256 claimableAt;
    }

    /// @notice MONKEY base units staked per account.
    mapping(address account => uint256 amount) public stakes;

    /// @notice MONKEY base units exiting per account, and when the cooldown passes.
    mapping(address account => Unstake) public unstakes;

    /// @notice `amount` MONKEY base units staked by `account`.
    event Staked(address indexed account, uint256 amount);

    /// @notice `amount` MONKEY base units marked for unstake by `account`, claimable once the
    /// timestamp passes.
    event UnstakeStarted(address indexed account, uint256 amount, uint256 claimableAt);

    /// @notice `amount` MONKEY base units returned to `account` after the cooldown.
    event UnstakeCompleted(address indexed account, uint256 amount);

    error ZeroAddress();
    error InvalidAmount(uint256 amount);
    error InsufficientStake(uint256 staked, uint256 amount);
    error NoPendingUnstake();
    error CooldownNotPassed(uint256 claimableAt);

    /// @param monkey_ The MONKEY token the vault mints.
    /// @param cooldownSeconds_ How long an unstake waits before it completes, in seconds; the
    /// engine records the same value at `InitStaking` so the log names it.
    constructor(IERC20 monkey_, uint256 cooldownSeconds_) {
        if (address(monkey_) == address(0)) revert ZeroAddress();
        if (cooldownSeconds_ == 0) revert InvalidAmount(cooldownSeconds_);
        monkey = monkey_;
        cooldown = cooldownSeconds_;
    }

    /// @notice Stakes `amount` MONKEY base units from the caller.
    function stake(uint256 amount) external nonReentrant {
        if (amount == 0) revert InvalidAmount(amount);
        monkey.safeTransferFrom(msg.sender, address(this), amount);
        stakes[msg.sender] += amount;
        emit Staked(msg.sender, amount);
    }

    /// @notice Starts unstaking `amount` MONKEY base units; `completeUnstake` returns them once the
    /// cooldown has passed. The stake stops counting at the engine's next snapshot only once the
    /// cooldown completes and this event mirrors, so it keeps earning through the cooldown.
    function startUnstake(uint256 amount) external nonReentrant {
        uint256 staked = stakes[msg.sender];
        if (amount == 0 || amount > staked) revert InsufficientStake(staked, amount);
        stakes[msg.sender] = staked - amount;
        Unstake storage pending = unstakes[msg.sender];
        pending.amount += amount;
        pending.claimableAt = block.timestamp + cooldown;
        emit UnstakeStarted(msg.sender, amount, pending.claimableAt);
    }

    /// @notice Returns the caller's unstaked MONKEY once the cooldown has passed.
    function completeUnstake() external nonReentrant {
        Unstake storage pending = unstakes[msg.sender];
        if (pending.amount == 0) revert NoPendingUnstake();
        if (block.timestamp < pending.claimableAt) revert CooldownNotPassed(pending.claimableAt);
        uint256 amount = pending.amount;
        delete unstakes[msg.sender];
        emit UnstakeCompleted(msg.sender, amount);
        monkey.safeTransfer(msg.sender, amount);
    }
}
