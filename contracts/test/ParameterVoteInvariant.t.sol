// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test, Vm} from "forge-std/Test.sol";

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {ParameterVote} from "../src/ParameterVote.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";

/// Members of a pool propose and vote while new rotation sets take effect, rotation switches, and time
/// passes within and past the windows. Each call's outcome is checked against a model of the rules,
/// and the handler records every command proposed, the weight each vote should add, and the events.
contract ParameterVoteHandler is Test {
    uint256 internal constant POOL = 6;

    ParameterVote internal immutable votes;
    OperatorSchedule internal immutable schedule;
    OperatorRegistry internal immutable registry;
    address internal immutable governance;

    address[POOL] internal pool;
    uint64 internal epoch;

    /// Each adopted set's weights and total, as the handler adopted them.
    mapping(uint32 setId => mapping(address member => uint256)) public weightIn;
    mapping(uint32 setId => uint256) public totalOf;
    /// The command proposed under each id, the weight of the votes that should count, and who voted.
    mapping(uint256 id => bytes) public sent;
    mapping(uint256 id => uint256) public yesSum;
    mapping(uint256 id => mapping(address member => bool)) internal hasVoted;
    /// How many times each id's `ProposalPassed` was emitted.
    mapping(uint256 id => uint256) public passedEvents;

    /// A `Voted` event emitted at or after its proposal's close.
    bool public lateVote;
    /// A call succeeded that the rules refuse, or reverted when they allow it.
    bool public unexpected;

    constructor(ParameterVote votes_, address governance_) {
        votes = votes_;
        schedule = votes_.schedule();
        registry = votes_.registry();
        governance = governance_;
        for (uint256 i = 0; i < POOL; ++i) {
            pool[i] = makeAddr(string.concat("member ", vm.toString(i)));
        }
        _adopt(0, type(uint256).max, 0);
    }

    function propose(uint256 memberSeed, bool fast, uint256 commandSeed, uint8 length) external {
        uint32 setId = schedule.currentSetId();
        address member = _member(setId, memberSeed);
        bytes memory command = _command(commandSeed, bound(length, 0, 64));
        uint256 weight = weightIn[setId][member];
        bool allowed = registry.rotationEnabled() && setId != 0 && weight != 0 && command.length != 0;

        vm.recordLogs();
        vm.prank(member);
        try votes.propose(command, fast) returns (uint256 id) {
            if (!allowed) unexpected = true;
            sent[id] = command;
            yesSum[id] = weight;
            hasVoted[id][member] = true;
        } catch {
            if (allowed) unexpected = true;
        }
        _record(vm.getRecordedLogs());
    }

    /// Votes on one of the three latest proposals seven times in eight, else on any id from zero to one
    /// past the last, which exercises the unknown-proposal refusal.
    function vote(uint256 idSeed, uint256 memberSeed) external {
        uint256 count = votes.proposalCount();
        uint256 recent = count < 3 ? count : 3;
        uint256 id = recent == 0 || idSeed % 8 == 0 ? bound(idSeed >> 3, 0, count + 1) : count - (idSeed >> 3) % recent;
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
        address member = _member(proposal.setId, memberSeed);
        uint256 weight = weightIn[proposal.setId][member];
        bool allowed = id != 0 && id <= votes.proposalCount() && !proposal.passed && block.timestamp < proposal.closesAt
            && registry.rotationEnabled() && weight != 0 && !hasVoted[id][member];

        vm.recordLogs();
        vm.prank(member);
        try votes.vote(id) {
            if (!allowed) unexpected = true;
            yesSum[id] += weight;
            hasVoted[id][member] = true;
        } catch {
            if (allowed) unexpected = true;
        }
        _record(vm.getRecordedLogs());
    }

    /// Adopts a set of the pool members `mask` selects, weighted from `seed`, to take effect within two
    /// days; an empty selection adopts an empty set after one with members.
    function adopt(uint256 seed, uint256 mask, uint32 delay) external {
        _adopt(seed, mask, bound(delay, 0, 2 days));
    }

    /// Turns rotation off one time in four, and back on whenever it is off.
    function switchRotation(uint8 chance) external {
        bool enabled = registry.rotationSwitch();
        if (enabled && chance % 4 != 0) return;
        vm.prank(governance);
        registry.setRotation(!enabled);
    }

    function warpWithin(uint32 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 0, 2 hours));
    }

    function warpPast(uint32 seconds_) external {
        vm.warp(block.timestamp + bound(seconds_, 0, 4 days));
    }

    /// @dev A member of set `setId` three times in four, else any pool member, which may be outside it.
    function _member(uint32 setId, uint256 seed) internal view returns (address) {
        (,, address[] memory members,) = schedule.setOf(setId);
        if (members.length == 0 || seed % 4 == 0) return pool[(seed >> 2) % POOL];
        return members[(seed >> 2) % members.length];
    }

    function _adopt(uint256 seed, uint256 mask, uint256 delay) internal {
        address[] memory members = new address[](POOL);
        uint256[] memory weights = new uint256[](POOL);
        uint256 count = 0;
        for (uint256 i = 0; i < POOL; ++i) {
            if ((mask >> i) & 1 == 0) continue;
            members[count] = pool[i];
            weights[count] = bound(uint256(keccak256(abi.encode(seed, i))), 1, 1e27);
            ++count;
        }
        assembly ("memory-safe") {
            mstore(members, count)
            mstore(weights, count)
        }
        uint32 before = schedule.setCount();
        vm.prank(address(registry));
        schedule.adopt(++epoch, uint64(block.timestamp + delay), members, members, weights, true);
        uint32 setId = schedule.setCount();
        if (setId == before) return;
        for (uint256 i = 0; i < count; ++i) {
            weightIn[setId][members[i]] = weights[i];
            totalOf[setId] += weights[i];
        }
    }

    function _record(Vm.Log[] memory logs) internal {
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].emitter != address(votes)) continue;
            uint256 id = uint256(logs[i].topics[1]);
            if (logs[i].topics[0] == ParameterVote.ProposalPassed.selector) ++passedEvents[id];
            if (logs[i].topics[0] == ParameterVote.Voted.selector) {
                (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
                if (block.timestamp >= proposal.closesAt) lateVote = true;
            }
        }
    }

    function _command(uint256 seed, uint256 length) internal pure returns (bytes memory command) {
        command = new bytes(length);
        bytes32 word = keccak256(abi.encode(seed));
        for (uint256 i = 0; i < length; ++i) {
            command[i] = word[i % 32];
        }
    }
}

contract ParameterVoteInvariantTest is ScheduleTest {
    ParameterVote internal votes;
    ParameterVoteHandler internal handler;

    function setUp() public {
        _deploySchedule();
        _setRotation(true);
        votes = new ParameterVote(schedule, 3 days, 1 hours);
        handler = new ParameterVoteHandler(votes, address(governance));
        targetContract(address(handler));
    }

    /// Every call succeeded exactly when the rules allow it.
    function invariant_EveryCallFollowsTheRules() public view {
        assertFalse(handler.unexpected());
    }

    /// The yes weight is the sum of the votes cast, within the total of the set the proposal opened
    /// under.
    function invariant_TheYesWeightIsTheVotesCastWithinTheTotal() public view {
        for (uint256 id = 1; id <= votes.proposalCount(); ++id) {
            (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
            assertEq(proposal.yesWeight, handler.yesSum(id));
            assertEq(proposal.totalWeight, handler.totalOf(proposal.setId));
            assertLe(proposal.yesWeight, proposal.totalWeight);
        }
    }

    /// A proposal has passed exactly when its yes weight is more than two thirds of its total, and
    /// `ProposalPassed` was emitted once for it then and never otherwise.
    function invariant_PassedExactlyWhenPastTwoThirdsAndEmittedOnce() public view {
        for (uint256 id = 1; id <= votes.proposalCount(); ++id) {
            (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
            assertEq(proposal.passed, votes.passes(proposal.yesWeight, proposal.totalWeight));
            assertEq(proposal.passed, proposal.yesWeight * 3 > proposal.totalWeight * 2);
            assertEq(handler.passedEvents(id), proposal.passed ? 1 : 0);
        }
    }

    /// Every proposal stores the command proposed, and its hash.
    function invariant_TheStoredCommandIsTheOneProposed() public view {
        for (uint256 id = 1; id <= votes.proposalCount(); ++id) {
            (ParameterVote.Proposal memory proposal, bytes memory command) = votes.proposalOf(id);
            assertEq(command, handler.sent(id));
            assertEq(proposal.commandHash, keccak256(command));
        }
    }

    function invariant_NoVoteCountsAtOrAfterTheClose() public view {
        assertFalse(handler.lateVote());
    }
}
