// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test, Vm} from "forge-std/Test.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {Staking} from "../src/Staking.sol";
import {Vault} from "../src/Vault.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";

/// @notice Random stake, closes, draws, relays, receiver swaps, release moves, approvals, rotation
/// switches, installations, syncs, timelock releases and clock moves over four candidates, recording
/// every generation handed out and every naming the vault puts in effect.
contract ScheduleHandler is Test {
    bytes32 internal constant CONFIG = keccak256("candidate config");

    OperatorRegistry internal immutable registry;
    OperatorSchedule internal immutable schedule;
    Vault internal immutable vault;
    Staking internal immutable staking;
    MonkeyToken internal immutable monkey;
    address internal immutable governance;
    address[] internal actors;
    uint256 internal releases = 100;
    uint256 internal receivers;

    /// @notice Generations a fold, a draw or a release move handed out.
    mapping(uint64 generation => bool) public handedOut;
    /// @notice Hand-outs of a generation handed out before.
    uint256 public reusedGenerations;
    /// @notice Draws of the member of the turn before that did not keep its generation.
    uint256 public misinherited;
    /// @notice The release and turn member (zero for the vault's own fold) each generation was in
    /// effect for, while the vault reported it folded.
    mapping(uint64 generation => bytes32 naming) public namingOf;
    /// @notice Generations seen in effect for two different namings.
    uint256 public sharedGenerations;
    mapping(uint64 turn => address member) public drawnMember;
    uint64[] public drawnTurns;
    uint64 public highestDrawn;
    uint256 public lastDrawnFell;

    constructor(OperatorSchedule schedule_, Vault vault_, address[] memory actors_) {
        schedule = schedule_;
        registry = schedule_.registry();
        vault = vault_;
        staking = registry.staking();
        monkey = MonkeyToken(address(staking.monkey()));
        governance = address(registry.governance());
        actors = actors_;
    }

    modifier observed() {
        vm.recordLogs();
        _;
        _record(vm.getRecordedLogs());
        _observe();
    }

    function drawnCount() external view returns (uint256) {
        return drawnTurns.length;
    }

    function stake(uint256 actorSeed, uint256 amount) external observed {
        address account = actors[actorSeed % actors.length];
        amount = bound(amount, 1, 50_000e18);
        vm.prank(address(vault));
        monkey.mint(account, amount);
        vm.startPrank(account);
        monkey.approve(address(staking), amount);
        staking.stake(amount);
        staking.delegate(account, amount);
        vm.stopPrank();
    }

    function close() external observed {
        (uint64 epochLength,,,,,,) = registry.parameters();
        if (block.timestamp < uint256(registry.openEpochStart()) + epochLength) return;
        registry.closeEpoch();
    }

    /// @dev Draws the turn open now, or, once it is drawn, waits for the next one's window and draws
    /// that.
    function draw() external observed {
        uint64 turn = _nextTurn();
        if (schedule.turnOf(turn).candidate != address(0)) vm.warp(schedule.drawDeadline(++turn));
        try schedule.draw(turn) {} catch {}
    }

    function relay(bytes32 seed) external observed {
        SeedReceiver receiver = schedule.seedReceiver();
        uint64 turn = _nextTurn() + 1;
        uint64 epoch = receiver.seedEpoch(turn);
        vm.prank(receiver.relayAlias());
        try receiver.receiveSeed(turn, epoch, seed == 0 ? bytes32(uint256(1)) : seed) {} catch {}
    }

    /// @dev One call in four swaps in a receiver with no relays, as an Ethereum fork would.
    function swapReceiver(uint256 seed) external observed {
        if (seed % 4 != 0) return;
        SeedReceiver fresh = new SeedReceiver(makeAddr(string(abi.encode("relay", receivers++))), 1606824023);
        vm.prank(governance);
        schedule.setSeedReceiver(fresh);
    }

    /// @dev Moves the clock, and a keeper then draws the turn open now.
    function warp(uint256 seconds_) external observed {
        vm.warp(block.timestamp + bound(seconds_, 1 hours, 2 days));
        try schedule.draw(_nextTurn()) {} catch {}
    }

    function awaitSet() external observed {
        uint32 pending = schedule.pendingSetId();
        if (pending == 0) return;
        (, uint64 effectiveAt,,) = schedule.setOf(pending);
        vm.warp(effectiveAt);
    }

    /// @dev Moves an actor to a new release the timelock approves for it first.
    function moveRelease(uint256 actorSeed) external observed {
        address account = actors[actorSeed % actors.length];
        bytes memory digest = _digest(releases++);
        vm.prank(governance);
        registry.setReleaseApproval(account, digest, CONFIG, true);
        vm.prank(account);
        registry.setRelease(digest, CONFIG);
    }

    /// @dev Withdraws or restores the approval of the release an actor holds, delisting or
    /// relisting it.
    function toggleApproval(uint256 actorSeed) external observed {
        address account = actors[actorSeed % actors.length];
        OperatorRegistry.Candidate memory candidate = registry.candidateOf(account);
        bool approved = registry.releaseApproved(keccak256(abi.encode(account, candidate.release)));
        // The approval is keyed by the release hash, so the call names it through a digest that
        // the handler or the setup made.
        (bool found, bytes memory digest) = _digestOf(candidate.release);
        if (!found) return;
        vm.prank(governance);
        registry.setReleaseApproval(account, digest, CONFIG, !approved);
    }

    /// @dev Acts on one call in three, so turns run for most of a run.
    function switchRotation(uint256 seed) external observed {
        if (seed % 3 != 0) return;
        bool on = registry.rotationSwitch();
        vm.prank(governance);
        registry.setRotation(!on);
    }

    function install(uint256 actorSeed) external observed {
        address candidate = actors[actorSeed % actors.length];
        vm.prank(governance);
        try registry.installOperator(
            candidate,
            vm.addr(uint256(keccak256(abi.encode("installed", actorSeed)))),
            _digest(500 + actorSeed % 3),
            CONFIG
        ) {}
            catch {}
    }

    function sync() external observed {
        vault.syncRelease();
    }

    function nameRelease(uint256 seed) external observed {
        vm.prank(governance);
        try vault.setCurrentRelease(_digest(600 + seed % 4), CONFIG) {} catch {}
    }

    /// @dev Checks each generation the call handed out, and each drawn turn's inheritance.
    function _record(Vm.Log[] memory logs) internal {
        uint64[] memory fresh = new uint64[](logs.length);
        uint256 count = 0;
        for (uint256 i = 0; i < logs.length; ++i) {
            bytes32 topic = logs[i].topics[0];
            uint64 generation;
            if (logs[i].emitter == address(schedule) && topic == OperatorSchedule.TurnAssigned.selector) {
                uint64 turn = uint64(uint256(logs[i].topics[1]));
                address candidate;
                (,, candidate,,, generation) =
                    abi.decode(logs[i].data, (uint16, uint8, address, address, bytes32, uint64));
                drawnMember[turn] = candidate;
                drawnTurns.push(turn);
                OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
                OperatorSchedule.Turn memory prior = schedule.turnOf(drawn.prior);
                if (drawn.prior + 1 == turn && prior.candidate == candidate) {
                    if (generation != prior.generation) misinherited++;
                    continue;
                }
            } else if (logs[i].emitter == address(schedule) && topic == OperatorSchedule.TurnGenerationMoved.selector) {
                generation = abi.decode(logs[i].data, (uint64));
            } else if (logs[i].emitter == address(vault) && topic == Vault.ReleaseSynced.selector) {
                generation = abi.decode(logs[i].data, (uint64));
            } else if (logs[i].emitter == address(vault) && topic == Vault.CurrentReleaseChanged.selector) {
                (generation,,) = abi.decode(logs[i].data, (uint64, bytes, bytes32));
            } else {
                continue;
            }
            bool repeated = false;
            for (uint256 j = 0; j < count; ++j) {
                repeated = repeated || fresh[j] == generation;
            }
            // A release move gives the two turns it touches one generation.
            if (repeated) continue;
            fresh[count++] = generation;
            if (handedOut[generation]) reusedGenerations++;
            handedOut[generation] = true;
        }
    }

    /// @dev Records the naming the vault puts in effect now, and the last drawn turn.
    function _observe() internal {
        uint64 last = schedule.lastDrawnTurn();
        if (last < highestDrawn) lastDrawnFell++;
        else highestDrawn = last;
        bytes32 release = vault.currentRelease();
        if (vault.syncedRelease() != release) return;
        uint64 generation = vault.releaseGeneration();
        address member = registry.operatorGeneration() != 0 ? registry.namedOperator().candidate : address(0);
        bytes32 naming = keccak256(abi.encode(release, member));
        bytes32 seen = namingOf[generation];
        if (seen == 0) namingOf[generation] = naming;
        else if (seen != naming) sharedGenerations++;
    }

    function _nextTurn() internal view returns (uint64) {
        return uint64((block.timestamp - schedule.BOUNDARY_OFFSET()) / 1 days + 1);
    }

    function _digest(uint256 seed) internal pure returns (bytes memory) {
        return abi.encodePacked(keccak256(abi.encode(seed)), bytes16(keccak256(abi.encode(seed, 1))));
    }

    function _digestOf(bytes32 release) internal view returns (bool, bytes memory) {
        for (uint256 seed = 0; seed < releases; ++seed) {
            if (seed > actors.length && seed < 100) seed = 100;
            bytes memory digest = _digest(seed);
            if (keccak256(abi.encodePacked(digest, CONFIG)) == release) return (true, digest);
        }
        return (false, "");
    }
}

contract OperatorScheduleInvariantTest is ScheduleTest {
    ScheduleHandler internal handler;

    function setUp() public {
        _deploySchedule();
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_digest(0), CONFIG)));
        address[] memory actors = new address[](4);
        uint256[] memory stakes = new uint256[](4);
        for (uint256 i = 0; i < 4; ++i) {
            actors[i] = makeAddr(string(abi.encode("actor", i)));
            stakes[i] = (4 - i) * 10_000e18;
        }
        _rotate(actors, stakes);
        uint64 first = _nextTurn() + 1;
        _relay(first, keccak256("first seed"));
        vm.warp(schedule.drawDeadline(first));
        handler = new ScheduleHandler(schedule, vault, actors);
        targetContract(address(handler));
    }

    /// @notice Every generation a fold, a draw or a release move hands out is new, and a member drawn
    /// for the turn after its own keeps that turn's.
    function invariant_NoGenerationIsHandedOutTwice() public view {
        assertEq(handler.reusedGenerations(), 0);
        assertEq(handler.misinherited(), 0);
    }

    /// @notice No generation the vault reports folded is ever in effect for two different releases
    /// or holdings.
    function invariant_NoGenerationNamesTwoHoldings() public view {
        assertEq(handler.sharedGenerations(), 0);
    }

    /// @notice While a turn names the operator, the vault names its member's release in its
    /// generation, folded, with no transaction.
    function invariant_TheVaultFollowsTheNamedTurn() public view {
        uint64 generation = registry.operatorGeneration();
        if (generation == 0) return;
        (, OperatorSchedule.Turn memory turn) = schedule.turnAt(uint64(block.timestamp));
        assertEq(generation, turn.generation);
        assertEq(vault.releaseGeneration(), turn.generation);
        assertEq(vault.currentRelease(), registry.candidateOf(turn.candidate).release);
        assertEq(vault.syncedRelease(), vault.currentRelease());
        assertTrue(registry.rotationEnabled());
    }

    /// @notice A drawn turn keeps the member it was drawn with.
    function invariant_DrawnTurnsKeepTheirMember() public view {
        for (uint256 i = 0; i < handler.drawnCount(); ++i) {
            uint64 turn = handler.drawnTurns(i);
            assertEq(schedule.turnOf(turn).candidate, handler.drawnMember(turn));
        }
    }

    /// @notice The last drawn turn only increases.
    function invariant_TheLastDrawnTurnOnlyIncreases() public view {
        assertEq(handler.lastDrawnFell(), 0);
        assertGe(schedule.lastDrawnTurn(), handler.highestDrawn());
    }
}
