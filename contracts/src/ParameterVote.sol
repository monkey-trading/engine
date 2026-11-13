// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Math} from "@openzeppelin-contracts/utils/math/Math.sol";

import {OperatorRegistry} from "./OperatorRegistry.sol";
import {OperatorSchedule} from "./OperatorSchedule.sol";

/// @title ParameterVote
/// @notice While rotation is on, the engine's governed parameters change only by a vote of the members
/// of the operator schedule's rotation set, each weighted by the stake the set snapshotted when it was
/// adopted. A member of the set in effect proposes a command, which casts its own yes vote; members of
/// that same set vote yes until the window closes, and the vote that first takes the yes weight past
/// two thirds of the set's total passes it. A member that does not vote counts against. The engine
/// mirrors `ProposalPassed` from finalised blocks and applies the command.
/// @dev Rotation is on while the registry's `rotationEnabled()` holds. The contract neither reads the
/// command bytes nor limits the parameters: the engine decodes the command and holds every change to
/// its own limits, and `fast` asks for the engine's fast path, which it keeps for risk-reducing
/// restrictions and applies without notice. A proposal keeps the set it opened under, so a newer set
/// taking effect changes neither its voters nor its weights. Not upgradeable; no owner, no funds.
contract ParameterVote {
    /// @notice A proposal: the hash of its command, the member that proposed it, the set in effect when
    /// it opened, its path, whether it passed, its window in seconds since the Unix epoch, and the
    /// set's total weight with the weight voting yes.
    struct Proposal {
        bytes32 commandHash;
        address proposer;
        uint32 setId;
        bool fast;
        bool passed;
        uint64 opensAt;
        uint64 closesAt;
        uint256 totalWeight;
        uint256 yesWeight;
    }

    /// @notice One proposal as the paged view returns it.
    struct ProposalView {
        uint256 id;
        Proposal proposal;
        bytes command;
    }

    /// @notice The longest command a proposal takes, in bytes.
    uint256 public constant MAX_COMMAND_BYTES = 32_768;

    /// @notice The longest window either path may have, in seconds.
    uint64 public constant MAX_WINDOW = 30 days;

    /// @notice The schedule whose rotation sets vote.
    OperatorSchedule public immutable schedule;

    /// @notice The schedule's registry, whose `rotationEnabled()` opens voting.
    OperatorRegistry public immutable registry;

    /// @notice How long a proposal on the normal path takes votes, in seconds.
    uint64 public immutable normalWindow;

    /// @notice How long a proposal on the fast path takes votes, in seconds.
    uint64 public immutable fastWindow;

    /// @notice Proposals made so far; ids count from 1.
    uint256 public proposalCount;

    /// @notice Whether `member` has voted on proposal `id`.
    mapping(uint256 id => mapping(address member => bool)) public voted;

    mapping(uint256 id => Proposal) private proposed;
    mapping(uint256 id => bytes) private commands;

    /// @notice `proposer`, a member of set `setId`, proposed `command`, which takes votes until `closesAt`.
    event Proposed(
        uint256 indexed id,
        address indexed proposer,
        uint32 indexed setId,
        bool fast,
        uint64 closesAt,
        bytes32 commandHash,
        bytes command
    );
    /// @notice `member` voted yes on `id` with `weight`, taking its yes weight to `yesWeight`.
    event Voted(uint256 indexed id, address indexed member, uint256 weight, uint256 yesWeight);
    /// @notice Proposal `id` passed with `yesWeight` of set `setId`'s `totalWeight`; the engine applies
    /// `command`.
    event ProposalPassed(
        uint256 indexed id,
        uint32 indexed setId,
        bool fast,
        uint256 yesWeight,
        uint256 totalWeight,
        bytes32 commandHash,
        bytes command
    );

    error ZeroAddress();
    error InvalidWindow();
    error RotationOff();
    error NoSetInEffect();
    error NotAMember(address account);
    error EmptyCommand();
    error CommandTooLong(uint256 length);
    error UnknownProposal(uint256 id);
    error AlreadyPassed(uint256 id);
    error VotingClosed(uint256 id, uint64 closesAt);
    error AlreadyVoted(uint256 id, address member);

    constructor(OperatorSchedule schedule_, uint64 normalWindow_, uint64 fastWindow_) {
        if (address(schedule_).code.length == 0) revert ZeroAddress();
        if (normalWindow_ == 0 || normalWindow_ > MAX_WINDOW || fastWindow_ == 0 || fastWindow_ > MAX_WINDOW) {
            revert InvalidWindow();
        }
        schedule = schedule_;
        registry = schedule_.registry();
        normalWindow = normalWindow_;
        fastWindow = fastWindow_;
    }

    /// @notice Proposes `command` to the set in effect, on the fast path or the normal one, and casts
    /// the caller's yes vote, which passes it at once when the caller holds more than two thirds of the
    /// set's weight.
    /// @return id The new proposal's id.
    function propose(bytes calldata command, bool fast) external returns (uint256 id) {
        if (!registry.rotationEnabled()) revert RotationOff();
        uint32 setId = schedule.currentSetId();
        if (setId == 0) revert NoSetInEffect();
        (uint256 weight, uint256 total) = weightOf(setId, msg.sender);
        if (weight == 0) revert NotAMember(msg.sender);
        if (command.length == 0) revert EmptyCommand();
        if (command.length > MAX_COMMAND_BYTES) revert CommandTooLong(command.length);

        id = ++proposalCount;
        _open(id, command, fast, setId, total);
        _vote(id, proposed[id], weight);
    }

    /// @notice Votes yes on proposal `id` with the caller's weight in the set it opened under, before
    /// its window closes and while rotation is on.
    function vote(uint256 id) external {
        if (id == 0 || id > proposalCount) revert UnknownProposal(id);
        Proposal storage proposal = proposed[id];
        if (proposal.passed) revert AlreadyPassed(id);
        if (block.timestamp >= proposal.closesAt) revert VotingClosed(id, proposal.closesAt);
        if (!registry.rotationEnabled()) revert RotationOff();
        (uint256 weight,) = weightOf(proposal.setId, msg.sender);
        if (weight == 0) revert NotAMember(msg.sender);
        if (voted[id][msg.sender]) revert AlreadyVoted(id, msg.sender);
        _vote(id, proposal, weight);
    }

    /// @notice Proposal `id` and its command; zeros and an empty command when there is none.
    function proposalOf(uint256 id) external view returns (Proposal memory, bytes memory command) {
        return (proposed[id], commands[id]);
    }

    /// @notice Up to `limit` proposals from id `first` on, in id order; empty when `first` is zero or
    /// past the last.
    function proposals(uint256 first, uint256 limit) external view returns (ProposalView[] memory page) {
        uint256 count = proposalCount;
        if (first == 0 || first > count) return page;
        uint256 length = Math.min(limit, count - first + 1);
        page = new ProposalView[](length);
        for (uint256 i = 0; i < length; ++i) {
            uint256 id = first + i;
            page[i] = ProposalView(id, proposed[id], commands[id]);
        }
    }

    /// @notice `member`'s weight in set `setId` and the set's total weight, as the set adopted them;
    /// zero weight for an account outside the set.
    function weightOf(uint32 setId, address member) public view returns (uint256 weight, uint256 total) {
        (,, address[] memory candidates, uint256[] memory weights) = schedule.setOf(setId);
        for (uint256 i = 0; i < candidates.length; ++i) {
            if (candidates[i] == member) weight += weights[i];
            total += weights[i];
        }
    }

    /// @notice Whether `yesWeight` is strictly more than two thirds of `totalWeight`:
    /// `yesWeight * 3 > totalWeight * 2`, compared without overflow.
    function passes(uint256 yesWeight, uint256 totalWeight) public pure returns (bool) {
        // 3y > 2t exactly when y exceeds floor(2t / 3).
        return yesWeight > Math.mulDiv(totalWeight, 2, 3);
    }

    /// @dev Stores proposal `id` of `command` to set `setId`, open from now for its path's window.
    function _open(uint256 id, bytes calldata command, bool fast, uint32 setId, uint256 total) private {
        uint64 opensAt = uint64(block.timestamp);
        uint64 closesAt = opensAt + (fast ? fastWindow : normalWindow);
        bytes32 commandHash = keccak256(command);
        proposed[id] = Proposal(commandHash, msg.sender, setId, fast, false, opensAt, closesAt, total, 0);
        commands[id] = command;
        emit Proposed(id, msg.sender, setId, fast, closesAt, commandHash, command);
    }

    /// @dev Records the caller's yes vote with `weight`, and passes the proposal when it first takes
    /// the yes weight past two thirds.
    function _vote(uint256 id, Proposal storage proposal, uint256 weight) private {
        voted[id][msg.sender] = true;
        uint256 yesWeight = proposal.yesWeight + weight;
        proposal.yesWeight = yesWeight;
        emit Voted(id, msg.sender, weight, yesWeight);
        if (!passes(yesWeight, proposal.totalWeight)) return;
        proposal.passed = true;
        emit ProposalPassed(
            id, proposal.setId, proposal.fast, yesWeight, proposal.totalWeight, proposal.commandHash, commands[id]
        );
    }
}
