// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {Math} from "@openzeppelin-contracts/utils/math/Math.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import {EnumerableSet} from "@openzeppelin-contracts/utils/structs/EnumerableSet.sol";

/// @title Staking
/// @notice MONKEY staking for USDC distributions. Holders stake the MONKEY the vault minted them;
/// each epoch the engine distributes house NAV above its liquidity floor to stakers pro-rata from
/// the staked snapshot, paid in USDC by the vault against a checkpoint proof. This contract is
/// custody only: it holds MONKEY, enforces the unstake cooldown, and emits the events the engine
/// mirrors. `UnstakeCompleted` fires only once the cooldown has passed, and the engine keeps the
/// stake through the cooldown and the snapshot, so exiting stake keeps earning until its mirror
/// lands: the cooldown is the lock-up that prices jumping in just before a snapshot.
/// @notice Delegation: a staker delegates chosen amounts of its undelegated stake to any number of
/// operator candidates, moves amounts between them, and takes amounts back; the operator registry
/// reads each candidate's live vote total when an election epoch closes. New stake is undelegated,
/// and undelegated stake abstains. `startUnstake` takes undelegated stake first, then reduces the
/// staker's delegations in proportion, so votes drop at `startUnstake`, though the unstaking amount
/// keeps earning through the cooldown. Delegation changes nothing about distributions.
/// @notice Self-bonds: the registry locks a registered candidate's self-bond, which `startUnstake`
/// cannot take while the candidate is registered nor for one cooldown after it leaves.
/// @dev Not upgradeable. One pending unstake per account; a further `startUnstake` adds to it and
/// restarts its cooldown. The cooldown must cover at least one distribution epoch: a shorter
/// lock-up would let stake jump in just before a snapshot and leave before it prices. The deployer
/// is the registry, which alone locks and releases self-bonds: deploy it only through
/// `OperatorRegistry`, which creates it. A staker may hold any number of delegations: only an
/// unstake larger than its undelegated stake reads them, and `delegate`, `undelegate` and
/// `redelegate` each touch one or two, so a staker can always shrink its list first.
contract Staking is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;
    using EnumerableSet for EnumerableSet.AddressSet;

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

    /// @notice `amount` MONKEY base units of a staker's stake voting for `candidate`.
    struct Delegation {
        address candidate;
        uint256 amount;
    }

    /// @notice MONKEY base units staked per account.
    mapping(address account => uint256 amount) public stakes;

    /// @notice MONKEY base units exiting per account, and when the cooldown passes.
    mapping(address account => Unstake) public unstakes;

    /// @notice MONKEY base units each account delegates to each candidate.
    mapping(address account => mapping(address candidate => uint256 amount)) public delegatedTo;

    /// @notice The part of each account's stake its delegations hold; the rest is undelegated and
    /// abstains.
    mapping(address account => uint256 amount) public delegatedStake;

    /// @notice MONKEY base units delegated to each candidate, across every account.
    mapping(address candidate => uint256 amount) public votes;

    /// @dev The candidates each account delegates a nonzero amount to.
    mapping(address account => EnumerableSet.AddressSet) private delegatedCandidates;

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

    /// @notice `account` delegated `amount` of its undelegated stake to `candidate`.
    event Delegated(address indexed account, address indexed candidate, uint256 amount);

    /// @notice `amount` left `account`'s delegation to `candidate`: back to its undelegated stake at
    /// `undelegate`, or out of its stake at `startUnstake`, which emits this for each delegation it
    /// reduces before its `UnstakeStarted`.
    event Undelegated(address indexed account, address indexed candidate, uint256 amount);

    /// @notice `account` moved `amount` of its delegation to `from` to its delegation to `to`.
    event Redelegated(address indexed account, address indexed from, address indexed to, uint256 amount);

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
    error InsufficientUndelegated(uint256 undelegated, uint256 amount);
    error InsufficientDelegation(uint256 delegated, uint256 amount);
    error SameCandidate(address candidate);

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

    /// @notice Stakes `amount` MONKEY base units from the caller. New stake is undelegated: it abstains
    /// until `delegate` gives it a candidate.
    function stake(uint256 amount) external nonReentrant {
        if (amount == 0) revert InvalidAmount(amount);
        monkey.safeTransferFrom(msg.sender, address(this), amount);
        stakes[msg.sender] += amount;
        totalStaked += amount;
        emit Staked(msg.sender, amount);
    }

    /// @notice Starts unstaking `amount` MONKEY base units; `completeUnstake` returns them once the
    /// cooldown has passed. The amount comes from undelegated stake first; the rest comes from the
    /// caller's delegations in proportion, each reduction emitting `Undelegated`. The amount stops
    /// voting now, but stops counting at the engine's next snapshot only once the cooldown completes
    /// and this event mirrors, so it keeps earning through the cooldown. A locked self-bond stays
    /// staked.
    /// @dev An unstake no larger than the undelegated stake reads no delegation. A larger one costs
    /// gas for each delegation (the forge gas test reports it), so a staker with many delegations
    /// can undelegate first to keep the unstake within the block gas limit.
    function startUnstake(uint256 amount) external nonReentrant {
        uint256 staked = stakes[msg.sender];
        if (amount == 0 || amount > staked) revert InsufficientStake(staked, amount);
        uint256 locked = lockedBond(msg.sender);
        if (staked - amount < locked) revert SelfBondLocked(locked);
        uint256 delegated = delegatedStake[msg.sender];
        uint256 undelegated = staked - delegated;
        if (amount > undelegated) {
            uint256 fromDelegations = amount - undelegated;
            delegatedStake[msg.sender] = delegated - fromDelegations;
            _reduceDelegations(msg.sender, delegated, fromDelegations);
        }
        stakes[msg.sender] = staked - amount;
        totalStaked -= amount;
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

    /// @notice Delegates `amount` of the caller's undelegated stake to `candidate`, adding to any
    /// delegation it already holds there. The next election close counts it.
    function delegate(address candidate, uint256 amount) external {
        if (candidate == address(0)) revert ZeroAddress();
        if (amount == 0) revert InvalidAmount(amount);
        uint256 delegated = delegatedStake[msg.sender];
        uint256 undelegated = stakes[msg.sender] - delegated;
        if (amount > undelegated) revert InsufficientUndelegated(undelegated, amount);
        delegatedStake[msg.sender] = delegated + amount;
        _add(msg.sender, candidate, amount);
        emit Delegated(msg.sender, candidate, amount);
    }

    /// @notice Returns `amount` of the caller's delegation to `candidate` to its undelegated stake.
    function undelegate(address candidate, uint256 amount) external {
        uint256 delegation = delegatedTo[msg.sender][candidate];
        if (amount == 0 || amount > delegation) revert InsufficientDelegation(delegation, amount);
        delegatedStake[msg.sender] -= amount;
        _remove(msg.sender, candidate, delegation, amount);
        emit Undelegated(msg.sender, candidate, amount);
    }

    /// @notice Moves `amount` of the caller's delegation to `from` to `to` in one call; the caller's
    /// delegated stake and the votes' sum stay as they were.
    function redelegate(address from, address to, uint256 amount) external {
        if (to == address(0)) revert ZeroAddress();
        if (to == from) revert SameCandidate(to);
        uint256 delegation = delegatedTo[msg.sender][from];
        if (amount == 0 || amount > delegation) revert InsufficientDelegation(delegation, amount);
        _remove(msg.sender, from, delegation, amount);
        _add(msg.sender, to, amount);
        emit Redelegated(msg.sender, from, to, amount);
    }

    /// @notice `account`'s delegations, each nonzero, in the order of its candidate list, which
    /// `startUnstake` reduces from the end. Returns every delegation the account holds.
    function delegationsOf(address account) external view returns (Delegation[] memory delegations) {
        EnumerableSet.AddressSet storage candidates = delegatedCandidates[account];
        delegations = new Delegation[](candidates.length());
        for (uint256 i = 0; i < delegations.length; ++i) {
            address candidate = candidates.at(i);
            delegations[i] = Delegation(candidate, delegatedTo[account][candidate]);
        }
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

    /// @dev Takes `amount` from `account`'s delegations, which hold `delegated`, in proportion. From
    /// the last in `delegationsOf` order to the first, each gives the amount still to take times its
    /// share of the delegations not yet reduced, rounded down: what is left to take never exceeds
    /// what is left delegated, so no reduction exceeds its delegation, and the first gives exactly
    /// what remains, so the reductions sum to `amount`. Walking from the end keeps every index still
    /// to visit in place as emptied delegations leave the list.
    function _reduceDelegations(address account, uint256 delegated, uint256 amount) private {
        EnumerableSet.AddressSet storage candidates = delegatedCandidates[account];
        for (uint256 i = candidates.length(); i > 0; --i) {
            address candidate = candidates.at(i - 1);
            uint256 delegation = delegatedTo[account][candidate];
            uint256 taken = Math.mulDiv(amount, delegation, delegated);
            delegated -= delegation;
            amount -= taken;
            if (taken == 0) continue;
            _remove(account, candidate, delegation, taken);
            emit Undelegated(account, candidate, taken);
        }
    }

    /// @dev Adds `amount` to `account`'s delegation to `candidate` and to its votes; the caller
    /// keeps `delegatedStake`.
    function _add(address account, address candidate, uint256 amount) private {
        uint256 delegation = delegatedTo[account][candidate];
        if (delegation == 0) delegatedCandidates[account].add(candidate);
        delegatedTo[account][candidate] = delegation + amount;
        votes[candidate] += amount;
    }

    /// @dev Takes `amount` from `account`'s `delegation` to `candidate` and from its votes, dropping
    /// the candidate from the list once the delegation is empty; the caller keeps `delegatedStake`.
    function _remove(address account, address candidate, uint256 delegation, uint256 amount) private {
        uint256 left = delegation - amount;
        delegatedTo[account][candidate] = left;
        votes[candidate] -= amount;
        if (left == 0) delegatedCandidates[account].remove(candidate);
    }
}
