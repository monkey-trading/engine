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

/// @notice Random stake, undelegations, standby keys, closes, draws, relays, receiver swaps, release
/// moves, approvals, eligibility and margin changes, rotation switches, installations, syncs, timelock
/// releases and clock moves over four candidates, recording every generation handed out, every naming
/// the vault puts in effect and every set a close adopts.
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
    uint256 internal immutable signerKey;
    uint256 internal standbyKeys;

    /// @notice Generations a fold, a draw or a release move handed out.
    mapping(uint64 generation => bool) public handedOut;
    /// @notice Hand-outs of a generation handed out before.
    uint256 public reusedGenerations;
    /// @notice Draws of the operator the registry named, on its release folded in the vault, that
    /// did not keep the vault's generation.
    uint256 public misinherited;
    /// @notice Draws whose source is not the operator the registry named at the draw.
    uint256 public wrongSources;
    /// @notice The release each generation was in effect for, while the vault reported it folded,
    /// and the member of the turn that named it, if one did.
    mapping(uint64 generation => bytes32 release) public releaseOf;
    mapping(uint64 generation => address member) public memberOf;
    /// @notice Generations seen in effect for two different namings.
    uint256 public sharedGenerations;
    mapping(uint64 turn => address member) public drawnMember;
    uint64[] public drawnTurns;
    uint64 public highestDrawn;
    uint256 public lastDrawnFell;
    /// @notice Set members adopted with a weight of zero or above their votes at the close.
    uint256 public overweighted;
    /// @notice Sets adopted with the quorum whose keyed members' weights fall short of it, or naming a
    /// member with no promotable standby key that the set before did not name.
    uint256 public unbackedQuorums;
    /// @notice Members a set adopted with the quorum named with no promotable standby key when the sets
    /// before it had named them with none since an epoch length ago or longer, with no key held since.
    uint256 public repeatedCarries;
    /// @notice The last set that named each member with no promotable standby key, and when the run of
    /// sets naming it with none that it ends began.
    mapping(address member => uint32 setId) public keylessIn;
    mapping(address member => uint256 since) public keylessSince;
    /// @notice Sets adopted below the quorum, and those with members.
    uint256 public carriedSets;
    uint256 public adoptedSets;

    constructor(OperatorSchedule schedule_, Vault vault_, address[] memory actors_, uint256 signerKey_) {
        schedule = schedule_;
        signerKey = signerKey_;
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

    /// @dev Takes back part of an actor's delegation to itself, so closes fall under the quorum and
    /// carried weights exceed votes.
    function undelegate(uint256 actorSeed, uint256 amount) external observed {
        address account = actors[actorSeed % actors.length];
        uint256 delegation = staking.delegatedTo(account, account);
        if (delegation == 0) return;
        vm.prank(account);
        staking.undelegate(account, bound(amount, 1, delegation));
    }

    /// @dev Registers a fresh standby key for an actor on its current release, vouched by the vault's
    /// signer, as its node does after a release move.
    function registerStandby(uint256 actorSeed) external observed {
        address account = actors[actorSeed % actors.length];
        uint256 secret = uint256(keccak256(abi.encode("handler standby key", standbyKeys++)));
        uint64 expiry = uint64(block.timestamp + 1 days);
        bytes32 release = registry.candidateOf(account).release;
        bytes32 digest = schedule.vouchDigest(account, vm.addr(secret), release, 1, expiry);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerKey, digest);
        vm.prank(account);
        // A key registered ends the account's run of carries with no key.
        try schedule.registerStandby(vm.addr(secret), 1, expiry, abi.encodePacked(r, s, v)) {
            keylessIn[account] = 0;
        } catch {}
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
        _draw(turn);
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
        SeedReceiver fresh = new SeedReceiver(makeAddr(string(abi.encode("relay", receivers++))), 1606824023, 1 days);
        vm.prank(governance);
        schedule.setSeedReceiver(fresh);
    }

    /// @dev Moves the clock, and a keeper then draws the turn open now.
    function warp(uint256 seconds_) external observed {
        vm.warp(block.timestamp + bound(seconds_, 1 hours, 2 days));
        _draw(_nextTurn());
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
        if (schedule.promotable(account) != address(0)) keylessIn[account] = 0;
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
        if (schedule.promotable(account) != address(0)) keylessIn[account] = 0;
    }

    /// @dev Raises the eligibility share past every candidate's, so closes adopt empty sets, or
    /// restores it.
    function toggleEligibility() external observed {
        OperatorRegistry.Parameters memory p = _parameters();
        p.eligibilityBps = p.eligibilityBps == 10_000 ? 500 : 10_000;
        vm.prank(governance);
        registry.setParameters(p);
    }

    /// @dev Drops the finality margin to nothing or restores it, so a close's own notice may end
    /// before an earlier set's.
    function toggleMargin() external observed {
        OperatorRegistry.Parameters memory p = _parameters();
        p.finalityMargin = p.finalityMargin == 0 ? 1 hours : 0;
        vm.prank(governance);
        registry.setParameters(p);
    }

    function _parameters() internal view returns (OperatorRegistry.Parameters memory) {
        (
            uint64 epochLength,
            uint64 finalityMargin,
            uint16 eligibilityBps,
            uint16 quorumBps,
            uint16 maxCandidates,
            uint256 minSelfBond,
            uint64 livenessTimeout
        ) = registry.parameters();
        return OperatorRegistry.Parameters(
            epochLength, finalityMargin, eligibilityBps, quorumBps, maxCandidates, minSelfBond, livenessTimeout
        );
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

    /// @dev Draws `turn`, checking that it continues the named operator's holding in the vault's
    /// generation, and otherwise takes a new one, and that its source is the operator named.
    function _draw(uint64 turn) internal {
        OperatorRegistry.Operator memory named = registry.namedOperator();
        uint64 generation = vault.releaseGeneration();
        bytes32 folded = vault.syncedRelease();
        try schedule.draw(turn) returns (address candidate) {
            OperatorSchedule.Turn memory drawn = schedule.turnOf(turn);
            drawnMember[turn] = candidate;
            drawnTurns.push(turn);
            if (drawn.source != named.candidate) wrongSources++;
            bytes32 release = registry.candidateOf(candidate).release;
            if (candidate == named.candidate && release == named.release && folded == release) {
                if (drawn.generation != generation) misinherited++;
            } else {
                _handOut(drawn.generation);
            }
        } catch {}
    }

    /// @dev Checks each generation a fold or a release move in the call handed out.
    function _record(Vm.Log[] memory logs) internal {
        uint64[] memory fresh = new uint64[](logs.length);
        uint256 count = 0;
        for (uint256 i = 0; i < logs.length; ++i) {
            bytes32 topic = logs[i].topics[0];
            uint64 generation;
            if (logs[i].emitter == address(schedule) && topic == OperatorSchedule.RotationSetAdopted.selector) {
                _checkSet(uint32(uint256(logs[i].topics[1])), logs[i].data);
                continue;
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
            _handOut(generation);
        }
    }

    /// @dev Checks a set the call's close adopted against the votes, keys and stake at that close.
    function _checkSet(uint32 setId, bytes memory data) internal {
        (, bool quorum, address[] memory members,, uint256[] memory weights) =
            abi.decode(data, (uint64, bool, address[], address[], uint256[]));
        if (members.length != 0) adoptedSets++;
        if (!quorum) carriedSets++;
        (,, address[] memory before,) = schedule.setOf(setId - 1);
        uint256 weighed = 0;
        for (uint256 i = 0; i < members.length; ++i) {
            if (weights[i] == 0 || weights[i] > staking.votes(members[i])) overweighted++;
            if (schedule.promotable(members[i]) != address(0)) {
                weighed += weights[i];
                continue;
            }
            if (keylessIn[members[i]] == 0 || keylessIn[members[i]] != setId - 1) {
                keylessSince[members[i]] = block.timestamp;
            }
            (uint64 epochLength,,,,,,) = registry.parameters();
            if (quorum && block.timestamp >= keylessSince[members[i]] + epochLength) repeatedCarries++;
            keylessIn[members[i]] = setId;
            bool carried = false;
            for (uint256 j = 0; j < before.length; ++j) {
                carried = carried || before[j] == members[i];
            }
            if (quorum && !carried) unbackedQuorums++;
        }
        (,,, uint16 quorumBps,,,) = registry.parameters();
        if (quorum && weighed * 10_000 < uint256(quorumBps) * staking.totalStaked()) unbackedQuorums++;
    }

    function _handOut(uint64 generation) internal {
        if (handedOut[generation]) reusedGenerations++;
        handedOut[generation] = true;
    }

    /// @dev Records the naming the vault puts in effect now, and the last drawn turn.
    function _observe() internal {
        uint64 last = schedule.lastDrawnTurn();
        if (last < highestDrawn) lastDrawnFell++;
        else highestDrawn = last;
        bytes32 release = vault.currentRelease();
        if (vault.syncedRelease() != release) return;
        uint64 generation = vault.releaseGeneration();
        bytes32 seen = releaseOf[generation];
        if (seen == 0) releaseOf[generation] = release;
        else if (seen != release) sharedGenerations++;
        if (registry.operatorGeneration() == 0) return;
        // A turn may continue the vault's own fold, but a generation never passes between members.
        address member = registry.namedOperator().candidate;
        address before = memberOf[generation];
        if (before == address(0)) memberOf[generation] = member;
        else if (before != member) sharedGenerations++;
    }

    function _nextTurn() internal view returns (uint64) {
        return uint64((block.timestamp - schedule.boundary(0)) / 1 days + 1);
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
        handler = new ScheduleHandler(schedule, vault, actors, privateKeyOf[vault.checkpointSigner()]);
        targetContract(address(handler));
    }

    /// @notice Every generation a fold, a draw or a release move hands out is new, and a draw of the
    /// operator the registry names, on its release the vault folded, keeps the vault's.
    function invariant_NoGenerationIsHandedOutTwice() public view {
        assertEq(handler.reusedGenerations(), 0);
        assertEq(handler.misinherited(), 0);
    }

    /// @notice No generation the vault reports folded is ever in effect for two different releases,
    /// nor for the turns of two different members.
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

    /// @notice A drawn turn keeps the member it was drawn with, and its source is the operator the
    /// registry named at its draw.
    function invariant_DrawnTurnsKeepTheirMemberAndSource() public view {
        assertEq(handler.wrongSources(), 0);
        for (uint256 i = 0; i < handler.drawnCount(); ++i) {
            uint64 turn = handler.drawnTurns(i);
            assertEq(schedule.turnOf(turn).candidate, handler.drawnMember(turn));
        }
    }

    /// @notice Sets take effect in the order adopted, so the set in effect at any time is the latest
    /// whose effective time has come.
    function invariant_SetsTakeEffectInAdoptionOrder() public view {
        uint64 previous = 0;
        for (uint32 id = 1; id <= schedule.setCount(); ++id) {
            (, uint64 effectiveAt,,) = schedule.setOf(id);
            assertGe(effectiveAt, previous);
            previous = effectiveAt;
        }
    }

    /// @notice A set weighs each member above zero and at most its votes at the close; a set adopted
    /// with the quorum names a member with no promotable standby key only if the set before named it,
    /// and not when sets have named it with none for an epoch length, and its members holding one make
    /// the quorum.
    function invariant_ASetWeighsItsMembersByTheirVotesAtItsClose() public view {
        assertEq(handler.overweighted(), 0);
        assertEq(handler.unbackedQuorums(), 0);
        assertEq(handler.repeatedCarries(), 0);
    }

    /// @notice The last drawn turn only increases.
    function invariant_TheLastDrawnTurnOnlyIncreases() public view {
        assertEq(handler.lastDrawnFell(), 0);
        assertGe(schedule.lastDrawnTurn(), handler.highestDrawn());
    }
}
