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
/// @notice Delegation: a staker delegates its whole stake to one operator candidate and may change
/// or clear the choice at any time; the operator registry reads each candidate's live vote total
/// when an election epoch closes. Votes follow `stakes`: they rise at `stake` and drop at
/// `startUnstake`, though the unstaking amount keeps earning through the cooldown. Undelegated
/// stake abstains, and delegation changes nothing about distributions.
/// @notice Self-bonds: the registry locks a registered candidate's self-bond, which `startUnstake`
/// cannot take while the candidate is registered nor for one cooldown after it leaves.
/// @dev Not upgradeable. One pending unstake per account; a further `startUnstake` adds to it and
/// restarts its cooldown. The cooldown must cover at least one distribution epoch: a shorter
/// lock-up would let stake jump in just before a snapshot and leave before it prices. The deployer
/// is the registry, which alone locks and releases self-bonds: deploy it only through
/// `OperatorRegistry`, which creates it.
contract Staking is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @notice The token staked: MONKEY, minted only by the vault.
    IERC20 public immutable monkey;

    /// @notice How long an unstake waits before it completes, in seconds.
    uint256 public immutable cooldown;

    /// @notice The engine's distribution epoch length, in seconds; the cooldown must cover it.
    uint256 public immutable epoch;

    /// @notice The operator registry that deployed this contract and locks self-bonds.
    address public immutable registry;

    struct Unstake {
        uint256 amount;
        uint256 claimableAt;
    }

    /// @notice A candidate's locked self-bond; `releasesAt` is zero while the candidate is
    /// registered, then the time one cooldown after it left.
    struct Bond {
        uint256 amount;
        uint256 releasesAt;
    }

    /// @notice MONKEY base units staked per account.
    mapping(address account => uint256 amount) public stakes;

    /// @notice MONKEY base units exiting per account, and when the cooldown passes.
    mapping(address account => Unstake) public unstakes;

    /// @notice The candidate each account's whole stake votes for; zero while it abstains.
    mapping(address account => address candidate) public delegates;

    /// @notice MONKEY base units staked by the accounts that delegate to each candidate.
    mapping(address candidate => uint256 amount) public votes;

    /// @notice MONKEY base units staked across every account, unstaking amounts excluded.
    uint256 public totalStaked;

    /// @notice Each candidate's self-bond.
    mapping(address candidate => Bond) public bonds;

    /// @notice `amount` MONKEY base units staked by `account`.
    event Staked(address indexed account, uint256 amount);

    /// @notice `amount` MONKEY base units marked for unstake by `account`, claimable once the
    /// timestamp passes.
    event UnstakeStarted(address indexed account, uint256 amount, uint256 claimableAt);

    /// @notice `amount` MONKEY base units returned to `account` after the cooldown.
    event UnstakeCompleted(address indexed account, uint256 amount);

    /// @notice `account`'s whole stake now votes for `candidate`, or abstains when it is zero.
    event Delegated(address indexed account, address indexed candidate);

    /// @notice `amount` of `candidate`'s stake locked as its self-bond.
    event BondLocked(address indexed candidate, uint256 amount);

    /// @notice `candidate`'s self-bond of `amount` stays locked until `releasesAt`.
    event BondReleasing(address indexed candidate, uint256 amount, uint256 releasesAt);

    error ZeroAddress();
    error InvalidAmount(uint256 amount);
    error InsufficientStake(uint256 staked, uint256 amount);
    error NoPendingUnstake();
    error CooldownNotPassed(uint256 claimableAt);
    error CooldownBelowEpoch(uint256 cooldown, uint256 epoch);
    error NotRegistry(address caller);
    error SelfBondLocked(uint256 locked);

    /// @param monkey_ The MONKEY token the vault mints.
    /// @param cooldownSeconds_ How long an unstake waits before it completes, in seconds; the
    /// engine records the same value at `InitStaking` so the log names it. Must cover at least
    /// one `epochSeconds_`.
    /// @param epochSeconds_ The engine's distribution epoch length, in seconds.
    constructor(IERC20 monkey_, uint256 cooldownSeconds_, uint256 epochSeconds_) {
        if (address(monkey_) == address(0)) revert ZeroAddress();
        if (cooldownSeconds_ == 0) revert InvalidAmount(cooldownSeconds_);
        if (cooldownSeconds_ < epochSeconds_) revert CooldownBelowEpoch(cooldownSeconds_, epochSeconds_);
        monkey = monkey_;
        cooldown = cooldownSeconds_;
        epoch = epochSeconds_;
        registry = msg.sender;
    }

    modifier onlyRegistry() {
        if (msg.sender != registry) revert NotRegistry(msg.sender);
        _;
    }

    /// @notice Stakes `amount` MONKEY base units from the caller, adding them to its delegate's votes.
    function stake(uint256 amount) external nonReentrant {
        if (amount == 0) revert InvalidAmount(amount);
        monkey.safeTransferFrom(msg.sender, address(this), amount);
        stakes[msg.sender] += amount;
        totalStaked += amount;
        address candidate = delegates[msg.sender];
        if (candidate != address(0)) votes[candidate] += amount;
        emit Staked(msg.sender, amount);
    }

    /// @notice Starts unstaking `amount` MONKEY base units; `completeUnstake` returns them once the
    /// cooldown has passed. The amount stops voting now, but stops counting at the engine's next
    /// snapshot only once the cooldown completes and this event mirrors, so it keeps earning
    /// through the cooldown. A locked self-bond stays staked.
    function startUnstake(uint256 amount) external nonReentrant {
        uint256 staked = stakes[msg.sender];
        if (amount == 0 || amount > staked) revert InsufficientStake(staked, amount);
        uint256 locked = lockedBond(msg.sender);
        if (staked - amount < locked) revert SelfBondLocked(locked);
        stakes[msg.sender] = staked - amount;
        totalStaked -= amount;
        address candidate = delegates[msg.sender];
        if (candidate != address(0)) votes[candidate] -= amount;
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

    /// @notice Delegates the caller's whole stake, now and as it changes, to `candidate`, or clears
    /// the delegation with the zero address so the stake abstains. The next election close counts it.
    function delegate(address candidate) external {
        address previous = delegates[msg.sender];
        uint256 staked = stakes[msg.sender];
        if (previous != address(0)) votes[previous] -= staked;
        if (candidate != address(0)) votes[candidate] += staked;
        delegates[msg.sender] = candidate;
        emit Delegated(msg.sender, candidate);
    }

    /// @notice Locks `amount` of `candidate`'s stake as its self-bond. Only the registry may call it.
    function lockBond(address candidate, uint256 amount) external onlyRegistry {
        uint256 locked = lockedBond(candidate);
        if (locked != 0) revert SelfBondLocked(locked);
        uint256 staked = stakes[candidate];
        if (staked < amount) revert InsufficientStake(staked, amount);
        bonds[candidate] = Bond(amount, 0);
        emit BondLocked(candidate, amount);
    }

    /// @notice Keeps `candidate`'s self-bond locked for one cooldown from now, then frees it. Only
    /// the registry may call it, when the candidate leaves.
    function releaseBond(address candidate) external onlyRegistry {
        Bond storage bond = bonds[candidate];
        bond.releasesAt = block.timestamp + cooldown;
        emit BondReleasing(candidate, bond.amount, bond.releasesAt);
    }

    /// @notice The part of `candidate`'s stake `startUnstake` cannot take: its self-bond while it is
    /// registered and until one cooldown after it left, then nothing.
    function lockedBond(address candidate) public view returns (uint256) {
        Bond memory bond = bonds[candidate];
        return bond.releasesAt == 0 || block.timestamp < bond.releasesAt ? bond.amount : 0;
    }
}
