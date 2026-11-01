// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test, Vm} from "forge-std/Test.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {Staking} from "../src/Staking.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// @notice Random staking, delegation, candidacy, approvals, rotation switches, closes and clock
/// moves among a few actors, recording each election the registry makes.
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
        uint256 free = staking.stakes(account) - staking.lockedBond(account);
        if (free == 0) return;
        vm.prank(account);
        staking.startUnstake(bound(amount, 1, free));
    }

    function completeUnstake(uint256 actorSeed) external {
        address account = actors[actorSeed % actors.length];
        (uint256 amount, uint256 claimableAt) = staking.unstakes(account);
        if (amount == 0 || block.timestamp < claimableAt) return;
        vm.prank(account);
        staking.completeUnstake();
    }

    function delegate(uint256 actorSeed, uint256 candidateSeed) external {
        address account = actors[actorSeed % actors.length];
        // One seed in six abstains.
        uint256 choice = candidateSeed % (actors.length + 1);
        vm.prank(account);
        staking.delegate(choice == actors.length ? address(0) : actors[choice]);
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

    /// @dev Approves three times in four, and withdraws approval otherwise.
    function approve(uint256 actorSeed, uint256 approvedSeed) external {
        uint256 index = actorSeed % actors.length;
        vm.prank(governance);
        registry.setReleaseApproval(actors[index], _digest(index), bytes32(0), approvedSeed % 4 != 0);
    }

    function switchRotation() external {
        bool enabled = registry.rotationEnabled();
        vm.prank(governance);
        registry.setRotation(!enabled);
    }

    function closeEpoch() external {
        (uint64 epochLength,,,,,,,,) = registry.parameters();
        if (block.timestamp < uint256(registry.openEpochStart()) + epochLength) return;
        bool enabled = registry.rotationEnabled();
        bool waiting = registry.pendingElection().effectiveAt != 0;
        vm.recordLogs();
        registry.closeEpoch();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].topics[0] != OperatorRegistry.Elected.selector) continue;
            (,, uint64 effectiveAt) = abi.decode(logs[i].data, (address, bytes32, uint64));
            (,, uint64 finalityMargin,,,,,,) = registry.parameters();
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

    function _digest(uint256 index) internal pure returns (bytes memory) {
        return abi.encodePacked(keccak256(abi.encode(index)), bytes16(0));
    }
}

contract OperatorRegistryInvariantTest is RegistryTest {
    RegistryHandler internal handler;

    function setUp() public {
        _deployRegistry();
        // A short minimum term, so the role can move within one run.
        OperatorRegistry.Parameters memory quick = _parameters();
        quick.minTerm = 2 days;
        vm.prank(address(governance));
        registry.setParameters(quick);
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

    /// @notice Each candidate's votes are exactly the stake of the accounts delegating to it.
    function invariant_VotesAreTheDelegatedStake() public view {
        for (uint256 c = 0; c < handler.actorCount(); c++) {
            address candidate = handler.actor(c);
            uint256 delegated = 0;
            for (uint256 i = 0; i < handler.actorCount(); i++) {
                address account = handler.actor(i);
                if (staking.delegates(account) == candidate) delegated += staking.stakes(account);
            }
            assertEq(staking.votes(candidate), delegated);
        }
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
}
