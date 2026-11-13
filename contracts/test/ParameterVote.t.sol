// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vm, console} from "forge-std/Test.sol";

import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {ParameterVote} from "../src/ParameterVote.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";

/// Proposals to a rotation set, weighted yes votes, the two-thirds rule, and the set a proposal keeps.
contract ParameterVoteTest is ScheduleTest {
    /// @dev Test values only: the normal window outlasts the notice before a new set takes effect.
    uint64 internal constant NORMAL = 10 days;
    uint64 internal constant FAST = 1 hours;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal dave = makeAddr("dave");
    address internal stranger = makeAddr("stranger");

    bytes internal command = abi.encode("set profit fee", uint256(1_500));

    ParameterVote internal votes;

    function setUp() public {
        _deploySchedule();
        votes = new ParameterVote(schedule, NORMAL, FAST);
    }

    function test_TheConstructorTakesTheScheduleRegistryAndWindows() public view {
        assertEq(address(votes.schedule()), address(schedule));
        assertEq(address(votes.registry()), address(registry));
        assertEq(votes.normalWindow(), NORMAL);
        assertEq(votes.fastWindow(), FAST);
        assertEq(votes.proposalCount(), 0);
    }

    function test_TheConstructorRefusesAScheduleWithoutCode() public {
        vm.expectRevert(ParameterVote.ZeroAddress.selector);
        new ParameterVote(OperatorSchedule(address(0)), NORMAL, FAST);
        vm.expectRevert(ParameterVote.ZeroAddress.selector);
        new ParameterVote(OperatorSchedule(stranger), NORMAL, FAST);
    }

    function test_TheConstructorBoundsBothWindows() public {
        uint64 max = votes.MAX_WINDOW();
        vm.expectRevert(ParameterVote.InvalidWindow.selector);
        new ParameterVote(schedule, 0, FAST);
        vm.expectRevert(ParameterVote.InvalidWindow.selector);
        new ParameterVote(schedule, NORMAL, 0);
        vm.expectRevert(ParameterVote.InvalidWindow.selector);
        new ParameterVote(schedule, max + 1, FAST);
        vm.expectRevert(ParameterVote.InvalidWindow.selector);
        new ParameterVote(schedule, NORMAL, max + 1);
        ParameterVote widest = new ParameterVote(schedule, max, max);
        assertEq(widest.normalWindow(), 30 days);
        ParameterVote narrowest = new ParameterVote(schedule, 1, 1);
        assertEq(narrowest.fastWindow(), 1);
    }

    function test_AProposalRecordsItselfAndTheProposersVote() public {
        uint32 setId = _rotateThree();
        uint64 closesAt = uint64(block.timestamp) + NORMAL;
        vm.expectEmit(address(votes));
        emit ParameterVote.Proposed(1, alice, setId, false, closesAt, keccak256(command), command);
        vm.expectEmit(address(votes));
        emit ParameterVote.Voted(1, alice, 40_000e18, 40_000e18);
        vm.prank(alice);
        assertEq(votes.propose(command, false), 1);

        assertEq(votes.proposalCount(), 1);
        assertTrue(votes.voted(1, alice));
        assertFalse(votes.voted(1, bob));
        (ParameterVote.Proposal memory proposal, bytes memory stored) = votes.proposalOf(1);
        assertEq(proposal.commandHash, keccak256(command));
        assertEq(proposal.proposer, alice);
        assertEq(proposal.setId, setId);
        assertFalse(proposal.fast);
        assertFalse(proposal.passed);
        assertEq(proposal.opensAt, block.timestamp);
        assertEq(proposal.closesAt, closesAt);
        assertEq(proposal.totalWeight, 100_000e18);
        assertEq(proposal.yesWeight, 40_000e18);
        assertEq(stored, command);
    }

    function test_SeventyPercentPassesAndReemitsTheCommand() public {
        uint32 setId = _rotateThree();
        _propose(alice, false);
        vm.expectEmit(address(votes));
        emit ParameterVote.Voted(1, bob, 30_000e18, 70_000e18);
        vm.expectEmit(address(votes));
        emit ParameterVote.ProposalPassed(1, setId, false, 70_000e18, 100_000e18, keccak256(command), command);
        vm.prank(bob);
        votes.vote(1);

        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertTrue(proposal.passed);
        assertEq(proposal.yesWeight, 70_000e18);
        vm.prank(carol);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.AlreadyPassed.selector, 1));
        votes.vote(1);
    }

    function test_SixtyPercentNeverPasses() public {
        _rotateThree();
        _propose(bob, false);
        vm.recordLogs();
        vm.prank(carol);
        votes.vote(1);
        assertEq(_passedEvents(vm.getRecordedLogs()), 0);

        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertFalse(proposal.passed);
        assertEq(proposal.yesWeight, 60_000e18);
        vm.warp(proposal.closesAt);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.VotingClosed.selector, 1, proposal.closesAt));
        votes.vote(1);
        vm.warp(uint256(proposal.closesAt) + 30 days);
        (proposal,) = votes.proposalOf(1);
        assertFalse(proposal.passed, "a closed proposal stays unpassed");
    }

    function test_ExactlyTwoThirdsDoesNotPass() public {
        address[] memory members = new address[](3);
        uint256[] memory stakes = new uint256[](3);
        (members[0], members[1], members[2]) = (alice, bob, carol);
        (stakes[0], stakes[1], stakes[2]) = (30_000e18, 30_000e18, 30_000e18);
        uint32 setId = _rotate(members, stakes);
        _propose(alice, false);
        vm.prank(bob);
        votes.vote(1);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertEq(proposal.yesWeight * 3, proposal.totalWeight * 2, "two of three equal members are two thirds");
        assertFalse(proposal.passed);

        vm.expectEmit(address(votes));
        emit ParameterVote.ProposalPassed(1, setId, false, 90_000e18, 90_000e18, keccak256(command), command);
        vm.prank(carol);
        votes.vote(1);
    }

    function test_AMemberWithExactlyTwoThirdsDoesNotPassAlone() public {
        address[] memory members = new address[](2);
        uint256[] memory stakes = new uint256[](2);
        (members[0], members[1]) = (alice, bob);
        (stakes[0], stakes[1]) = (20_000e18, 10_000e18);
        _rotate(members, stakes);
        _propose(alice, true);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertEq(proposal.yesWeight, 20_000e18);
        assertEq(proposal.totalWeight, 30_000e18);
        assertFalse(proposal.passed);
        vm.prank(bob);
        votes.vote(1);
        (proposal,) = votes.proposalOf(1);
        assertTrue(proposal.passed);
    }

    function test_AOneMemberSetPassesOnTheProposal() public {
        uint32 setId = _rotateOne(alice);
        vm.expectEmit(address(votes));
        emit ParameterVote.Proposed(1, alice, setId, true, uint64(block.timestamp) + FAST, keccak256(command), command);
        vm.expectEmit(address(votes));
        emit ParameterVote.Voted(1, alice, 30_000e18, 30_000e18);
        vm.expectEmit(address(votes));
        emit ParameterVote.ProposalPassed(1, setId, true, 30_000e18, 30_000e18, keccak256(command), command);
        vm.prank(alice);
        votes.propose(command, true);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertTrue(proposal.passed);
    }

    function test_EachPathHasItsWindow() public {
        _rotateThree();
        _propose(bob, true);
        _propose(bob, false);
        (ParameterVote.Proposal memory fast,) = votes.proposalOf(1);
        (ParameterVote.Proposal memory normal,) = votes.proposalOf(2);
        assertTrue(fast.fast);
        assertFalse(normal.fast);
        assertEq(fast.closesAt - fast.opensAt, FAST);
        assertEq(normal.closesAt - normal.opensAt, NORMAL);
    }

    function test_VotingClosesAtClosesAt() public {
        _rotateThree();
        _propose(bob, true);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        vm.warp(proposal.closesAt - 1);
        vm.prank(carol);
        votes.vote(1);
        vm.warp(proposal.closesAt);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.VotingClosed.selector, 1, proposal.closesAt));
        votes.vote(1);

        _propose(bob, false);
        (proposal,) = votes.proposalOf(2);
        vm.warp(proposal.closesAt - 1);
        vm.prank(alice);
        votes.vote(2);
        (proposal,) = votes.proposalOf(2);
        assertTrue(proposal.passed, "a vote one second before the close counts");
    }

    function test_OnlyMembersProposeAndVote() public {
        _rotateThree();
        vm.prank(stranger);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.NotAMember.selector, stranger));
        votes.propose(command, false);
        _propose(alice, false);
        vm.prank(stranger);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.NotAMember.selector, stranger));
        votes.vote(1);
        vm.prank(_key(bob));
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.NotAMember.selector, _key(bob)));
        votes.vote(1);
    }

    function test_AProposalKeepsTheSetItOpenedUnder() public {
        uint32 first = _rotateThree();
        vm.prank(carol);
        registry.deregister();
        _candidate(dave, 50_000e18, 4);
        _delegate(dave, dave);
        _closeNext();
        uint32 second = schedule.pendingSetId();
        assertEq(second, first + 1);
        _propose(alice, false);
        (, uint64 effectiveAt,,) = schedule.setOf(second);
        vm.warp(effectiveAt);
        assertEq(schedule.currentSetId(), second);
        // Stake moved since the first set changes nothing about its weights.
        _stake(bob, 100_000e18);
        _delegate(bob, bob);

        vm.prank(dave);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.NotAMember.selector, dave));
        votes.vote(1);
        vm.prank(carol);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.NotAMember.selector, carol));
        votes.propose(command, false);
        vm.prank(carol);
        votes.vote(1);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertEq(proposal.setId, first);
        assertEq(proposal.yesWeight, 70_000e18);
        assertEq(proposal.totalWeight, 100_000e18);
        assertTrue(proposal.passed);

        _propose(dave, false);
        (proposal,) = votes.proposalOf(2);
        assertEq(proposal.setId, second);
        (uint256 daveWeight, uint256 total) = votes.weightOf(second, dave);
        assertEq(proposal.yesWeight, daveWeight);
        assertEq(proposal.totalWeight, total);
    }

    function test_AMemberVotesOnce() public {
        _rotateThree();
        _propose(bob, false);
        vm.prank(bob);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.AlreadyVoted.selector, 1, bob));
        votes.vote(1);
        vm.prank(carol);
        votes.vote(1);
        vm.prank(carol);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.AlreadyVoted.selector, 1, carol));
        votes.vote(1);
    }

    function test_AnUnknownProposalTakesNoVote() public {
        _rotateThree();
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.UnknownProposal.selector, 0));
        votes.vote(0);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.UnknownProposal.selector, 1));
        votes.vote(1);
        _propose(bob, false);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.UnknownProposal.selector, 2));
        votes.vote(2);
    }

    function test_RotationOffRefusesProposalsAndVotes() public {
        vm.prank(alice);
        vm.expectRevert(ParameterVote.RotationOff.selector);
        votes.propose(command, false);

        _rotateThree();
        _propose(bob, false);
        _setRotation(false);
        vm.prank(alice);
        vm.expectRevert(ParameterVote.RotationOff.selector);
        votes.propose(command, false);
        vm.prank(alice);
        vm.expectRevert(ParameterVote.RotationOff.selector);
        votes.vote(1);

        _setRotation(true);
        vm.prank(alice);
        votes.vote(1);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertTrue(proposal.passed);
    }

    function test_ADrawnTurnKeepsVotingOpenAfterTheSwitchTurnsOff() public {
        _rotateThree();
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        vm.warp(schedule.boundary(turn));
        _setRotation(false);
        assertTrue(registry.rotationEnabled(), "a drawn turn names the operator");
        _propose(alice, false);
        vm.prank(bob);
        votes.vote(1);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertTrue(proposal.passed);
    }

    function test_NoSetInEffectRefusesProposals() public {
        _candidate(alice, 40_000e18, 1);
        _delegate(alice, alice);
        _setRotation(true);
        vm.prank(alice);
        vm.expectRevert(ParameterVote.NoSetInEffect.selector);
        votes.propose(command, false);
        _closeNext();
        vm.prank(alice);
        vm.expectRevert(ParameterVote.NoSetInEffect.selector);
        votes.propose(command, false);
    }

    function test_TheCommandIsBounded() public {
        _rotateThree();
        vm.prank(alice);
        vm.expectRevert(ParameterVote.EmptyCommand.selector);
        votes.propose("", false);
        uint256 max = votes.MAX_COMMAND_BYTES();
        bytes memory tooLong = new bytes(max + 1);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(ParameterVote.CommandTooLong.selector, max + 1));
        votes.propose(tooLong, false);

        bytes memory longest = _bytes(max, "longest");
        vm.prank(alice);
        votes.propose(longest, false);
        (ParameterVote.Proposal memory proposal, bytes memory stored) = votes.proposalOf(1);
        assertEq(stored, longest);
        assertEq(proposal.commandHash, keccak256(longest));
        vm.expectEmit(address(votes));
        emit ParameterVote.ProposalPassed(1, 1, false, 70_000e18, 100_000e18, keccak256(longest), longest);
        vm.prank(bob);
        votes.vote(1);
    }

    function test_ProposalsPagesById() public {
        _rotateThree();
        bytes[] memory sent = new bytes[](3);
        for (uint256 i = 0; i < 3; ++i) {
            sent[i] = _bytes(i + 1, "page");
            vm.prank(bob);
            votes.propose(sent[i], i == 1);
        }
        assertEq(votes.proposals(0, 5).length, 0, "ids count from 1");
        assertEq(votes.proposals(1, 0).length, 0);
        assertEq(votes.proposals(4, 1).length, 0, "past the last");

        ParameterVote.ProposalView[] memory page = votes.proposals(1, 2);
        assertEq(page.length, 2);
        assertEq(page[0].id, 1);
        assertEq(page[1].id, 2);
        assertEq(page[1].command, sent[1]);
        assertTrue(page[1].proposal.fast);

        page = votes.proposals(2, 10);
        assertEq(page.length, 2, "clipped to the count");
        assertEq(page[0].id, 2);
        assertEq(page[1].id, 3);
        assertEq(page[1].command, sent[2]);
        assertEq(page[1].proposal.commandHash, keccak256(sent[2]));
        assertEq(page[1].proposal.proposer, bob);

        page = votes.proposals(3, type(uint256).max);
        assertEq(page.length, 1);
        assertEq(page[0].id, 3);
    }

    function test_AnUnknownProposalReadsAsZeros() public view {
        (ParameterVote.Proposal memory proposal, bytes memory stored) = votes.proposalOf(7);
        assertEq(proposal.proposer, address(0));
        assertEq(proposal.closesAt, 0);
        assertEq(stored.length, 0);
    }

    function test_WeightOfReadsTheSetsWeights() public {
        uint32 setId = _rotateThree();
        (uint256 weight, uint256 total) = votes.weightOf(setId, alice);
        assertEq(weight, 40_000e18);
        assertEq(total, 100_000e18);
        (weight, total) = votes.weightOf(setId, stranger);
        assertEq(weight, 0);
        assertEq(total, 100_000e18);
        (weight, total) = votes.weightOf(setId + 1, alice);
        assertEq(weight, 0);
        assertEq(total, 0);
    }

    function test_PassesAtItsBoundaries() public view {
        assertFalse(votes.passes(0, 0));
        assertTrue(votes.passes(1, 0));
        assertFalse(votes.passes(0, 1));
        assertTrue(votes.passes(1, 1));
        assertFalse(votes.passes(2, 3));
        assertTrue(votes.passes(3, 3));
        assertFalse(votes.passes(200, 300));
        assertTrue(votes.passes(201, 300));
        assertFalse(votes.passes(2, 4));
        assertTrue(votes.passes(3, 4));
        // type(uint256).max is divisible by three.
        uint256 third = type(uint256).max / 3;
        assertFalse(votes.passes(2 * third, type(uint256).max));
        assertTrue(votes.passes(2 * third + 1, type(uint256).max));
        assertTrue(votes.passes(type(uint256).max, type(uint256).max));
    }

    /// `passes` agrees with `3 * yes > 2 * total` computed exactly in 128-bit limbs, over every input.
    function testFuzz_PassesIsStrictlyMoreThanTwoThirds(uint256 yes, uint256 total) public view {
        (uint256 yesHigh, uint256 yesLow) = _times(yes, 3);
        (uint256 totalHigh, uint256 totalLow) = _times(total, 2);
        bool expected = yesHigh > totalHigh || (yesHigh == totalHigh && yesLow > totalLow);
        assertEq(votes.passes(yes, total), expected);
    }

    /// Any subset of a set's members voting passes the proposal exactly when its weight times three
    /// exceeds twice the set's total, and `ProposalPassed` is emitted once, by the vote that crosses.
    function testFuzz_ASubsetPassesExactlyWhenItHoldsMoreThanTwoThirds(uint256 seed, uint8 size, uint256 voters)
        public
    {
        uint256 count = bound(size, 1, 30);
        address[] memory members = new address[](count);
        uint256[] memory weights = new uint256[](count);
        uint256 total = 0;
        for (uint256 i = 0; i < count; ++i) {
            members[i] = address(uint160(uint256(keccak256(abi.encode("member", i)))));
            weights[i] = bound(uint256(keccak256(abi.encode(seed, i))), 1, 1e30);
            total += weights[i];
        }
        _setRotation(true);
        vm.prank(address(registry));
        schedule.adopt(1, uint64(block.timestamp), members, members, weights, true);
        uint32 setId = schedule.currentSetId();
        // At least one voter, who proposes.
        voters |= 1;

        vm.recordLogs();
        uint256 subset = 0;
        uint256 yesAtPass = 0;
        bool proposed = false;
        for (uint256 i = 0; i < count; ++i) {
            if ((voters >> i) & 1 == 0) continue;
            subset += weights[i];
            vm.prank(members[i]);
            if (!proposed) {
                votes.propose(command, false);
                proposed = true;
            } else if (yesAtPass != 0) {
                vm.expectRevert(abi.encodeWithSelector(ParameterVote.AlreadyPassed.selector, 1));
                votes.vote(1);
                continue;
            } else {
                votes.vote(1);
            }
            if (yesAtPass == 0 && subset * 3 > total * 2) yesAtPass = subset;
        }

        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        bool expected = subset * 3 > total * 2;
        assertEq(proposal.setId, setId);
        assertEq(proposal.totalWeight, total);
        assertEq(proposal.passed, expected);
        assertEq(proposal.yesWeight, expected ? yesAtPass : subset);
        assertLe(proposal.yesWeight, proposal.totalWeight);
        assertEq(_passedEvents(vm.getRecordedLogs()), expected ? 1 : 0);
    }

    /// Reports a proposal of the longest command and the vote that passes it, at the member limit,
    /// with every slot cold as in its own transaction.
    function test_GasOfTheLongestCommandAtTheMemberLimit() public {
        uint256 count = uint256(registry.CANDIDATE_LIMIT()) + 1;
        address[] memory members = new address[](count);
        uint256[] memory weights = new uint256[](count);
        for (uint256 i = 0; i < count; ++i) {
            members[i] = address(uint160(uint256(keccak256(abi.encode("member", i)))));
            weights[i] = i == 0 || i == count - 1 ? 1_000_000e18 : 1e18;
        }
        _setRotation(true);
        vm.prank(address(registry));
        schedule.adopt(1, uint64(block.timestamp), members, members, weights, true);
        bytes memory longest = _bytes(votes.MAX_COMMAND_BYTES(), "gas");

        _cool();
        vm.prank(members[0]);
        votes.propose(longest, false);
        uint256 proposed = vm.lastCallGas().gasTotalUsed;
        _cool();
        vm.prank(members[count - 1]);
        votes.vote(1);
        uint256 passing = vm.lastCallGas().gasTotalUsed;
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(1);
        assertTrue(proposal.passed, "the last member's vote passes it");
        console.log("propose, longest command, 101 members, gas:", proposed);
        console.log("passing vote, longest command, 101 members, gas:", passing);
        // Within Arbitrum One's 32M per-transaction limit, leaving room for the command's calldata.
        assertLt(proposed, 30_000_000);
        assertLt(passing, 8_000_000);
    }

    function _cool() internal {
        vm.cool(address(votes));
        vm.cool(address(schedule));
        vm.cool(address(registry));
    }

    /// @dev Alice, bob and carol with 40%, 30% and 30% of the weight, in effect now.
    function _rotateThree() internal returns (uint32) {
        address[] memory members = new address[](3);
        uint256[] memory stakes = new uint256[](3);
        (members[0], members[1], members[2]) = (alice, bob, carol);
        (stakes[0], stakes[1], stakes[2]) = (40_000e18, 30_000e18, 30_000e18);
        return _rotate(members, stakes);
    }

    function _propose(address member, bool fast) internal returns (uint256) {
        vm.prank(member);
        return votes.propose(command, fast);
    }

    function _passedEvents(Vm.Log[] memory logs) internal view returns (uint256 passed) {
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].emitter == address(votes) && logs[i].topics[0] == ParameterVote.ProposalPassed.selector) {
                ++passed;
            }
        }
    }

    /// @dev `length` bytes derived from `tag`.
    function _bytes(uint256 length, string memory tag) internal pure returns (bytes memory out) {
        out = new bytes(length);
        for (uint256 i = 0; i < length; i += 32) {
            bytes32 word = keccak256(abi.encode(tag, i));
            for (uint256 j = i; j < length && j < i + 32; ++j) {
                out[j] = word[j - i];
            }
        }
    }

    /// @dev `a * k` for a small `k`, as a high and a low 128-bit limb, so no product overflows.
    function _times(uint256 a, uint256 k) internal pure returns (uint256 high, uint256 low) {
        low = (a & type(uint128).max) * k;
        high = (a >> 128) * k + (low >> 128);
        low &= type(uint128).max;
    }
}
