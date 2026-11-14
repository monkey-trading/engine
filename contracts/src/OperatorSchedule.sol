// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {ARB_SYS} from "./ArbSys.sol";
import {LogHeads} from "./LogHeads.sol";
import {OperatorRegistry} from "./OperatorRegistry.sol";
import {SeedReceiver} from "./SeedReceiver.sol";
import {SeedSchedule} from "./SeedSchedule.sol";
import {Vault} from "./Vault.sol";

/// @title OperatorSchedule
/// @notice Names the operator by daily turns, for a registry that names this schedule. Every election
/// close while rotation is on adopts a rotation set, which takes effect seven days plus the finality
/// margin after its close, and no earlier than the set adopted before it; sets waiting to take effect
/// queue in close order. With a quorum the set is the close's eligible candidates weighted by their
/// votes; without one it is the latest adopted set less the members no longer eligible or registered
/// under another admin key, their weights unchanged. A set holds each member's admin key as adopted.
/// An empty set replaces one with members, and no turn is drawn from it.
/// @notice Anyone draws turn `n` from its draw deadline, a day before its boundary at 22:30 UTC on day
/// `n`, until that boundary. The seed and the set, the one in effect at the deadline, are fixed by
/// then, so the moment of the draw picks neither. Position by position, the turn's seed picks a member
/// with probability proportional to its weight, with replacement, and the first one the registry lists
/// under the admin key the set adopted takes the turn; after 64 positions a scan of every member, from
/// an index the seed fixes, takes the first available, so a turn goes undrawn only when no member is
/// available. Availability is read at the draw: a deregistration or a withdrawn approval landing in the
/// window lets whoever draws choose by drawing before or after it. A member the registry names at the
/// draw, on the release it runs, keeps the generation the vault holds for it; any other member, and a
/// member whose release moves, takes a new one from the vault, so a node never reopens keys sealed for
/// an earlier holding.
/// @notice A turn names its member from its boundary, and the last turn drawn keeps naming its member
/// after it ends until another turn starts or the registry's timelock installs an operator. Turning
/// rotation off stops draws and adoptions; turns already drawn stand.
/// @notice Liveness: while a turn names the operator, its enclave posts `LogHeads` v2 heartbeats, and
/// anyone may `witness` each slot of `heartbeatInterval` seconds of block time. Once the
/// `heartbeatMisses` slots before the current one are all marked, all after the slot of the signer
/// epoch's latest head and of the signer change that started it, anyone may `promote`: the vault's
/// signer moves at once, in a new epoch, to the vouched standby key of the next member in the turn's
/// order that qualifies, and the promotion becomes the turn's assignment in a new generation.
/// @notice Every assignment of a turn, drawn or promoted, is kept by its position with its releases
/// and generations, and `turns` follows the latest.
/// @dev Seeds come from `seedReceiver`, which the registry's timelock replaces when an Ethereum fork
/// needs a new relay and receiver. A turn the receiver has no seed for, because no seed was relayed at
/// or before it, draws from `keccak256(abi.encode(seedAnchor, turn))`, where `seedAnchor` is the last
/// drawn turn's seed when the receiver was replaced, so no later draw changes it; only that refusal
/// falls back, and any other failure of the read reverts the draw, so a caller cannot pick the
/// fallback by limiting gas. Not upgradeable.
contract OperatorSchedule is SeedSchedule {
    /// @notice A rotation set's member, its admin key as adopted, and the sum of the weights up to
    /// and including its own.
    struct Member {
        address candidate;
        address adminKey;
        uint256 cumulative;
    }

    struct RotationSet {
        uint64 epoch;
        uint64 effectiveAt;
        Member[] members;
    }

    /// @notice A drawn turn. `candidate`, `position`, `reason` and `generation` are its latest
    /// assignment's; `setId`, `prior` (the turn drawn before it, zero for the first), `seed` and
    /// `source` (the operator the registry named at the draw, which hands the member the keys) are
    /// the draw's. `reason` is 0 for a draw and 2 for a promotion.
    struct Turn {
        address candidate;
        uint16 position;
        uint8 reason;
        uint32 setId;
        uint64 generation;
        uint64 prior;
        bytes32 seed;
        address source;
    }

    /// @notice The heartbeat rule. Slot `k` is `[k * heartbeatInterval, (k + 1) * heartbeatInterval)`
    /// of block time, in seconds; a promotion needs the last `heartbeatMisses` complete slots marked.
    /// A live head's signing time must be within `headTolerance` seconds of its block's, and a held
    /// head binds one of the last `headBlocks` Arbitrum blocks.
    struct Liveness {
        uint64 heartbeatInterval;
        uint64 headTolerance;
        uint16 heartbeatMisses;
        uint16 headBlocks;
    }

    /// @notice A witnessed slot's block time and Arbitrum block.
    struct Mark {
        uint64 at;
        uint64 blockNumber;
    }

    /// @notice An assignment of a turn: its member, the admin key its set adopted, the set and the
    /// reason, and, for a promotion, the standby key it installed, the signer epoch it started, the
    /// outgoing epoch's latest head and the vault's latest accepted checkpoint, which the promoted
    /// member's copy must hold; zeros for a draw. `at` is the block time it was made.
    struct Assignment {
        address candidate;
        address adminKey;
        uint32 setId;
        uint8 reason;
        address standbyKey;
        uint64 signerEpoch;
        uint64 headSeq;
        bytes32 headLink;
        uint64 checkpointSeq;
        bytes32 checkpointRoot;
        uint64 at;
    }

    /// @notice An assignment's release and generation, from when it was made or its member moved.
    struct Revision {
        bytes32 release;
        uint64 generation;
    }

    /// @notice A candidate's standby key, vouched for `release`, registered at `at`.
    struct Standby {
        address key;
        bytes32 release;
        uint64 at;
    }

    /// @notice How many positions a draw tries before it refuses the turn.
    uint16 public constant MAX_DRAW_POSITIONS = 64;

    uint8 internal constant DRAWN = 0;
    uint8 internal constant PROMOTED = 2;

    /// @dev The longest detection the liveness bounds allow, `(heartbeatMisses + 1) * heartbeatInterval
    /// + headTolerance`, in seconds; and the most blocks `arbBlockHash` reaches back.
    uint256 private constant MAX_DETECTION = 600;
    uint256 private constant MAX_HEAD_BLOCKS = 256;

    bytes32 private constant VOUCH_DOMAIN = keccak256("engine.standby-vouch.v1");

    /// @notice The registry whose closes adopt sets and whose listing a drawn member needs.
    OperatorRegistry public immutable registry;

    /// @notice The log heads whose latest head per signer epoch restarts the miss count.
    LogHeads public immutable logHeads;

    /// @notice The handover's asserted switch budget, in seconds, which the miss window exceeds.
    uint64 public immutable switchBudget;

    /// @notice The pinned Arbitrum block time, in milliseconds, at which `headBlocks` spans less
    /// than one interval.
    uint64 public immutable blockMillis;

    /// @notice The heartbeat rule in force, and the bounds the timelock changes it within, fixed at
    /// deployment.
    Liveness public liveness;
    Liveness public livenessMinimums;
    Liveness public livenessMaximums;

    /// @notice Each witnessed slot's mark. A mark counts for slot `k` only while `at / heartbeatInterval
    /// == k`, so one made under another interval never counts.
    mapping(uint64 slot => Mark) public marks;

    /// @notice The slot the latest witness marked.
    uint64 public latestMarkedSlot;

    /// @notice Whether `candidate` has held an assignment of `turn`.
    mapping(uint64 turn => mapping(address candidate => bool held)) public heldTurn;

    /// @notice Each candidate's standby key record; deleted when a promotion installs it.
    mapping(address candidate => Standby) public standbyOf;

    /// @notice The candidate each standby key was first registered for; never cleared, so no key
    /// registers twice.
    mapping(address key => address candidate) public standbyHolder;

    /// @notice The receiver whose seeds turns are drawn from.
    SeedReceiver public seedReceiver;

    /// @notice Rotation sets adopted so far; set ids count from 1.
    uint32 public setCount;

    /// @notice The last turn drawn, zero before any; it only increases.
    uint64 public lastDrawnTurn;

    /// @notice The last drawn turn's seed when the receiver was last replaced, which the fallback
    /// hashes; zero while none had been drawn, when the fallback refuses.
    bytes32 public seedAnchor;

    mapping(uint32 setId => RotationSet) private sets;
    mapping(uint64 turn => Turn) private turns;
    mapping(uint64 turn => mapping(uint16 position => Assignment)) private assignments;
    mapping(uint64 turn => mapping(uint16 position => Revision[])) private revisions;

    event RotationSetAdopted(
        uint32 indexed setId,
        uint64 indexed epoch,
        uint64 effectiveAt,
        bool quorum,
        address[] candidates,
        address[] adminKeys,
        uint256[] weights
    );
    /// @notice Turn `turn` was assigned from set `setId` at `position`, for `reason` (0 a draw, 2 a
    /// promotion), naming `candidate` with the `adminKey` the set adopted and the `release` it
    /// registered then, under `generation`.
    event TurnAssigned(
        uint64 indexed turn,
        uint32 indexed setId,
        uint16 position,
        uint8 reason,
        address candidate,
        address adminKey,
        bytes32 release,
        uint64 generation
    );
    /// @notice Turn `turn`'s member moved its release, and the turn holds `generation` from now on.
    event TurnGenerationMoved(uint64 indexed turn, uint64 generation);
    event SeedReceiverChanged(address indexed receiver);
    event LivenessChanged(Liveness liveness);
    event SlotWitnessed(uint64 indexed slot, uint64 blockNumber);
    /// @notice The vault's signer moved to `candidate`'s `standbyKey` in `signerEpoch`, as turn
    /// `turn`'s assignment at `position`, fixing the outgoing epoch's latest head and the vault's
    /// latest accepted checkpoint.
    event Promoted(
        uint64 indexed turn,
        uint16 position,
        address indexed candidate,
        address standbyKey,
        uint64 signerEpoch,
        uint64 headSeq,
        bytes32 headLink,
        uint64 checkpointSeq,
        bytes32 checkpointRoot
    );
    event StandbyRegistered(address indexed candidate, address indexed key, bytes32 indexed release, address voucher);
    event StandbyConsumed(address indexed candidate, address indexed key, uint64 signerEpoch);

    error UnauthorizedRegistry();
    error UnauthorizedGovernance();
    error ZeroAddress();
    error RotationOff();
    error NotDrawable(uint64 turn, uint64 opensAt, uint64 closesAt);
    error AlreadyDrawn(uint64 turn);
    error NoSetInEffect();
    error NoSeed(uint64 turn);
    error NoAvailableMember(uint64 turn);
    error EmptySet(uint32 setId);
    error InvalidLiveness();
    error TurnNamesTheOperator();
    error NoTurn();
    error PromotionNotDue(uint64 dueAt);
    error NoQualifiedPosition(uint64 turn);
    error NotRegistered();
    error InvalidStandbyKey(address key);
    error VouchExpired(uint64 expiry);
    error ReleaseNotApproved(bytes32 release);
    error InvalidVouch(address voucher);

    /// @param liveness_ The heartbeat rule in force, within `minimums_` and `maximums_`.
    /// @param switchBudget_ The handover's asserted switch budget, in seconds.
    /// @param blockMillis_ The pinned Arbitrum block time, in milliseconds.
    constructor(
        OperatorRegistry registry_,
        SeedReceiver receiver_,
        LogHeads logHeads_,
        Liveness memory liveness_,
        Liveness memory minimums_,
        Liveness memory maximums_,
        uint64 switchBudget_,
        uint64 blockMillis_
    ) SeedSchedule(receiver_.beaconGenesisTime()) {
        registry = registry_;
        seedReceiver = receiver_;
        logHeads = logHeads_;
        switchBudget = switchBudget_;
        blockMillis = blockMillis_;
        livenessMinimums = minimums_;
        livenessMaximums = maximums_;
        _checkLiveness(liveness_);
        liveness = liveness_;
    }

    modifier onlyRegistry() {
        if (msg.sender != address(registry)) revert UnauthorizedRegistry();
        _;
    }

    /// @notice Replaces the seed receiver for every turn drawn from now on: the registry's timelock
    /// names a new one when an Ethereum fork needs a new relay. Until the new receiver has a seed
    /// relayed, turns draw from the fallback over the last drawn turn's seed, which this call fixes as
    /// `seedAnchor`.
    function setSeedReceiver(SeedReceiver receiver) external {
        if (msg.sender != address(registry.governance())) revert UnauthorizedGovernance();
        if (address(receiver).code.length == 0) revert ZeroAddress();
        seedReceiver = receiver;
        seedAnchor = turns[lastDrawnTurn].seed;
        emit SeedReceiverChanged(address(receiver));
    }

    /// @notice Adopts the rotation set a close of `epoch` makes, to take effect at `effectiveAt`, or
    /// when the latest adopted set does if that is later, so sets take effect in the order adopted.
    /// `candidates` are the registry's eligible candidates, with their admin keys and votes. With
    /// `quorum` the set is those with votes; without, the latest adopted set less members not among
    /// them under the key that set adopted. An empty set is adopted only after a set with members.
    function adopt(
        uint64 epoch,
        uint64 effectiveAt,
        address[] calldata candidates,
        address[] calldata adminKeys,
        uint256[] calldata weights,
        bool quorum
    ) external onlyRegistry {
        (address[] memory members, address[] memory keys, uint256[] memory kept) = quorum
            ? _withVotes(candidates, adminKeys, weights)
            : _stillEligible(candidates, adminKeys);
        RotationSet storage latest = sets[setCount];
        if (members.length == 0 && latest.members.length == 0) return;
        if (effectiveAt < latest.effectiveAt) effectiveAt = latest.effectiveAt;
        _record(epoch, effectiveAt, quorum, members, keys, kept);
    }

    /// @notice Draws turn `turn` from the set in effect at its draw deadline: anyone may call from the
    /// deadline until the turn's boundary, while the registry's rotation switch is on.
    /// @return candidate The member drawn.
    function draw(uint64 turn) external returns (address candidate) {
        if (!registry.rotationSwitch()) revert RotationOff();
        uint64 opensAt = drawDeadline(turn);
        if (block.timestamp < opensAt || block.timestamp >= boundary(turn)) {
            revert NotDrawable(turn, opensAt, boundary(turn));
        }
        if (turns[turn].candidate != address(0)) revert AlreadyDrawn(turn);
        uint32 setId = setAt(opensAt);
        if (setId == 0) revert NoSetInEffect();
        if (sets[setId].members.length == 0) revert EmptySet(setId);
        bytes32 seed = _seed(turn);
        (uint16 position, Member memory member) = _pick(setId, seed, turn);
        candidate = member.candidate;
        bytes32 release = registry.candidateOf(candidate).release;
        OperatorRegistry.Operator memory named = registry.namedOperator();
        uint64 generation = _generation(candidate, release, named);
        turns[turn] = Turn(candidate, position, DRAWN, setId, generation, lastDrawnTurn, seed, named.candidate);
        lastDrawnTurn = turn;
        _assign(turn, position, _drawn(member, setId), release, generation);
    }

    /// @notice Gives the turn in effect, and the last drawn turn, a new generation where `candidate` is
    /// their latest assignment's member, recording it as a revision of that assignment with the
    /// candidate's new release: the registry calls it when the candidate's release moves. Both share
    /// one.
    function releaseMoved(address candidate) external onlyRegistry {
        (uint64 inEffect, Turn memory current) = turnAt(uint64(block.timestamp));
        uint64 last = lastDrawnTurn;
        bytes32 release = registry.candidateOf(candidate).release;
        uint64 generation = 0;
        if (inEffect != 0 && current.candidate == candidate) {
            generation = _reserve();
            _move(inEffect, generation, release);
        }
        if (last != inEffect && turns[last].candidate == candidate) {
            if (generation == 0) generation = _reserve();
            _move(last, generation, release);
        }
    }

    /// @notice Sets the heartbeat rule, within the deployment's bounds: only the registry's timelock,
    /// and only while no turn names the operator, so no count runs across a change.
    function setLiveness(Liveness calldata liveness_) external {
        if (msg.sender != address(registry.governance())) revert UnauthorizedGovernance();
        if (registry.operatorGeneration() != 0) revert TurnNamesTheOperator();
        _checkLiveness(liveness_);
        liveness = liveness_;
        emit LivenessChanged(liveness_);
    }

    /// @notice Marks the current slot, once, with its block time and Arbitrum block: anyone may call
    /// while a turn names the operator. No other slot is ever marked.
    /// @return marked False when the slot was already marked, which changes nothing.
    function witness() external returns (bool marked) {
        if (registry.operatorGeneration() == 0) revert NoTurn();
        uint64 interval = liveness.heartbeatInterval;
        uint64 slot = uint64(block.timestamp) / interval;
        if (_marked(slot, interval)) return false;
        uint64 blockNumber = uint64(ARB_SYS.arbBlockNumber());
        marks[slot] = Mark(uint64(block.timestamp), blockNumber);
        latestMarkedSlot = slot;
        emit SlotWitnessed(slot, blockNumber);
        return true;
    }

    /// @notice The slot the latest witness marked and the Arbitrum block that marked it; zeros
    /// before any.
    function latestMark() external view returns (uint64 slot, uint64 blockNumber) {
        slot = latestMarkedSlot;
        blockNumber = marks[slot].blockNumber;
    }

    /// @notice Moves the vault's signer to the standby key of the next member in the turn's order,
    /// once the operator has missed `heartbeatMisses` witnessed slots: anyone may call. The member is
    /// the first after the current assignment's position, in `memberAt`'s order, that the registry
    /// lists under the admin key its set adopted, that holds a promotable standby key, and that has
    /// held no assignment of the turn. The promotion becomes the turn's assignment in a new
    /// generation, fixing the outgoing epoch's latest head and the vault's latest accepted
    /// checkpoint, and consumes the standby record. Refused while no turn names the operator
    /// (`NoTurn`), before `promotionDueAt` (`PromotionNotDue`), when no position qualifies
    /// (`NoQualifiedPosition`), and by the vault once its exit has started or while the guardian has
    /// retired the signer.
    /// @return turn The turn promoted in.
    /// @return position The promoted assignment's position.
    function promote() external returns (uint64 turn, uint16 position) {
        if (registry.operatorGeneration() == 0) revert NoTurn();
        Vault vault = _vault();
        uint64 epoch = vault.signerEpoch();
        uint64 dueAt = _dueAt(vault, epoch);
        if (block.timestamp < dueAt) revert PromotionNotDue(dueAt);
        (turn,) = turnAt(uint64(block.timestamp));
        Turn storage named = turns[turn];
        Member memory member;
        address key;
        (position, member, key) = _nextQualified(turn, named);
        Assignment memory promoted = _promoted(vault, epoch, member, named.setId, key);
        uint64 generation = vault.reserveGeneration();
        (named.candidate, named.position, named.reason, named.generation) =
        (member.candidate, position, PROMOTED, generation);
        _assign(turn, position, promoted, standbyOf[member.candidate].release, generation);
        delete standbyOf[member.candidate];
        emit StandbyConsumed(member.candidate, key, promoted.signerEpoch);
        emit Promoted(
            turn,
            position,
            member.candidate,
            key,
            promoted.signerEpoch,
            promoted.headSeq,
            promoted.headLink,
            promoted.checkpointSeq,
            promoted.checkpointRoot
        );
        vault.promoteSigner(key);
    }

    /// @notice The earliest block time at which `promote`'s count could pass, from the heads, the
    /// signer change and the marks on chain now, assuming every later slot is marked: with `s` the
    /// later of the slots of the signer epoch's latest head and of the signer change, and `u` the
    /// latest unmarked complete slot after `s` among the `heartbeatMisses` before the current one,
    /// `(max(s, u) + heartbeatMisses + 1) * heartbeatInterval`. At or before now while the count
    /// passes; `type(uint64).max` while no turn names the operator.
    function promotionDueAt() external view returns (uint64) {
        if (registry.operatorGeneration() == 0) return type(uint64).max;
        Vault vault = _vault();
        return _dueAt(vault, vault.signerEpoch());
    }

    /// @notice The digest a vouching key signs, with no message prefix, for `candidate`'s standby
    /// `key` on `release`, bound to this chain, the vault, the registry and this schedule.
    function vouchDigest(address candidate, address key, bytes32 release, uint64 nonce, uint64 expiry)
        public
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                VOUCH_DOMAIN,
                block.chainid,
                address(_vault()),
                address(registry),
                address(this),
                candidate,
                key,
                release,
                nonce,
                expiry
            )
        );
    }

    /// @notice Registers `key` as the caller's standby key for its current release, replacing and
    /// voiding any earlier one. The caller is a registered candidate whose release the timelock
    /// approved; `key` was never registered and never the vault's signer; `vouch` signs
    /// `vouchDigest` until `expiry`, by the vault's current signer or any candidate's standby key that
    /// is promotable now, so the operator in effect cannot keep a candidate out.
    function registerStandby(address key, uint64 nonce, uint64 expiry, bytes calldata vouch) external {
        OperatorRegistry.Candidate memory registered = registry.candidateOf(msg.sender);
        if (registered.adminKey == address(0)) revert NotRegistered();
        Vault vault = _vault();
        if (key == address(0) || standbyHolder[key] != address(0) || vault.wasSigner(key)) {
            revert InvalidStandbyKey(key);
        }
        if (expiry < block.timestamp) revert VouchExpired(expiry);
        bytes32 release = registered.release;
        if (!_approved(msg.sender, release)) revert ReleaseNotApproved(release);
        (address voucher,,) = ECDSA.tryRecover(vouchDigest(msg.sender, key, release, nonce, expiry), vouch);
        if (
            voucher == address(0)
                || (voucher != vault.checkpointSigner() && promotable(standbyHolder[voucher]) != voucher)
        ) revert InvalidVouch(voucher);
        standbyOf[msg.sender] = Standby(key, release, uint64(block.timestamp));
        standbyHolder[key] = msg.sender;
        emit StandbyRegistered(msg.sender, key, release, voucher);
    }

    /// @notice `candidate`'s standby key while a promotion may install it, zero otherwise: the
    /// candidate still runs the release the key was registered for, that release is still approved
    /// for it, no promotion consumed the record, and the key was never the vault's signer.
    function promotable(address candidate) public view returns (address key) {
        Standby storage record = standbyOf[candidate];
        key = record.key;
        if (key == address(0)) return key;
        bytes32 release = record.release;
        if (
            registry.candidateOf(candidate).release != release || !_approved(candidate, release)
                || _vault().wasSigner(key)
        ) return address(0);
    }

    /// @notice Turn `turn`'s assignment at `position`, and its releases and generations in order.
    function assignmentOf(uint64 turn, uint16 position) external view returns (Assignment memory, Revision[] memory) {
        return (assignments[turn][position], revisions[turn][position]);
    }

    /// @notice The member position `position` of `turn`'s draw from set `setId` picks with `seed`.
    /// Below `MAX_DRAW_POSITIONS`, the first whose cumulative weight exceeds
    /// `keccak256(abi.encode(seed, turn, position))` modulo the set's total weight; from it on, the
    /// scan's: the member `position - MAX_DRAW_POSITIONS` places after the one at index
    /// `keccak256(abi.encode(seed, turn, MAX_DRAW_POSITIONS))` modulo the member count, wrapping.
    function memberAt(uint32 setId, bytes32 seed, uint64 turn, uint16 position) public view returns (address) {
        Member[] storage members = sets[setId].members;
        if (members.length == 0) revert EmptySet(setId);
        return members[_index(members, seed, turn, position)].candidate;
    }

    /// @notice The set in effect at `timestamp`, zero before the first takes effect.
    function setAt(uint64 timestamp) public view returns (uint32 setId) {
        setId = setCount;
        while (setId != 0 && sets[setId].effectiveAt > timestamp) {
            --setId;
        }
    }

    /// @notice The set in effect now, zero before the first takes effect.
    function currentSetId() public view returns (uint32) {
        return setAt(uint64(block.timestamp));
    }

    /// @notice The next set to take effect, the earliest of those waiting, zero when none waits.
    function pendingSetId() external view returns (uint32) {
        uint32 next = currentSetId() + 1;
        return next <= setCount ? next : 0;
    }

    /// @notice Set `setId`'s close epoch, effective time, members and their weights.
    function setOf(uint32 setId)
        external
        view
        returns (uint64 epoch, uint64 effectiveAt, address[] memory candidates, uint256[] memory weights)
    {
        RotationSet storage set = sets[setId];
        uint256 count = set.members.length;
        candidates = new address[](count);
        weights = new uint256[](count);
        uint256 below = 0;
        for (uint256 i = 0; i < count; ++i) {
            Member memory member = set.members[i];
            (candidates[i], weights[i], below) = (member.candidate, member.cumulative - below, member.cumulative);
        }
        return (set.epoch, set.effectiveAt, candidates, weights);
    }

    /// @notice Turn `turn` as drawn; zero when it was not.
    function turnOf(uint64 turn) external view returns (Turn memory) {
        return turns[turn];
    }

    /// @notice The latest drawn turn whose boundary is at or before `timestamp`, zero when none is.
    function turnAt(uint64 timestamp) public view returns (uint64 turn, Turn memory) {
        turn = lastDrawnTurn;
        while (turn != 0 && boundary(turn) > timestamp) {
            turn = turns[turn].prior;
        }
        return (turn, turns[turn]);
    }

    /// @notice The member `turnAt(timestamp)` names, its generation, and its turn's start and end;
    /// zeros when no turn has started by then.
    function namedAt(uint64 timestamp)
        external
        view
        returns (address candidate, uint64 generation, uint64 since, uint64 until)
    {
        (uint64 turn, Turn memory named) = turnAt(timestamp);
        if (turn == 0) return (address(0), 0, 0, 0);
        since = boundary(turn);
        return (named.candidate, named.generation, since, since + 1 days);
    }

    /// @notice When the last drawn turn ends, zero before any is drawn.
    function turnsRunUntil() external view returns (uint64) {
        uint64 last = lastDrawnTurn;
        return last == 0 ? 0 : boundary(last) + 1 days;
    }

    /// @notice Whether `candidate` has held any assignment of the turn in effect or of the last drawn
    /// turn, or is the turn in effect's source, which hands its member the keys.
    function holdsTurn(address candidate) external view returns (bool) {
        (uint64 inEffect, Turn memory current) = turnAt(uint64(block.timestamp));
        return heldTurn[inEffect][candidate] || heldTurn[lastDrawnTurn][candidate] || candidate == current.source;
    }

    /// @notice The operator the registry named when `turn` was drawn, which hands its member the keys;
    /// zero when it was not drawn.
    function sourceOf(uint64 turn) external view returns (address) {
        return turns[turn].source;
    }

    /// @dev Stores the next set and reports it.
    function _record(
        uint64 epoch,
        uint64 effectiveAt,
        bool quorum,
        address[] memory members,
        address[] memory keys,
        uint256[] memory weights
    ) private {
        uint32 setId = ++setCount;
        RotationSet storage set = sets[setId];
        (set.epoch, set.effectiveAt) = (epoch, effectiveAt);
        uint256 cumulative = 0;
        for (uint256 i = 0; i < members.length; ++i) {
            cumulative += weights[i];
            set.members.push(Member(members[i], keys[i], cumulative));
        }
        emit RotationSetAdopted(setId, epoch, effectiveAt, quorum, members, keys, weights);
    }

    /// @dev The eligible candidates with votes, their admin keys and votes.
    function _withVotes(address[] calldata candidates, address[] calldata adminKeys, uint256[] calldata weights)
        private
        pure
        returns (address[] memory members, address[] memory keys, uint256[] memory kept)
    {
        (members, keys, kept) = _arrays(candidates.length);
        uint256 count = 0;
        for (uint256 i = 0; i < candidates.length; ++i) {
            if (weights[i] == 0) continue;
            (members[count], keys[count], kept[count]) = (candidates[i], adminKeys[i], weights[i]);
            ++count;
        }
        _trim(members, keys, kept, count);
    }

    /// @dev The members of the latest adopted set still among the eligible `candidates` under the
    /// admin key that set adopted, with their weights in it.
    function _stillEligible(address[] calldata candidates, address[] calldata adminKeys)
        private
        view
        returns (address[] memory members, address[] memory keys, uint256[] memory kept)
    {
        (members, keys, kept) = _arrays(candidates.length);
        Member[] storage inEffect = sets[setCount].members;
        uint256 count = 0;
        uint256 below = 0;
        for (uint256 j = 0; j < inEffect.length; ++j) {
            Member memory member = inEffect[j];
            for (uint256 i = 0; i < candidates.length; ++i) {
                if (candidates[i] != member.candidate) continue;
                // A re-registration under another admin key leaves the set.
                if (adminKeys[i] == member.adminKey) {
                    (members[count], keys[count], kept[count]) =
                    (member.candidate, member.adminKey, member.cumulative - below);
                    ++count;
                }
                break;
            }
            below = member.cumulative;
        }
        _trim(members, keys, kept, count);
    }

    function _arrays(uint256 length) private pure returns (address[] memory, address[] memory, uint256[] memory) {
        return (new address[](length), new address[](length), new uint256[](length));
    }

    /// @dev Shortens the three arrays to their first `count` entries.
    function _trim(address[] memory members, address[] memory keys, uint256[] memory kept, uint256 count) private pure {
        assembly ("memory-safe") {
            mstore(members, count)
            mstore(keys, count)
            mstore(kept, count)
        }
    }

    /// @dev Turn `turn`'s final seed: the receiver's, or, only when it refuses with `NoRelayedSeed`,
    /// the hash of `seedAnchor` with `turn`.
    function _seed(uint64 turn) private view returns (bytes32) {
        try seedReceiver.seedOf(turn) returns (bytes32 seed) {
            return seed;
        } catch (bytes memory reason) {
            if (bytes4(reason) != SeedReceiver.NoRelayedSeed.selector) {
                assembly ("memory-safe") {
                    revert(add(reason, 32), mload(reason))
                }
            }
        }
        bytes32 anchor = seedAnchor;
        if (anchor == 0) revert NoSeed(turn);
        return keccak256(abi.encode(anchor, turn));
    }

    /// @dev The first position whose member the registry lists under the admin key the set adopted:
    /// the weighted positions, then the scan's, one for each member.
    function _pick(uint32 setId, bytes32 seed, uint64 turn)
        private
        view
        returns (uint16 position, Member memory member)
    {
        Member[] storage members = sets[setId].members;
        uint256 end = MAX_DRAW_POSITIONS + members.length;
        for (; position < end; ++position) {
            member = members[_index(members, seed, turn, position)];
            if (_listed(member)) return (position, member);
        }
        revert NoAvailableMember(turn);
    }

    /// @dev The first position after the turn's current assignment, in `_pick`'s order, whose member
    /// the registry lists under the admin key the set adopted, holds a promotable standby key, and
    /// has held no assignment of the turn; with that key.
    function _nextQualified(uint64 turn, Turn storage named)
        private
        view
        returns (uint16 position, Member memory member, address key)
    {
        Member[] storage members = sets[named.setId].members;
        bytes32 seed = named.seed;
        uint256 end = MAX_DRAW_POSITIONS + members.length;
        for (position = named.position + 1; position < end; ++position) {
            member = members[_index(members, seed, turn, position)];
            if (heldTurn[turn][member.candidate] || !_listed(member)) continue;
            key = promotable(member.candidate);
            if (key != address(0)) return (position, member, key);
        }
        revert NoQualifiedPosition(turn);
    }

    /// @dev Whether the registry lists `member` under the admin key its set adopted.
    function _listed(Member memory member) private view returns (bool) {
        return registry.isListed(member.candidate) && registry.keyHolder(member.adminKey) == member.candidate;
    }

    /// @dev A draw's assignment of `member` from set `setId`.
    function _drawn(Member memory member, uint32 setId) private view returns (Assignment memory drawn) {
        (drawn.candidate, drawn.adminKey, drawn.setId, drawn.at) =
        (member.candidate, member.adminKey, setId, uint64(block.timestamp));
    }

    /// @dev A promotion's assignment of `member` from set `setId` to `key`, in the epoch after
    /// `epoch`, fixing `epoch`'s latest head and the vault's latest accepted checkpoint.
    function _promoted(Vault vault, uint64 epoch, Member memory member, uint32 setId, address key)
        private
        view
        returns (Assignment memory promoted)
    {
        promoted = _drawn(member, setId);
        (promoted.reason, promoted.standbyKey, promoted.signerEpoch) = (PROMOTED, key, epoch + 1);
        (promoted.headSeq, promoted.headLink,,,) = logHeads.heads(epoch);
        (promoted.checkpointSeq, promoted.checkpointRoot) = (vault.latestSeq(), vault.latestRoot());
    }

    /// @dev Records `assignment` of `turn` at `position` with its first release and generation, and
    /// reports it.
    function _assign(uint64 turn, uint16 position, Assignment memory assignment, bytes32 release, uint64 generation)
        private
    {
        assignments[turn][position] = assignment;
        revisions[turn][position].push(Revision(release, generation));
        heldTurn[turn][assignment.candidate] = true;
        emit TurnAssigned(
            turn,
            assignment.setId,
            position,
            assignment.reason,
            assignment.candidate,
            assignment.adminKey,
            release,
            generation
        );
    }

    /// @dev The time from which a promotion's count passes, as `promotionDueAt` states, for signer
    /// epoch `epoch`: it scans at most `heartbeatMisses` slots.
    function _dueAt(Vault vault, uint64 epoch) private view returns (uint64) {
        Liveness memory rule = liveness;
        uint64 interval = rule.heartbeatInterval;
        uint64 misses = rule.heartbeatMisses;
        uint64 since = vault.signerSince() / interval;
        (,,,, uint64 postedAt) = logHeads.heads(epoch);
        if (postedAt / interval > since) since = postedAt / interval;
        uint64 current = uint64(block.timestamp) / interval;
        for (uint64 slot = current - 1; slot > since && slot + misses >= current; --slot) {
            if (!_marked(slot, interval)) {
                since = slot;
                break;
            }
        }
        return (since + misses + 1) * interval;
    }

    /// @dev Whether a witness marked `slot` under `interval`.
    function _marked(uint64 slot, uint64 interval) private view returns (bool) {
        uint64 markedAt = marks[slot].at;
        return markedAt != 0 && markedAt / interval == slot;
    }

    /// @dev Reverts unless each of `rule`'s values is within the deployment's bounds and together they
    /// keep detection within ten minutes and above the switch budget, the tolerance under one
    /// interval, and the held-head block window within `arbBlockHash`'s reach and under one interval.
    function _checkLiveness(Liveness memory rule) private view {
        Liveness memory lo = livenessMinimums;
        Liveness memory hi = livenessMaximums;
        uint256 interval = rule.heartbeatInterval;
        uint256 misses = rule.heartbeatMisses;
        if (
            rule.heartbeatInterval < lo.heartbeatInterval || rule.heartbeatInterval > hi.heartbeatInterval
                || rule.headTolerance < lo.headTolerance || rule.headTolerance > hi.headTolerance
                || rule.heartbeatMisses < lo.heartbeatMisses || rule.heartbeatMisses > hi.heartbeatMisses
                || rule.headBlocks < lo.headBlocks || rule.headBlocks > hi.headBlocks || misses < 2
                || (misses + 1) * interval + rule.headTolerance > MAX_DETECTION || rule.headTolerance >= interval
                || rule.headBlocks > MAX_HEAD_BLOCKS || uint256(rule.headBlocks) * blockMillis >= interval * 1000
                || misses * interval <= switchBudget
        ) revert InvalidLiveness();
    }

    function _approved(address candidate, bytes32 release) private view returns (bool) {
        return registry.releaseApproved(keccak256(abi.encode(candidate, release)));
    }

    function _vault() private view returns (Vault) {
        return Vault(address(registry.vault()));
    }

    /// @dev The index of the member `position` picks from `members`, which must not be empty: for a
    /// weighted position the first whose cumulative weight exceeds the position's point, for a scan
    /// position the scan's start plus its place in the scan.
    function _index(Member[] storage members, bytes32 seed, uint64 turn, uint16 position)
        private
        view
        returns (uint256 low)
    {
        uint256 count = members.length;
        if (position >= MAX_DRAW_POSITIONS) {
            uint256 start = uint256(keccak256(abi.encode(seed, turn, MAX_DRAW_POSITIONS))) % count;
            return (start + position - MAX_DRAW_POSITIONS) % count;
        }
        uint256 high = count - 1;
        uint256 r = uint256(keccak256(abi.encode(seed, turn, position))) % members[high].cumulative;
        while (low < high) {
            uint256 mid = (low + high) / 2;
            if (members[mid].cumulative > r) high = mid;
            else low = mid + 1;
        }
    }

    /// @dev The generation the drawn member's turn holds: the vault's in effect when the registry
    /// names the member now on `release` and the vault has folded that release, so its holding
    /// continues; a new one otherwise.
    function _generation(address candidate, bytes32 release, OperatorRegistry.Operator memory named)
        private
        returns (uint64)
    {
        Vault vault = _vault();
        if (candidate == named.candidate && release == named.release && vault.syncedRelease() == release) {
            return vault.releaseGeneration();
        }
        return vault.reserveGeneration();
    }

    function _reserve() private returns (uint64) {
        return _vault().reserveGeneration();
    }

    /// @dev Gives `turn` `generation`, appending it with `release` to its latest assignment's revisions.
    function _move(uint64 turn, uint64 generation, bytes32 release) private {
        Turn storage moved = turns[turn];
        moved.generation = generation;
        revisions[turn][moved.position].push(Revision(release, generation));
        emit TurnGenerationMoved(turn, generation);
    }
}
