// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "./OperatorRegistry.sol";
import {SeedReceiver} from "./SeedReceiver.sol";
import {SeedSchedule} from "./SeedSchedule.sol";

/// @dev The vault, whose generations a turn holds.
interface IGenerations {
    function reserveGeneration() external returns (uint64);
    function releaseGeneration() external view returns (uint64);
    function syncedRelease() external view returns (bytes32);
}

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

    /// @notice A drawn turn. `prior` is the turn drawn before it, zero for the first; `reason` is 0
    /// for a draw; `source` is the operator the registry named at the draw, which hands the member
    /// the keys.
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

    /// @notice How many positions a draw tries before it refuses the turn.
    uint16 public constant MAX_DRAW_POSITIONS = 64;

    uint8 internal constant DRAWN = 0;

    /// @notice The registry whose closes adopt sets and whose listing a drawn member needs.
    OperatorRegistry public immutable registry;

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

    event RotationSetAdopted(
        uint32 indexed setId,
        uint64 indexed epoch,
        uint64 effectiveAt,
        bool quorum,
        address[] candidates,
        address[] adminKeys,
        uint256[] weights
    );
    /// @notice Turn `turn` was drawn from set `setId` at `position`, naming `candidate` with the
    /// `adminKey` the set adopted and the `release` it registered at the draw, under `generation`.
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

    constructor(OperatorRegistry registry_, SeedReceiver receiver_) SeedSchedule(receiver_.beaconGenesisTime()) {
        registry = registry_;
        seedReceiver = receiver_;
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
        emit TurnAssigned(turn, setId, position, DRAWN, candidate, member.adminKey, release, generation);
    }

    /// @notice Gives the turn in effect, and the last drawn turn, a new generation where `candidate` is
    /// their member: the registry calls it when the candidate's release moves. Both share one.
    function releaseMoved(address candidate) external onlyRegistry {
        (uint64 inEffect, Turn memory current) = turnAt(uint64(block.timestamp));
        uint64 last = lastDrawnTurn;
        uint64 generation = 0;
        if (inEffect != 0 && current.candidate == candidate) {
            generation = _reserve();
            _move(inEffect, generation);
        }
        if (last != inEffect && turns[last].candidate == candidate) {
            if (generation == 0) generation = _reserve();
            _move(last, generation);
        }
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

    /// @notice Whether `candidate` is the member of the turn in effect, its source, which hands it
    /// the keys, or the member of the last drawn turn.
    function holdsTurn(address candidate) external view returns (bool) {
        (, Turn memory current) = turnAt(uint64(block.timestamp));
        return
            candidate == current.candidate || candidate == current.source || candidate == turns[lastDrawnTurn].candidate;
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
            if (registry.isListed(member.candidate) && registry.keyHolder(member.adminKey) == member.candidate) {
                return (position, member);
            }
        }
        revert NoAvailableMember(turn);
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
        IGenerations vault = IGenerations(address(registry.vault()));
        if (candidate == named.candidate && release == named.release && vault.syncedRelease() == release) {
            return vault.releaseGeneration();
        }
        return vault.reserveGeneration();
    }

    function _reserve() private returns (uint64) {
        return IGenerations(address(registry.vault())).reserveGeneration();
    }

    function _move(uint64 turn, uint64 generation) private {
        turns[turn].generation = generation;
        emit TurnGenerationMoved(turn, generation);
    }
}
