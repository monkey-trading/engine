// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ARB_SYS} from "./ArbSys.sol";
import {LogHeads} from "./LogHeads.sol";
import {OperatorRegistry} from "./OperatorRegistry.sol";
import {SeedReceiver} from "./SeedReceiver.sol";
import {SeedSchedule} from "./SeedSchedule.sol";
import {Vault} from "./Vault.sol";

/// @title OperatorSchedule
/// @notice Names the operator by turns, a day long in production (`SeedSchedule`), for a registry that
/// names this schedule. Every election close while rotation is on adopts a rotation set, which takes
/// effect seven days plus the finality margin after its close, and no earlier than the set adopted
/// before it; sets waiting to take effect queue in close order. The registry chooses each set
/// (`OperatorRegistry._adopt`). A set holds each member's admin key as adopted. An empty set replaces
/// one with members, and no turn is drawn from it.
/// @notice Anyone draws turn `n` from its draw deadline, a turn before its boundary (with day-long
/// turns, 22:30 UTC on day `n`), until that boundary. The set, the one in effect at the deadline, is fixed
/// by then, and so is a relayed seed; a fallback seed is fixed by the block of the first fix from the
/// deadline, whose hash no caller knows when it sends it. So the moment of the draw picks neither. Position by position, the
/// turn's seed picks a member with probability proportional to its weight, with replacement, and the
/// first one the registry lists under the admin key the set adopted takes the turn; after 64 positions
/// a scan of every member, from an index the seed fixes, takes the first available, so a turn goes
/// undrawn only when no member is available. Availability is read at the draw: a deregistration or a
/// withdrawn approval landing in the window lets whoever draws choose by drawing before or after it. A
/// member the registry names at the draw, on the release it runs, keeps the generation the vault holds
/// for it; any other member, and a member whose release moves, takes a new one from the vault, so a
/// node never reopens keys sealed for an earlier holding.
/// @notice A turn names its member from its boundary, and the last turn drawn keeps naming its member
/// after it ends until another turn starts or the registry's timelock installs an operator. Turning
/// rotation off stops draws and adoptions; turns already drawn stand.
/// @notice Liveness: while a turn names the operator, its enclave posts `LogHeads` v2 heartbeats, and
/// anyone may `witness` each slot of `heartbeatInterval` seconds of block time. Once the
/// `heartbeatMisses` slots before the current one are all marked, all after the slot of the signer
/// epoch's latest head and of the signer change that started it, anyone may `promote`: the vault's
/// signer moves at once, in a new epoch, to the vouched standby key of the next member in the turn's
/// order that qualifies, and the promotion becomes the turn's assignment in a new generation.
/// @notice A turn's member takes it with its first log head under its assignment, which `LogHeads`
/// latches. Once `takeWindow` slots after the latest of the boundary's slot, the assignment's and the
/// signer change's are all marked with no latch for it, anyone may `skip` the turn to the next member in
/// its order, which the key holder hands the keys to, in a new generation and with no signer change.
/// Once `heartbeatMisses + 1` slots after the later of the boundary's slot and the signer change's are
/// all marked with no assignment of the turn latched, in a set of two or more members, `promote` may run
/// even while heads post. The rule and the window in force always leave the first skip's member a take
/// before that promotion (`_skipsFirst`).
/// @notice The key holder is the candidate whose enclave holds the signer epoch's keys, as
/// `LogHeads.keyHolder` names it from this schedule's turns: the candidate whose assignment, release move
/// or naming as a draw's source recorded the release that signed the epoch's newest head, when that head
/// was signed since `registryTurn` began, which no release move of its own changes. Otherwise no member has
/// taken the keys in the epoch since then, so they are where it found them: walking back from the turn in
/// effect over turns that no head took and no promotion reached, the member of the latest turn one did; in
/// a turn the epoch began in, the member the vault's timelock installed; else the source of
/// `registryTurn`, as the first turn and the first after an installation are. A skip never names the key
/// holder, and records a skipped member that holds the keys as having held them. A promotion names a member
/// that never held the turn's keys, by position, whether or not it held an assignment of the turn, since
/// whether an assignment's member took the keys is the key holder's choice, and names the key holder
/// last, so a holder that withholds the keys can neither choose whom a promotion names nor take the turn
/// itself. Only a member that held the keys under an assignment of the turn is never named again in it.
/// Skips and promotions search past the current position through the whole scan, continued from its start
/// past its end, so every member stays in reach and each assignment of a turn takes a later position.
/// @notice Every assignment of a turn, drawn, skipped to or promoted, is kept by its position with its
/// releases and generations, and `turns` follows the latest.
/// @dev Seeds come from `seedReceiver`, which the registry's timelock replaces when an Ethereum fork
/// needs a new relay and receiver. A turn with no relayed seed draws only once the receiver has fixed
/// the hash `h` of an Arbitrum block made from its deadline (`SeedReceiver.fixFallback`): the first
/// draw call fixes the block and draws no one, and a draw from a later block draws. A turn the
/// receiver has no seed for, because no seed was relayed at or before it, draws from
/// `keccak256(abi.encode(seedAnchor, turn, h))`, where `seedAnchor` is the last drawn turn's seed when
/// the receiver was replaced, so no later draw changes it; only that refusal falls back, and any other
/// failure of the read reverts the draw, so a caller cannot pick the fallback by limiting gas. Not
/// upgradeable.
contract OperatorSchedule is SeedSchedule {
    /// @notice A rotation set's member, its admin key as adopted, and the sum of the weights up to
    /// and including its own.
    struct Member {
        address candidate;
        address adminKey;
        uint256 cumulative;
    }

    /// @dev A candidate's latest carry with no promotable standby key: the set, and when the run began.
    struct Keyless {
        uint32 setId;
        uint64 since;
    }

    struct RotationSet {
        uint64 epoch;
        uint64 effectiveAt;
        Member[] members;
    }

    /// @notice A drawn turn. `candidate`, `position`, `reason` and `generation` are its latest
    /// assignment's; `setId`, `prior` (the turn drawn before it, zero for the first), `seed` and
    /// `source` (the operator the registry named at the draw, which hands the member the keys) are
    /// the draw's. `reason` is 0 for a draw, 1 for a skip and 2 for a promotion.
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

    /// @notice The take window: how many complete marked slots a turn's current assignment may go
    /// unlatched before anyone may skip it, and the bounds the timelock changes it within, fixed at
    /// deployment.
    struct TakeWindow {
        uint16 slots;
        uint16 minimum;
        uint16 maximum;
    }

    /// @notice How many positions a draw tries before it refuses the turn.
    uint16 public constant MAX_DRAW_POSITIONS = 64;

    uint8 internal constant DRAWN = 0;
    uint8 internal constant SKIPPED = 1;
    uint8 internal constant PROMOTED = 2;

    /// @dev `held`'s flags: an assignment of the turn, and the turn's keys.
    uint8 private constant ASSIGNED = 1;
    uint8 private constant KEYS = 2;

    /// @dev The longest detection the liveness bounds allow, `(heartbeatMisses + 1) * heartbeatInterval
    /// + headTolerance`, in seconds; and the most blocks `arbBlockHash` reaches back.
    uint256 private constant MAX_DETECTION = 600;
    uint256 private constant MAX_HEAD_BLOCKS = 256;
    /// @dev How long before a promotion falls due the operator's node stops signing (its fence), in
    /// seconds: a skip's member signs its first head before it. The node's `liveness::DELTA`; a change
    /// to either changes both.
    uint256 private constant STOP_MARGIN = 15;

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

    /// @notice The take window in force, and its bounds.
    TakeWindow public takeWindow;

    /// @notice Each witnessed slot's mark. A mark counts for slot `k` only while `at / heartbeatInterval
    /// == k`, so one made under another interval never counts.
    mapping(uint64 slot => Mark) public marks;

    /// @notice The slot the latest witness marked.
    uint64 public latestMarkedSlot;

    /// @dev What `candidate` held of `turn`: `ASSIGNED` once it held an assignment of it, and `KEYS` once
    /// it held the turn's keys as a promotion or skip found it: the member a promotion names, the key holder
    /// it moves past (`_promotion`), or a skipped member that held the keys (`skip`).
    mapping(uint64 turn => mapping(address candidate => uint8 held)) private held;

    /// @notice The candidate whose assignment or release move last recorded `release`, or the operator a
    /// draw found the registry naming on it; zero for a release never recorded. Only the timelock approves
    /// a release for a candidate, so no candidate moves another's entry.
    mapping(bytes32 release => address candidate) public releaseCandidate;

    /// @notice The latest turn drawn while the registry named its own operator, with no turn naming one:
    /// the first turn, or the first after an installation, whose source holds the keys from before it.
    /// A head posted before its boundary names no key holder, and the key holder walk stops at it.
    uint64 public registryTurn;

    /// @notice The key holder a skip in signer epoch `epoch` moved past while it held the keys under the
    /// skipped assignment: before the epoch's first head, the member whose registered enclave the vault's
    /// timelock installed in that turn before its take. Zero when no skip did.
    mapping(uint64 epoch => address candidate) public skippedHolder;

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
    /// @dev The last set that carried each candidate with no promotable standby key, and when the run of
    /// sets carrying it with none that it ends began.
    mapping(address candidate => Keyless) private keyless;
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
    /// @notice Turn `turn` was assigned from set `setId` at `position`, for `reason` (0 a draw, 1 a
    /// skip, 2 a promotion), naming `candidate` with the `adminKey` the set adopted and the `release` it
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
    event TakeWindowChanged(uint16 slots);
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
    error InvalidTakeWindow();
    error SkipNotDue(uint64 dueAt);

    /// @param liveness_ The heartbeat rule in force, within `minimums_` and `maximums_`.
    /// @param takeWindow_ The take window in force and its bounds, whose minimum exceeds the switch
    /// budget plus one interval plus a head's inclusion, which `headTolerance` bounds, at every interval
    /// and tolerance the heartbeat rule's bounds allow, and whose value leaves a skip's member a take
    /// before the untaken-turn promotion under `liveness_` (`_skipsFirst`): in full, a nonzero minimum
    /// with `(minimum - 1) * minimums_.heartbeatInterval > switchBudget_ + maximums_.headTolerance`,
    /// and a maximum spanning at most a turn at the shortest interval.
    /// @param switchBudget_ The handover's asserted switch budget, in seconds.
    /// @param blockMillis_ The pinned Arbitrum block time, in milliseconds.
    /// @dev Takes its beacon genesis time and turn length from `receiver_`.
    constructor(
        OperatorRegistry registry_,
        SeedReceiver receiver_,
        LogHeads logHeads_,
        Liveness memory liveness_,
        Liveness memory minimums_,
        Liveness memory maximums_,
        TakeWindow memory takeWindow_,
        uint64 switchBudget_,
        uint64 blockMillis_
    ) SeedSchedule(receiver_.beaconGenesisTime(), receiver_.turnLength()) {
        registry = registry_;
        seedReceiver = receiver_;
        logHeads = logHeads_;
        switchBudget = switchBudget_;
        blockMillis = blockMillis_;
        livenessMinimums = minimums_;
        livenessMaximums = maximums_;
        _checkLiveness(liveness_);
        liveness = liveness_;
        uint256 minimum = takeWindow_.minimum;
        if (
            minimum == 0 || takeWindow_.slots < minimum || takeWindow_.slots > takeWindow_.maximum
                || (minimum - 1) * minimums_.heartbeatInterval <= switchBudget_ + maximums_.headTolerance
                || uint256(takeWindow_.maximum) * minimums_.heartbeatInterval > turnLength
                || !_skipsFirst(liveness_, takeWindow_.slots)
        ) revert InvalidTakeWindow();
        takeWindow = takeWindow_;
    }

    modifier onlyRegistry() {
        if (msg.sender != address(registry)) revert UnauthorizedRegistry();
        _;
    }

    /// @notice Replaces the seed receiver for every turn drawn from now on: the registry's timelock
    /// names a new one when an Ethereum fork needs a new relay. Until the new receiver has a seed
    /// relayed, turns draw from the fallback over the last drawn turn's seed, which this call fixes as
    /// `seedAnchor`, and the block hash the new receiver fixes for each turn. The new receiver keeps the
    /// schedule's turn length.
    function setSeedReceiver(SeedReceiver receiver) external {
        if (msg.sender != address(registry.governance())) revert UnauthorizedGovernance();
        // A receiver without code, or numbering turns by another length, is refused.
        uint64 length = receiver.turnLength();
        if (length != turnLength) revert InvalidTurnLength(length);
        seedReceiver = receiver;
        seedAnchor = turns[lastDrawnTurn].seed;
        emit SeedReceiverChanged(address(receiver));
    }

    /// @notice Adopts the rotation set a close of `epoch` makes, as the registry chose it: `candidates`,
    /// each distinct, with the admin keys they hold and positive `weights`. It takes effect at
    /// `effectiveAt`, or when the latest adopted set does if that is later, so sets take effect in the
    /// order adopted. An empty set is adopted only after a set with members.
    /// @dev A set holds at most the registry's `CANDIDATE_LIMIT` plus one members, 101: it comes from the
    /// registry's list, which the named operator's place beyond the cap bounds.
    function adopt(
        uint64 epoch,
        uint64 effectiveAt,
        address[] calldata candidates,
        address[] calldata adminKeys,
        uint256[] calldata weights,
        bool quorum
    ) external onlyRegistry {
        RotationSet storage latest = sets[setCount];
        if (candidates.length == 0 && latest.members.length == 0) return;
        if (effectiveAt < latest.effectiveAt) effectiveAt = latest.effectiveAt;
        _record(epoch, effectiveAt, quorum, candidates, adminKeys, weights);
    }

    /// @notice Whether the set the registry is choosing carries `candidate`, a member of the latest set
    /// with no promotable standby key, recording the carry if so: below the quorum always, and with
    /// `quorum` until `epochLength` has passed since the unbroken run of sets carrying it with none began.
    /// The run ends once the member holds a promotable key again, by registering one (`registerStandby`)
    /// or moving to the release its key was registered for (`releaseMoved`), so a member that moves
    /// releases again after that keeps its place as long again.
    function carryWithoutKey(address candidate, bool quorum, uint64 epochLength) external onlyRegistry returns (bool) {
        uint32 latest = setCount;
        Keyless storage carried = keyless[candidate];
        uint64 since = carried.since;
        if (carried.setId != latest) since = uint64(block.timestamp);
        if (quorum && block.timestamp >= uint256(since) + epochLength) return false;
        (carried.setId, carried.since) = (latest + 1, since);
        return true;
    }

    /// @notice Ends `candidate`'s run of carries with no key (`carryWithoutKey`) when it holds a promotable
    /// standby key: the registry calls it when the candidate registers or the timelock approves the
    /// release it holds, either of which can make a key it kept promotable again, as a move does.
    function endKeylessRun(address candidate) public onlyRegistry {
        if (promotable(candidate) != address(0)) delete keyless[candidate];
    }

    /// @notice Draws turn `turn` from the set in effect at its draw deadline: anyone may call from the
    /// deadline until the turn's boundary, while the registry's rotation switch is on. For a turn with
    /// no relayed seed, a call before the receiver has fixed its fallback's block hash moves that on
    /// and draws no one: call again from a later block.
    /// @dev A position from `MAX_DRAW_POSITIONS` on is the scan's, at most 164 for a set of 101 members.
    /// @return candidate The member drawn, or zero when the call only moved the fallback on.
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
        if (!seedReceiver.fixFallback(turn)) return address(0);
        bytes32 seed = _seed(turn);
        (uint16 position, Member memory member) = _pick(setId, seed, turn);
        candidate = member.candidate;
        bytes32 release = _releaseOf(candidate);
        OperatorRegistry.Operator memory named = registry.namedOperator();
        // The source holds the keys under the release the registry names it on, which no assignment
        // may have recorded.
        releaseCandidate[named.release] = named.candidate;
        if (_noTurn()) registryTurn = turn;
        uint64 generation = _generation(candidate, release, named);
        turns[turn] = Turn(candidate, position, DRAWN, setId, generation, lastDrawnTurn, seed, named.candidate);
        lastDrawnTurn = turn;
        _assign(turn, position, _drawn(member, setId), release, generation);
    }

    /// @notice Records `candidate`'s new release as its own (`releaseCandidate`), whatever turn names it,
    /// and gives the turn in effect, and the last drawn turn, a new generation where `candidate` is
    /// their latest assignment's member, recording it as a revision of that assignment with the
    /// candidate's new release: the registry calls it when the candidate's release moves. Both share
    /// one. A move that leaves the candidate a promotable standby key, back to the release it was
    /// registered for, ends its run of carries with no key (`endKeylessRun`).
    function releaseMoved(address candidate) external onlyRegistry {
        endKeylessRun(candidate);
        (uint64 inEffect, Turn memory current) = turnAt(uint64(block.timestamp));
        uint64 last = lastDrawnTurn;
        bytes32 release = _releaseOf(candidate);
        releaseCandidate[release] = candidate;
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

    /// @notice Sets the heartbeat rule, within the deployment's bounds and leaving a skip's member a take
    /// before the untaken-turn promotion under the take window in force (`_skipsFirst`): only the
    /// registry's timelock, and only while no turn names the operator, so no count runs across a change.
    function setLiveness(Liveness memory liveness_) external {
        _governWithNoTurn();
        _checkLiveness(liveness_);
        if (!_skipsFirst(liveness_, takeWindow.slots)) revert InvalidLiveness();
        liveness = liveness_;
        emit LivenessChanged(liveness_);
    }

    /// @notice Sets the take window, within its bounds and leaving a skip's member a take before the
    /// untaken-turn promotion under the heartbeat rule in force (`_skipsFirst`): only the registry's
    /// timelock, and only while no turn names the operator.
    function setTakeWindow(uint16 slots) external {
        _governWithNoTurn();
        TakeWindow storage window = takeWindow;
        if (slots < window.minimum || slots > window.maximum || !_skipsFirst(liveness, slots)) {
            revert InvalidTakeWindow();
        }
        window.slots = slots;
        emit TakeWindowChanged(slots);
    }

    /// @notice Marks the current slot, once, with its block time and Arbitrum block: anyone may call
    /// while a turn names the operator. No other slot is ever marked.
    /// @return marked False when the slot was already marked, which changes nothing.
    function witness() external returns (bool marked) {
        if (_noTurn()) revert NoTurn();
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
    /// once the operator has missed `heartbeatMisses` witnessed slots, or no member has taken the turn
    /// in `heartbeatMisses + 1` (see `promotionDueAt`): anyone may call. The member is
    /// the first after the current assignment's position, in `memberAt`'s order through the whole scan,
    /// that the registry lists under the admin key its set adopted, that holds a promotable standby key,
    /// and that did not hold the turn's keys under an assignment of it, in this order of preference: one
    /// that never held the turn's keys and is not the key holder, whether it held no assignment of the
    /// turn or held one the key holder never handed them to; then the key holder, as `skip` defines it,
    /// holding no assignment of the turn, or a holder an earlier promotion of the turn moved past, so its
    /// own standby key fences its old key out only when no other member qualifies.
    /// The promotion records the key holder and the member it names as having held the turn's keys
    /// (`heldKeys`), becomes the turn's assignment in a new generation, fixing the outgoing epoch's
    /// latest head and the vault's latest accepted checkpoint, and consumes the standby record.
    /// Refused while no turn names the operator
    /// (`NoTurn`), before `promotionDueAt` (`PromotionNotDue`), when no position qualifies
    /// (`NoQualifiedPosition`), and by the vault once its exit has started or while the guardian has
    /// retired the signer.
    /// @dev On the missed-head path a head included before it in the same block restarts the count, so
    /// it reverts `PromotionNotDue`; one included after it is signed for the old epoch and fails.
    /// @return turn The turn promoted in.
    /// @return position The promoted assignment's position.
    function promote() external returns (uint64 turn, uint16 position) {
        if (_noTurn()) revert NoTurn();
        Vault vault = _vault();
        uint64 epoch = vault.signerEpoch();
        uint64 dueAt = _dueAt(vault, epoch);
        if (block.timestamp < dueAt) revert PromotionNotDue(dueAt);
        (turn,) = turnAt(uint64(block.timestamp));
        Turn storage named = turns[turn];
        Member memory member;
        address key;
        (address holder,) = logHeads.keyHolder();
        (position, member, key) = _promotion(turn, named, holder);
        Assignment memory promoted = _promoted(vault, epoch, member, named.setId, key);
        _reassign(turn, named, position, promoted, standbyOf[member.candidate].release);
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
    /// signer change, the latches and the marks on chain now, assuming every later slot is marked: the
    /// earlier of two paths. Missed heads: with `s` the later of the slots of the signer epoch's latest
    /// head and of the signer change, and `u` the latest unmarked complete slot after `s` among the
    /// `heartbeatMisses` before the current one, `(max(s, u) + heartbeatMisses + 1) * heartbeatInterval`.
    /// An untaken turn, while its current assignment is unlatched in a set of two or more members: the
    /// same with `s` the later of the slots of the turn's boundary and of the signer change, and
    /// `heartbeatMisses + 1` slots in place of `heartbeatMisses`, so the incumbent's heads, which bind its
    /// own earlier assignment, do not move it. Neither path reads who qualifies for the promotion, so no
    /// change to a listing, a release or a standby key moves the time earlier; with no member qualifying,
    /// `promote` is refused and the governance fallback applies. A head can latch only
    /// the current assignment, and a promotion past a latched one restarts both counts at its signer
    /// change, where the missed-head path passes first, so an unlatched current assignment stands for
    /// no assignment of the turn latched. At or before now while a count passes; `type(uint64).max`
    /// while no turn names the operator.
    function promotionDueAt() external view returns (uint64) {
        if (_noTurn()) return type(uint64).max;
        Vault vault = _vault();
        return _dueAt(vault, vault.signerEpoch());
    }

    /// @notice Moves the turn in effect from its current assignment, which no head has latched, to the
    /// next member in its order, once `skipDueAt` has passed: anyone may call. The member is the first
    /// after the current position, in `memberAt`'s order through the whole scan, that the registry lists
    /// under the admin key its set adopted, that has held no assignment of the turn, and that is not the
    /// key holder (`LogHeads.keyHolder`, as the contract's notice states). The key holder hands the skip's
    /// member the keys, in another generation; it needs no standby key, and the vault's signer stays. A
    /// skipped member that is the key holder withheld the keys under its own assignment: the skip records it
    /// as having held them (`heldKeys`). The skip
    /// becomes the turn's assignment in a new generation, and a turn drawn after it continues that
    /// generation when it names the same member, or takes a new one when it continued the skipped
    /// assignment's. Refused while no turn names the operator (`NoTurn`), before `skipDueAt`, which never
    /// passes once a head latched the current assignment, while it is a promotion, or while the guardian
    /// has retired the signer, when no head can latch (`SkipNotDue`), and
    /// when no position qualifies (`NoQualifiedPosition`). A head included before it in the same block
    /// latches the assignment, so the skip reverts; one included after binds a superseded assignment and
    /// latches nothing.
    /// @return turn The turn skipped in.
    /// @return position The skip's assignment's position.
    function skip() external returns (uint64 turn, uint16 position) {
        if (_noTurn()) revert NoTurn();
        uint64 dueAt;
        (turn, dueAt) = _skipDue();
        Turn storage named = turns[turn];
        if (block.timestamp < dueAt) revert SkipNotDue(dueAt);
        Member memory member;
        bool found;
        (address holder, uint64 epoch) = logHeads.keyHolder();
        (found, position, member,) = _next(turn, named, false, holder, 1);
        if (!found) revert NoQualifiedPosition(turn);
        // A skipped member whose enclave holds the keys withheld them under its own assignment.
        if (holder == named.candidate) {
            _recordKeys(turn, holder);
            skippedHolder[epoch] = holder;
        }
        Assignment memory skipped = _drawn(member, named.setId);
        skipped.reason = SKIPPED;
        _reassign(turn, named, position, skipped, _releaseOf(member.candidate));
    }

    /// @notice Whether `candidate` has held an assignment of `turn`.
    function heldTurn(uint64 turn, address candidate) external view returns (bool) {
        return held[turn][candidate] & ASSIGNED != 0;
    }

    /// @notice Whether `candidate` held `turn`'s keys as a promotion or skip in it found: the member a
    /// promotion named, the key holder one moved past, or a skipped member that held them.
    function heldKeys(uint64 turn, address candidate) external view returns (bool) {
        return held[turn][candidate] & KEYS != 0;
    }

    /// @notice The earliest block time at which `skip` could pass for the turn in effect's current
    /// assignment, assuming every later slot is marked: with `s` the latest of the slots of the turn's
    /// boundary, of the assignment and of the signer change, and `u` the latest unmarked complete slot
    /// after `s` among the `takeWindow` before the current one, `(max(s, u) + takeWindow + 1) *
    /// heartbeatInterval`, so an unmarked slot, as in a chain outage, or a signer the timelock installs
    /// gives the member a fresh window. `type(uint64).max` while no turn names the operator, once a head
    /// latched the assignment, while it is a promotion, whose member the missed-head count passes if it
    /// never beats, and while the guardian has retired the signer, when no head can latch.
    function skipDueAt() external view returns (uint64 dueAt) {
        if (_noTurn()) return type(uint64).max;
        (, dueAt) = _skipDue();
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
        address voucher = logHeads.recover(vouchDigest(msg.sender, key, release, nonce, expiry), vouch);
        if (
            voucher == address(0)
                || (voucher != vault.checkpointSigner() && promotable(standbyHolder[voucher]) != voucher)
        ) revert InvalidVouch(voucher);
        standbyOf[msg.sender] = Standby(key, release, uint64(block.timestamp));
        standbyHolder[key] = msg.sender;
        delete keyless[msg.sender];
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
        if (_releaseOf(candidate) != release || !_approved(candidate, release) || _vault().wasSigner(key)) {
            return address(0);
        }
    }

    /// @notice Turn `turn`'s assignment at `position`, and its releases and generations in order.
    function assignmentOf(uint64 turn, uint16 position) external view returns (Assignment memory, Revision[] memory) {
        return (assignments[turn][position], revisions[turn][position]);
    }

    /// @notice Turn `turn`'s assignment at `position`: its member, and its revision at `index`, zeros past
    /// the last, at a cost that does not grow with the revisions.
    function revisionOf(uint64 turn, uint16 position, uint256 index)
        external
        view
        returns (address candidate, Revision memory revision)
    {
        Revision[] storage list = revisions[turn][position];
        if (index < list.length) revision = list[index];
        candidate = assignments[turn][position].candidate;
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
    /// @dev Each close ends an epoch at least one epoch length after the last close's, and only a late
    /// close lets the next follow at once, so with epochs of at least a day at most
    /// `ceil((7 days + finality margin) / 1 day) + 1` sets wait at once, counting the largest margin in
    /// force over that time: 9 with a margin up to a day. Each change of the epoch length by the
    /// timelock can let one more close follow at once.
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

    /// @notice Member `index` of set `setId`, with the admin key it adopted and the weights up to its own;
    /// zero past the set's last member.
    function memberOf(uint32 setId, uint256 index) external view returns (Member memory member) {
        Member[] storage members = sets[setId].members;
        if (index < members.length) member = members[index];
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
        return (named.candidate, named.generation, since, boundary(turn + 1));
    }

    /// @notice When the last drawn turn ends, zero before any is drawn.
    function turnsRunUntil() external view returns (uint64) {
        uint64 last = lastDrawnTurn;
        return last == 0 ? 0 : boundary(last + 1);
    }

    /// @notice The operator the registry named when `turn` was drawn, which hands its member the keys;
    /// zero when it was not drawn. That is the member of the turn drawn before it, or, for the first
    /// turn and the first after an installation, the operator the registry named itself.
    function sourceOf(uint64 turn) external view returns (address) {
        return turns[turn].source;
    }

    /// @dev Stores the next set and reports it.
    function _record(
        uint64 epoch,
        uint64 effectiveAt,
        bool quorum,
        address[] calldata members,
        address[] calldata keys,
        uint256[] calldata weights
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

    /// @dev Turn `turn`'s final seed: the receiver's, or, only when it refuses with `NoRelayedSeed`,
    /// the hash of `seedAnchor` with `turn` and the block hash the receiver fixed for the turn.
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
        return keccak256(abi.encode(anchor, turn, seedReceiver.fallbackHash(turn)));
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
    /// the registry lists under the admin key the set adopted and whose rank is one of `ranks`, a bit per
    /// rank. A rank is the member's `held` flags plus two when it is `holder`, the candidate whose enclave
    /// holds the keys (`LogHeads.keyHolder`): 0 for a member that held nothing of the turn; 1 for one that held an
    /// assignment of it but not its keys; 2 for `holder` holding no assignment of the turn, or a holder a
    /// promotion moved past; and from 3 for a member that held the keys under an assignment of the turn,
    /// which no caller asks for. The search runs to one whole scan past the later of the weighted
    /// positions and the current one, a scan position past the scan's end continuing it from its start, so
    /// every member is in reach and each assignment of the turn takes a later position than the one before:
    /// with at most `2 * count + 1` assignments of a turn, each at most `MAX_DRAW_POSITIONS + count`
    /// positions on, positions fit a `uint16` for any set the registry adopts. With `standby`, for a
    /// promotion, the member holds a promotable standby key, returned with it. `found` is false when no
    /// position qualifies.
    function _next(uint64 turn, Turn storage named, bool standby, address holder, uint256 ranks)
        private
        view
        returns (bool found, uint16 position, Member memory member, address key)
    {
        Member[] storage members = sets[named.setId].members;
        position = named.position;
        uint256 last = (position < MAX_DRAW_POSITIONS ? MAX_DRAW_POSITIONS - 1 : position) + members.length;
        while (position < last) {
            member = members[_index(members, named.seed, turn, ++position)];
            address candidate = member.candidate;
            if ((ranks >> (held[turn][candidate] + (candidate == holder ? 2 : 0))) & 1 == 0 || !_listed(member)) {
                continue;
            }
            if (!standby) return (true, position, member, key);
            key = promotable(candidate);
            if (key != address(0)) return (true, position, member, key);
        }
    }

    /// @dev The member a promotion in `turn` names, with its position and promotable standby key: the
    /// first `_next` finds of ranks 0 and 1 together, a member that never held the turn's keys and is not
    /// `holder`, whether or not it held an assignment, since whether an assignment's member took the keys
    /// is the holder's choice; else of rank 2. Records it and `holder` as having held the turn's keys.
    /// Reverts `NoQualifiedPosition` when no position qualifies.
    function _promotion(uint64 turn, Turn storage named, address holder)
        private
        returns (uint16 position, Member memory member, address key)
    {
        bool found;
        (found, position, member, key) = _next(turn, named, true, holder, 3);
        if (!found) (found, position, member, key) = _next(turn, named, true, holder, 4);
        if (!found) revert NoQualifiedPosition(turn);
        _recordKeys(turn, holder);
        _recordKeys(turn, member.candidate);
    }

    /// @dev Records `candidate` as having held `turn`'s keys, unless it is zero, as the key holder is when
    /// nothing names one.
    function _recordKeys(uint64 turn, address candidate) private {
        if (candidate != address(0)) held[turn][candidate] |= KEYS;
    }

    /// @dev The release the registry records for `candidate`.
    function _releaseOf(address candidate) private view returns (bytes32) {
        return registry.candidateOf(candidate).release;
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

    /// @dev Makes `assignment` on `release` the turn's at `position`, in a new generation that a turn drawn
    /// after it continues or leaves as `_carryNext` states.
    function _reassign(uint64 turn, Turn storage named, uint16 position, Assignment memory assignment, bytes32 release)
        private
    {
        uint64 generation = _reserve();
        _carryNext(turn, assignment.candidate, generation, named.generation, release);
        (named.candidate, named.position, named.reason, named.generation) =
        (assignment.candidate, position, assignment.reason, generation);
        _assign(turn, position, assignment, release, generation);
    }

    /// @dev Records `assignment` of `turn` at `position` with its first release and generation, and
    /// reports it.
    function _assign(uint64 turn, uint16 position, Assignment memory assignment, bytes32 release, uint64 generation)
        private
    {
        assignments[turn][position] = assignment;
        revisions[turn][position].push(Revision(release, generation));
        held[turn][assignment.candidate] |= ASSIGNED;
        releaseCandidate[release] = assignment.candidate;
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
    /// epoch `epoch`, while a turn names the operator: each path scans at most `heartbeatMisses + 1`
    /// slots.
    function _dueAt(Vault vault, uint64 epoch) private view returns (uint64 due) {
        Liveness memory rule = liveness;
        uint64 interval = rule.heartbeatInterval;
        uint64 misses = rule.heartbeatMisses;
        uint64 changed = vault.signerSince() / interval;
        uint64 since = changed;
        (,,,, uint64 postedAt) = logHeads.heads(epoch);
        if (postedAt / interval > since) since = postedAt / interval;
        due = _counted(since, misses, interval);
        (uint64 turn, Turn memory named) = turnAt(uint64(block.timestamp));
        if (sets[named.setId].members.length < 2 || _latched(turn, named.position)) return due;
        since = boundary(turn) / interval;
        if (changed > since) since = changed;
        uint64 untaken = _counted(since, misses + 1, interval);
        if (untaken < due) due = untaken;
    }

    /// @dev The earliest block time at which `count` complete slots after slot `since` are all marked,
    /// assuming every later slot is: the latest unmarked one among the `count` complete slots before the
    /// current one, if after `since`, restarts the count.
    function _counted(uint64 since, uint64 count, uint64 interval) private view returns (uint64) {
        uint64 current = uint64(block.timestamp) / interval;
        for (uint64 slot = current - 1; slot > since && slot + count >= current; --slot) {
            if (!_marked(slot, interval)) {
                since = slot;
                break;
            }
        }
        return (since + count + 1) * interval;
    }

    /// @dev The turn in effect and the time from which a skip of its current assignment passes, as
    /// `skipDueAt` states, while a turn names the operator.
    function _skipDue() private view returns (uint64 turn, uint64) {
        Turn memory named;
        (turn, named) = turnAt(uint64(block.timestamp));
        Vault vault = _vault();
        if (named.reason == PROMOTED || _latched(turn, named.position) || vault.checkpointSigner() == address(0)) {
            return (turn, type(uint64).max);
        }
        uint64 interval = liveness.heartbeatInterval;
        uint64 since = boundary(turn) / interval;
        uint64 at = assignments[turn][named.position].at / interval;
        if (at > since) since = at;
        at = vault.signerSince() / interval;
        if (at > since) since = at;
        return (turn, _counted(since, takeWindow.slots, interval));
    }

    /// @dev Whether a head latched turn `turn`'s assignment at `position`.
    function _latched(uint64 turn, uint16 position) private view returns (bool) {
        (uint64 at,,,,) = logHeads.latches(turn, position);
        return at != 0;
    }

    /// @dev Whether a witness marked `slot` under `interval`.
    function _marked(uint64 slot, uint64 interval) private view returns (bool) {
        uint64 markedAt = marks[slot].at;
        return markedAt != 0 && markedAt / interval == slot;
    }

    /// @dev Reverts unless each of `rule`'s values is within the deployment's bounds and together they
    /// keep detection within ten minutes and above the switch budget, the tolerance under one
    /// interval, and the held-head block window within `arbBlockHash`'s reach and under one interval.
    /// In full: `heartbeatMisses >= 2`, `(heartbeatMisses + 1) * heartbeatInterval + headTolerance <= 600`,
    /// `headTolerance < heartbeatInterval`, `headBlocks <= 256`, `headBlocks * blockMillis <
    /// heartbeatInterval * 1000` and `heartbeatMisses * heartbeatInterval > switchBudget`.
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

    /// @dev Whether, under `rule` and a take window of `slots`, a turn's first skip comes due early enough
    /// for its member to take the turn before the untaken-turn promotion: from the same slot, the skip
    /// is due `slots + 1` slots on and the promotion `heartbeatMisses + 2`, and the take between them
    /// needs, as the window's minimum does, the switch budget plus one interval plus a head's inclusion,
    /// which `headTolerance` bounds, its head signed before the member's node stops at `STOP_MARGIN`:
    /// `(heartbeatMisses - slots) * heartbeatInterval > switchBudget + max(headTolerance, STOP_MARGIN)`.
    function _skipsFirst(Liveness memory rule, uint256 slots) private view returns (bool) {
        uint256 misses = rule.heartbeatMisses;
        uint256 margin = rule.headTolerance > STOP_MARGIN ? rule.headTolerance : STOP_MARGIN;
        return misses > slots && (misses - slots) * rule.heartbeatInterval > switchBudget + margin;
    }

    /// @dev Reverts unless the registry's timelock calls while no turn names the operator.
    function _governWithNoTurn() private view {
        if (msg.sender != address(registry.governance())) revert UnauthorizedGovernance();
        if (!_noTurn()) revert TurnNamesTheOperator();
    }

    /// @dev Whether no turn names the operator: the registry's operator generation is zero.
    function _noTurn() private view returns (bool) {
        return registry.operatorGeneration() == 0;
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

    /// @dev A turn drawn after `turn` before its promotion or skip took its generation from the holder
    /// the draw expected: the new member's continues the reassignment's `generation` on its `release`,
    /// as a repeat turn continues its holder's, and one that continued the outgoing assignment's
    /// (`outgoing`) takes a new generation, since that assignment's member does not hold the keys.
    function _carryNext(uint64 turn, address member, uint64 generation, uint64 outgoing, bytes32 release) private {
        uint64 last = lastDrawnTurn;
        if (last == turn) return;
        Turn storage next = turns[last];
        if (next.candidate == member) {
            _move(last, generation, release);
        } else if (next.generation == outgoing) {
            _move(last, _reserve(), _releaseOf(next.candidate));
        }
    }

    /// @dev Gives `turn` `generation`, appending it with `release` to its latest assignment's revisions.
    function _move(uint64 turn, uint64 generation, bytes32 release) private {
        Turn storage moved = turns[turn];
        moved.generation = generation;
        revisions[turn][moved.position].push(Revision(release, generation));
        emit TurnGenerationMoved(turn, generation);
    }
}
