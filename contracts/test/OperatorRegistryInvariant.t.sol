// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test, Vm} from "forge-std/Test.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {Staking} from "../src/Staking.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// @notice Random staking, delegation, candidacy, approvals, release moves, rotation switches,
/// closes and clock moves among a few actors, recording each election the registry makes.
contract RegistryHandler is Test {
    OperatorRegistry internal registry;
    Staking internal staking;
    MonkeyToken internal monkey;
    address internal vault;
    address internal governance;
    address[5] internal actors =
        [makeAddr("actor 0"), makeAddr("actor 1"), makeAddr("actor 2"), makeAddr("actor 3"), makeAddr("actor 4")];

    /// @notice Elections made while rotation was off, or with an effective time off the notice.
    uint256 public misplacedElections;
    /// @notice Elections made so far.
    uint256 public elections;
    /// @notice Every candidate an election named.
    mapping(address candidate => bool) public everElected;
    /// @notice Release moves that landed on a release not approved for the mover, or while an
    /// election waiting to take effect involved it, or that left the release moved off approved.
    uint256 public misplacedMoves;
    /// @notice Moves that changed the mover's delegated stake or the votes' sum.
    uint256 public unbalancedMoves;
    /// @notice Unstakes that took delegated stake while undelegated stake covered them, or that
    /// left delegated stake below what they did not take.
    uint256 public misallocatedUnstakes;

    constructor(OperatorRegistry registry_, address vault_) {
        registry = registry_;
        staking = registry_.staking();
        monkey = MonkeyToken(address(staking.monkey()));
        vault = vault_;
        governance = address(registry_.governance());
    }

    function actorCount() external pure returns (uint256) {
        return 5;
    }

    function actor(uint256 i) external view returns (address) {
        return actors[i];
    }

    function stake(uint256 actorSeed, uint256 amount) external {
        address account = actors[actorSeed % actors.length];
        amount = bound(amount, 1, 1e24);
        vm.prank(vault);
        monkey.mint(account, amount);
        vm.startPrank(account);
        monkey.approve(address(staking), amount);
        staking.stake(amount);
        vm.stopPrank();
    }

    function startUnstake(uint256 actorSeed, uint256 amount) external {
        address account = actors[actorSeed % actors.length];
        uint256 staked = staking.stakes(account);
        uint256 free = staked - staking.lockedBond(account);
        if (free == 0) return;
        amount = bound(amount, 1, free);
        uint256 delegated = staking.delegatedStake(account);
        uint256 votes = _votes();
        vm.prank(account);
        staking.startUnstake(amount);
        // Undelegated stake goes first; only the rest leaves the delegations, and the votes with it.
        uint256 fromDelegations = amount > staked - delegated ? amount - (staked - delegated) : 0;
        if (staking.delegatedStake(account) != delegated - fromDelegations || _votes() != votes - fromDelegations) {
            misallocatedUnstakes++;
        }
    }

    function completeUnstake(uint256 actorSeed) external {
        address account = actors[actorSeed % actors.length];
        (uint256 amount, uint256 claimableAt) = staking.unstakes(account);
        if (amount == 0 || block.timestamp < claimableAt) return;
        vm.prank(account);
        staking.completeUnstake();
    }

    function delegate(uint256 actorSeed, uint256 candidateSeed, uint256 amount) external {
        address account = actors[actorSeed % actors.length];
        uint256 undelegated = staking.stakes(account) - staking.delegatedStake(account);
        if (undelegated == 0) return;
        vm.prank(account);
        staking.delegate(actors[candidateSeed % actors.length], bound(amount, 1, undelegated));
    }

    function undelegate(uint256 actorSeed, uint256 delegationSeed, uint256 amount) external {
        address account = actors[actorSeed % actors.length];
        Staking.Delegation[] memory delegations = staking.delegationsOf(account);
        if (delegations.length == 0) return;
        Staking.Delegation memory delegation = delegations[delegationSeed % delegations.length];
        vm.prank(account);
        staking.undelegate(delegation.candidate, bound(amount, 1, delegation.amount));
    }

    function redelegate(uint256 actorSeed, uint256 delegationSeed, uint256 toSeed, uint256 amount) external {
        address account = actors[actorSeed % actors.length];
        Staking.Delegation[] memory delegations = staking.delegationsOf(account);
        if (delegations.length == 0) return;
        Staking.Delegation memory from = delegations[delegationSeed % delegations.length];
        uint256 toIndex = toSeed % actors.length;
        if (actors[toIndex] == from.candidate) toIndex = (toIndex + 1) % actors.length;
        address to = actors[toIndex];
        uint256 delegated = staking.delegatedStake(account);
        uint256 votes = _votes();
        vm.prank(account);
        staking.redelegate(from.candidate, to, bound(amount, 1, from.amount));
        if (staking.delegatedStake(account) != delegated || _votes() != votes) unbalancedMoves++;
    }

    /// @dev The votes of every actor, the only candidates the handler delegates to.
    function _votes() internal view returns (uint256 sum) {
        for (uint256 i = 0; i < actors.length; i++) {
            sum += staking.votes(actors[i]);
        }
    }

    /// @dev Stakes up to the bond first, so candidacies are common. The admin key is the address of
    /// the account's value as a private key, a test stand-in for the enclave key, and it authorizes
    /// the registration as `register` requires.
    function register(uint256 actorSeed, uint256 bond) external {
        address account = actors[actorSeed % actors.length];
        bond = bound(bond, 1_000e18, 1e22);
        if (staking.lockedBond(account) != 0 || registry.candidateOf(account).adminKey != address(0)) return;
        uint256 staked = staking.stakes(account);
        if (staked < bond) this.stake(actorSeed, bond - staked);
        address adminKey = vm.addr(uint256(uint160(account)));
        OperatorRegistry.Registration memory registration = OperatorRegistry.Registration(
            adminKey,
            vm.addr(uint256(keccak256(abi.encode("enclave", account)))),
            bond,
            _digest(actorSeed % actors.length),
            bytes32(0),
            "api.example",
            "handover.example:9443"
        );
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(uint256(uint160(account)), registry.registrationDigest(account, adminKey));
        vm.prank(account);
        registry.register(registration, abi.encodePacked(r, s, v));
    }

    /// @dev Acts on one call in four, so candidacies last long enough to be elected.
    function deregister(uint256 actorSeed) external {
        address account = actors[actorSeed % actors.length];
        if (actorSeed % 4 != 0 || registry.candidateOf(account).adminKey == address(0)) return;
        if (registry.operatorInEffect().candidate == account || registry.pendingElection().candidate == account) {
            return;
        }
        vm.prank(account);
        registry.deregister();
    }

    /// @dev Approves three times in four, and withdraws approval otherwise, of one of the actor's
    /// two releases: the one it registers, or the one it may move to.
    function approve(uint256 actorSeed, uint256 approvedSeed) external {
        uint256 index = actorSeed % actors.length;
        vm.prank(governance);
        registry.setReleaseApproval(
            actors[index], _digest(_releaseIndex(index, approvedSeed / 4)), bytes32(0), approvedSeed % 4 != 0
        );
    }

    /// @dev Tries to move an actor to one of its two releases, counting a move that should have
    /// been refused. While an election waits, each call is an upgrade attempt by either side of it;
    /// otherwise every third call is one by the operator in effect; either only when that is an
    /// actor, and to a release the timelock approves for it first.
    function moveRelease(uint256 actorSeed, uint256 releaseSeed) external {
        uint256 index = actorSeed % actors.length;
        bool upgrade = false;
        OperatorRegistry.Election memory waiting = registry.pendingElection();
        address target;
        if (waiting.effectiveAt != 0) {
            target = actorSeed % 2 == 0 ? waiting.candidate : registry.operatorInEffect().candidate;
        } else if (actorSeed % 3 == 0) {
            target = registry.operatorInEffect().candidate;
        }
        for (uint256 i = 0; i < actors.length; i++) {
            if (target != address(0) && actors[i] == target) (index, upgrade) = (i, true);
        }
        address account = actors[index];
        bytes memory digest = _digest(_releaseIndex(index, releaseSeed));
        bytes32 release = keccak256(abi.encodePacked(digest, bytes32(0)));
        if (upgrade) {
            vm.prank(governance);
            registry.setReleaseApproval(account, digest, bytes32(0), true);
        }
        bool approved = registry.releaseApproved(keccak256(abi.encode(account, release)));
        bool involved = waiting.effectiveAt != 0
            && (account == waiting.candidate || account == registry.operatorInEffect().candidate);
        bytes32 left = registry.candidateOf(account).release;
        vm.prank(account);
        try registry.setRelease(digest, bytes32(0)) {
            bool stillApproved = left != release && registry.releaseApproved(keccak256(abi.encode(account, left)));
            if (!approved || involved || stillApproved) misplacedMoves++;
        } catch {}
    }

    function switchRotation() external {
        bool enabled = registry.rotationEnabled();
        vm.prank(governance);
        registry.setRotation(!enabled);
    }

    function closeEpoch() external {
        (uint64 epochLength,,,,,,) = registry.parameters();
        if (block.timestamp < uint256(registry.openEpochStart()) + epochLength) return;
        bool enabled = registry.rotationEnabled();
        bool waiting = registry.pendingElection().effectiveAt != 0;
        vm.recordLogs();
        registry.closeEpoch();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].topics[0] != OperatorRegistry.Elected.selector) continue;
            (,, uint64 effectiveAt) = abi.decode(logs[i].data, (address, bytes32, uint64));
            (, uint64 finalityMargin,,,,,) = registry.parameters();
            if (!enabled || waiting || effectiveAt != block.timestamp + registry.NOTICE() + finalityMargin) {
                misplacedElections++;
            }
            everElected[address(uint160(uint256(logs[i].topics[2])))] = true;
            elections++;
        }
    }

    function warp(uint256 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 1, 10 days));
    }

    /// @dev Warps to a waiting election's effective time, so elections take effect within a run
    /// rather than mostly being cancelled during their notice.
    function awaitElection() external {
        uint64 effectiveAt = registry.pendingElection().effectiveAt;
        if (effectiveAt != 0) vm.warp(effectiveAt);
    }

    function _digest(uint256 index) internal pure returns (bytes memory) {
        return abi.encodePacked(keccak256(abi.encode(index)), bytes16(0));
    }

    /// @dev The actor's registered release index, or its alternative one, by `seed`'s parity.
    function _releaseIndex(uint256 index, uint256 seed) internal view returns (uint256) {
        return index + (seed % 2) * actors.length;
    }
}

contract OperatorRegistryInvariantTest is RegistryTest {
    RegistryHandler internal handler;

    function setUp() public {
        _deployRegistry();
        handler = new RegistryHandler(registry, address(vault));
        targetContract(address(handler));
    }

    /// @notice The staked total is every account's stake, and the contract holds it with every
    /// pending unstake.
    function invariant_StakingHoldsTheStakedTotalAndPendingUnstakes() public view {
        uint256 staked = 0;
        uint256 exiting = 0;
        for (uint256 i = 0; i < handler.actorCount(); i++) {
            address account = handler.actor(i);
            staked += staking.stakes(account);
            (uint256 amount,) = staking.unstakes(account);
            exiting += amount;
        }
        assertEq(staking.totalStaked(), staked);
        assertEq(monkey.balanceOf(address(staking)), staked + exiting);
    }

    /// @notice Each candidate's votes are exactly the amounts the accounts delegate to it.
    function invariant_VotesAreTheDelegatedAmounts() public view {
        for (uint256 c = 0; c < handler.actorCount(); c++) {
            address candidate = handler.actor(c);
            uint256 delegated = 0;
            for (uint256 i = 0; i < handler.actorCount(); i++) {
                delegated += staking.delegatedTo(handler.actor(i), candidate);
            }
            assertEq(staking.votes(candidate), delegated);
        }
    }

    /// @notice An account's delegations are its listed, nonzero, distinct amounts, which sum to its
    /// delegated stake, and never exceed its stake.
    function invariant_NoStakerDelegatesMoreThanItsStake() public view {
        for (uint256 i = 0; i < handler.actorCount(); i++) {
            address account = handler.actor(i);
            Staking.Delegation[] memory delegations = staking.delegationsOf(account);
            uint256 sum = 0;
            uint256 listed = 0;
            for (uint256 d = 0; d < delegations.length; d++) {
                assertGt(delegations[d].amount, 0);
                assertEq(staking.delegatedTo(account, delegations[d].candidate), delegations[d].amount);
                for (uint256 e = 0; e < d; e++) {
                    assertTrue(delegations[e].candidate != delegations[d].candidate);
                }
                sum += delegations[d].amount;
            }
            for (uint256 c = 0; c < handler.actorCount(); c++) {
                if (staking.delegatedTo(account, handler.actor(c)) != 0) listed++;
            }
            assertEq(delegations.length, listed);
            assertEq(staking.delegatedStake(account), sum);
            assertLe(sum, staking.stakes(account));
        }
    }

    /// @notice A move changes neither the mover's delegated stake nor the votes' sum, and an unstake
    /// takes delegated stake only past the undelegated stake, the votes falling by the same amount.
    function invariant_MovesAndUnstakesKeepTheTallies() public view {
        assertEq(handler.unbalancedMoves(), 0);
        assertEq(handler.misallocatedUnstakes(), 0);
    }

    /// @notice No account's stake falls below its locked self-bond, and a registered candidate's
    /// locked bond is the bond it registered.
    function invariant_SelfBondsStayStaked() public view {
        for (uint256 i = 0; i < handler.actorCount(); i++) {
            address account = handler.actor(i);
            assertGe(staking.stakes(account), staking.lockedBond(account));
            OperatorRegistry.Candidate memory candidate = registry.candidateOf(account);
            if (candidate.adminKey != address(0)) assertEq(staking.lockedBond(account), candidate.bond);
        }
        assertLe(registry.candidateCount(), MAX_CANDIDATES);
    }

    /// @notice Elections happen only while rotation is on and none is waiting, each taking effect at
    /// its close plus the notice and the finality margin; the operator in effect is the deployer or
    /// a candidate an election named.
    function invariant_TheRoleMovesOnlyByElection() public view {
        assertEq(handler.misplacedElections(), 0);
        address inEffect = registry.operatorInEffect().candidate;
        assertTrue(inEffect == deployerKey || handler.everElected(inEffect));
    }

    /// @notice A release moves only to one the timelock approved for the mover, never for either
    /// side of an election waiting to take effect, and one way: the release left loses its approval.
    function invariant_ReleasesMoveOnlyWhenApprovedAndNoElectionWaits() public view {
        assertEq(handler.misplacedMoves(), 0);
    }

    /// @notice A candidate is listed exactly while it is registered and the release it registered,
    /// or last moved to, is approved for it.
    function invariant_ListingFollowsTheRegisteredReleasesApproval() public view {
        for (uint256 i = 0; i < handler.actorCount(); i++) {
            address account = handler.actor(i);
            OperatorRegistry.Candidate memory candidate = registry.candidateOf(account);
            bool approved = candidate.adminKey != address(0)
                && registry.releaseApproved(keccak256(abi.encode(account, candidate.release)));
            bool listed = false;
            for (uint256 j = 0; j < registry.candidateCount(); j++) {
                listed = listed || registry.candidateList(j) == account;
            }
            assertEq(listed, approved);
        }
    }

    /// @notice An elected operator in effect runs the release its candidacy holds: an election
    /// names the release registered at its close, and the operator's move changes both.
    function invariant_AnElectedOperatorRunsItsCandidacysRelease() public view {
        OperatorRegistry.Operator memory operator = registry.operatorInEffect();
        if (operator.candidate == deployerKey) return;
        assertEq(operator.release, registry.candidateOf(operator.candidate).release);
    }
}
