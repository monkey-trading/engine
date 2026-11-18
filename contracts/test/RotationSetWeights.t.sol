// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vm} from "forge-std/Vm.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {ParameterVote} from "../src/ParameterVote.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";

/// @notice What a close weighs a rotation set's members by: the quorum counts only the votes of the
/// members a set would adopt by votes, a quorum carries a member of the latest set between standby keys
/// for an epoch from the first close that carried it with none, and each carried weight is at most the
/// member's votes at the close. At the launch
/// parameters the candidate runbook gives: a 14-day epoch, a one-day finality margin, eligibility at 1%
/// of total stake and a quorum of 25%.
contract RotationSetWeightsTest is ScheduleTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal dave = makeAddr("dave");
    address internal erin = makeAddr("erin");
    address internal backer = makeAddr("backer");
    address internal holders = makeAddr("holders");

    bytes internal command = abi.encode("a governed command");
    ParameterVote internal votes;

    function setUp() public {
        _deploySchedule();
        votes = new ParameterVote(schedule, 10 days, 1 hours);
        OperatorRegistry.Parameters memory launch = _parameters();
        (launch.epochLength, launch.finalityMargin, launch.eligibilityBps, launch.quorumBps) =
        (14 days, 1 days, 100, 2_500);
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (launch)));
    }

    /// @dev A backer delegates 210k to dave for one close, which has the quorum and weighs dave at
    /// 220k of 320k. The backer then unstakes, which leaves 110k of 810k delegated, under the quorum,
    /// and two more closes carry the set; dave's own 10k keeps it eligible.
    function _backedForOneClose() internal returns (uint32 setId) {
        _candidate(alice, 60_000e18, 1);
        _candidate(carol, 40_000e18, 2);
        _candidate(dave, 10_000e18, 3);
        _delegate(alice, alice);
        _delegate(carol, carol);
        _delegate(dave, dave);
        _stake(holders, 700_000e18);
        _stake(backer, 210_000e18);
        _delegate(backer, dave);
        _setRotation(true);
        _closeNext();
        (uint256 backed,) = _weightIn(1, dave);
        assertEq(backed, 220_000e18);

        vm.prank(backer);
        staking.startUnstake(210_000e18);
        vm.warp(block.timestamp + COOLDOWN);
        vm.prank(backer);
        staking.completeUnstake();
        assertEq(staking.stakes(backer), 0);

        _closeNext();
        _closeNext();
        setId = schedule.setCount();
        assertEq(setId, 3);
        (, uint64 effectiveAt,,) = schedule.setOf(setId);
        vm.warp(effectiveAt);
        assertEq(schedule.currentSetId(), setId);
    }

    function _weightIn(uint32 setId, address member) internal view returns (uint256 weight, uint256 total) {
        (,, address[] memory members, uint256[] memory weights) = schedule.setOf(setId);
        for (uint256 i = 0; i < members.length; ++i) {
            total += weights[i];
            if (members[i] == member) weight = weights[i];
        }
    }

    function test_BelowQuorumACarriedWeightIsAtMostTheMembersVotes() public {
        uint32 setId = _backedForOneClose();
        (uint256 weight, uint256 total) = _weightIn(setId, dave);
        assertEq(weight, staking.votes(dave), "the weight is dave's own votes");
        assertEq(weight, 10_000e18);
        (uint256 aliceWeight,) = _weightIn(setId, alice);
        assertEq(aliceWeight, 60_000e18, "a weight under the member's votes is kept");
        assertEq(total, 110_000e18);
    }

    function test_AVoteNeedsTheStakeThatStillBacksTheSet() public {
        uint32 setId = _backedForOneClose();
        assertLt(staking.votes(dave) * 10_000 / staking.totalStaked(), 200, "dave's votes are under 2% of the stake");
        vm.prank(dave);
        uint256 id = votes.propose(command, true);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
        assertEq(proposal.setId, setId);
        assertFalse(proposal.passed, "dave's own weight passes nothing");
        vm.prank(alice);
        votes.vote(id);
        vm.prank(carol);
        votes.vote(id);
        (proposal,) = votes.proposalOf(id);
        assertTrue(proposal.passed, "the set's members with the stake behind them pass it");
    }

    /// The cap applies to the set it carries, so a later close below the quorum carries the capped
    /// weights, and a member's added votes raise its weight only once a close has the quorum.
    function test_ACarriedWeightRisesOnlyWithAQuorum() public {
        uint32 setId = _backedForOneClose();
        // 210k of 910k delegated is still under the quorum.
        _stake(alice, 100_000e18);
        _delegate(alice, alice);
        _closeNext();
        (uint256 daveWeight,) = _weightIn(setId + 1, dave);
        (uint256 aliceWeight,) = _weightIn(setId + 1, alice);
        assertEq(daveWeight, 10_000e18);
        assertEq(aliceWeight, 60_000e18);

        _stake(alice, 200_000e18);
        _delegate(alice, alice);
        _closeNext();
        (aliceWeight,) = _weightIn(setId + 2, alice);
        assertEq(aliceWeight, 360_000e18, "a close with the quorum weighs by the votes now");
    }

    /// @dev Alice and bob (54% of the stake) move to newly approved releases, which voids their
    /// standby keys until each registers a new one, and the epoch ends in that window: dave, whose key
    /// is in place, closes it. Both then register new keys.
    function _closedDuringReleaseMoves() internal returns (uint32 setId) {
        setId = _closedWhileBetweenKeys();
        _registerStandby(alice);
        _registerStandby(bob);
        (, uint64 effectiveAt,,) = schedule.setOf(setId);
        vm.warp(effectiveAt);
    }

    /// @dev As `_closedDuringReleaseMoves`, with alice and bob still between keys after the close.
    function _closedWhileBetweenKeys() internal returns (uint32 setId) {
        _candidate(alice, 300_000e18, 1);
        _candidate(bob, 240_000e18, 2);
        _candidate(dave, 12_000e18, 3);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _delegate(dave, dave);
        _stake(holders, 448_000e18);
        _setRotation(true);
        _closeNext();
        (,, address[] memory first,) = schedule.setOf(1);
        assertEq(first.length, 3);
        _moveReleasesAsTheEpochEnds();
        vm.prank(dave);
        registry.closeEpoch();
        setId = schedule.setCount();
        assertEq(setId, 2);
    }

    /// @dev Moves alice and bob to newly approved releases ten minutes before the open epoch ends,
    /// which voids their standby keys, and warps to its end.
    function _moveReleasesAsTheEpochEnds() internal {
        (uint64 epochLength,,,,,,) = registry.parameters();
        vm.warp(uint256(registry.openEpochStart()) + epochLength - 10 minutes);
        _moveReleases();
        vm.warp(uint256(registry.openEpochStart()) + epochLength);
    }

    /// @dev Moves alice and bob to newly approved releases, which voids their standby keys.
    function _moveReleases() internal {
        vm.startPrank(address(governance));
        registry.setReleaseApproval(alice, _digest(11), CONFIG, true);
        registry.setReleaseApproval(bob, _digest(12), CONFIG, true);
        vm.stopPrank();
        vm.prank(alice);
        registry.setRelease(_digest(11), CONFIG);
        vm.prank(bob);
        registry.setRelease(_digest(12), CONFIG);
        assertEq(schedule.promotable(alice), address(0));
        assertEq(schedule.promotable(bob), address(0));
    }

    /// @dev Alice (30% of the stake) and bob (25%) move releases as the epoch ends, and erin, whose key
    /// is in place and whose 25% alone makes the quorum, closes it in that window. Returns whether the
    /// close had the quorum.
    function _quorumWhileBetweenKeys() internal returns (bool quorum) {
        _candidate(alice, 300_000e18, 1);
        _candidate(bob, 250_000e18, 2);
        _candidate(erin, 250_000e18, 3);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _delegate(erin, erin);
        _stake(holders, 200_000e18);
        _setRotation(true);
        _closeNext();
        _moveReleasesAsTheEpochEnds();
        vm.recordLogs();
        vm.prank(erin);
        registry.closeEpoch();
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; ++i) {
            if (logs[i].emitter != address(schedule)) continue;
            if (logs[i].topics[0] != OperatorSchedule.RotationSetAdopted.selector) continue;
            (, quorum,,,) = abi.decode(logs[i].data, (uint64, bool, address[], address[], uint256[]));
        }
        assertEq(schedule.setCount(), 2);
    }

    /// Erin's keyed quarter of the stake makes the quorum, but the set keeps alice and bob, the larger
    /// members, at their votes while they are between keys, so erin alone passes no vote.
    function test_AQuorumKeepsTheMembersBetweenKeys() public {
        assertTrue(_quorumWhileBetweenKeys(), "erin's 25% makes the quorum");
        (address[] memory members, uint64 effectiveAt, uint256[] memory weights) = _members(2);
        assertEq(members.length, 3);
        assertEq(members[0], erin);
        assertEq(members[1], alice);
        assertEq(members[2], bob);
        assertEq(weights[0], 250_000e18);
        assertEq(weights[1], 300_000e18);
        assertEq(weights[2], 250_000e18);

        vm.warp(effectiveAt);
        vm.prank(erin);
        uint256 id = votes.propose(command, true);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
        assertEq(proposal.setId, 2);
        assertFalse(proposal.passed, "erin's weight passes nothing");
    }

    /// The carry lasts an epoch: the next close with the quorum, an epoch on, while alice and bob still
    /// hold no key, adopts erin alone, and once alice holds a key again a close adopts her by her votes.
    function test_AQuorumCarriesAMemberBetweenKeysForAnEpoch() public {
        assertTrue(_quorumWhileBetweenKeys());
        _closeNext();
        (address[] memory members,, uint256[] memory weights) = _members(3);
        assertEq(members.length, 1);
        assertEq(members[0], erin);
        assertEq(weights[0], 250_000e18);

        _registerStandby(alice);
        _closeNext();
        (members,, weights) = _members(4);
        assertEq(members.length, 2);
        assertEq(members[0], alice);
        assertEq(members[1], erin);
        assertEq(weights[0], 300_000e18);
        assertEq(weights[1], 250_000e18);
    }

    /// A close nobody called until an hour before the next epoch ended lets the next close follow an
    /// hour later. Erin's keyed quarter closes both while alice and bob are between keys: the second
    /// carries them too, since an epoch length has not passed since the first did, so it cannot drop
    /// them before the first set ever takes effect; the close an epoch later does.
    function test_AQuorumSoonAfterALateCloseKeepsTheMembersBetweenKeys() public {
        _candidate(alice, 300_000e18, 1);
        _candidate(bob, 250_000e18, 2);
        _candidate(erin, 250_000e18, 3);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _delegate(erin, erin);
        _stake(holders, 200_000e18);
        _setRotation(true);
        _closeNext();
        _moveReleases();
        (uint64 epochLength,,,,,,) = registry.parameters();
        uint256 late = uint256(registry.openEpochStart()) + 2 * uint256(epochLength) - 1 hours;
        vm.warp(late);
        vm.prank(erin);
        registry.closeEpoch();
        vm.warp(late + 1 hours);
        vm.prank(erin);
        registry.closeEpoch();
        for (uint32 setId = 2; setId <= 3; ++setId) {
            (address[] memory members,, uint256[] memory weights) = _members(setId);
            assertEq(members.length, 3);
            assertEq(members[1], alice);
            assertEq(members[2], bob);
            assertEq(weights[1], 300_000e18);
            assertEq(weights[2], 250_000e18);
        }
        (, uint64 second,,) = schedule.setOf(2);
        (, uint64 third,,) = schedule.setOf(3);
        assertLt(third - second, 1 days, "the third set follows the second within the day");

        _closeNext();
        assertGe(block.timestamp, late + epochLength, "an epoch length after the first carry");
        (address[] memory fourth,,) = _members(4);
        assertEq(fourth.length, 1);
        assertEq(fourth[0], erin);
    }

    /// A key registered after a close carried a member with none starts a new run: alice registers one an
    /// hour after erin's close carried her, moves releases again before the next close, and that close
    /// carries her once more while bob, who held no key throughout, leaves. A close that adopts her by
    /// her votes ends a run too, so a third move is carried again, and the close an epoch on drops her.
    function test_AKeyRegisteredSinceACarryStartsANewRun() public {
        assertTrue(_quorumWhileBetweenKeys());
        vm.warp(block.timestamp + 1 hours);
        _registerStandby(alice);
        _moveAliceAsTheEpochEnds(21);
        _closeNext();
        (address[] memory members,,) = _members(3);
        assertEq(members.length, 2);
        assertEq(members[0], erin);
        assertEq(members[1], alice);

        _registerStandby(alice);
        _closeNext();
        (members,,) = _members(4);
        assertEq(members.length, 2);
        assertEq(members[0], alice, "adopted by her votes");
        _moveAliceAsTheEpochEnds(22);
        _closeNext();
        (members,,) = _members(5);
        assertEq(members.length, 2);
        assertEq(members[1], alice, "a new run");
        _closeNext();
        (members,,) = _members(6);
        assertEq(members.length, 1);
        assertEq(members[0], erin);
    }

    /// Moving back to the release its key was registered for, which the timelock approves again, leaves
    /// alice a promotable key with no registration, and ends her run too: a retried move before the next
    /// close is carried afresh.
    function test_AMoveBackToAKeyedReleaseStartsANewRun() public {
        assertTrue(_quorumWhileBetweenKeys());
        vm.warp(block.timestamp + 1 hours);
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, true);
        vm.prank(alice);
        registry.setRelease(_digest(1), CONFIG);
        assertTrue(schedule.promotable(alice) != address(0), "her first key is promotable again");
        _moveAliceAsTheEpochEnds(21);
        _closeNext();
        (address[] memory members,,) = _members(3);
        assertEq(members.length, 2);
        assertEq(members[1], alice, "a new run");
        _closeNext();
        (members,,) = _members(4);
        assertEq(members.length, 1);
        assertEq(members[0], erin);
    }

    /// Leaving and registering again on the release her kept key was registered for, which the timelock
    /// approves again, leaves alice a promotable key with no new key, and ends her run too: a move
    /// before the next close is carried afresh. The test's 3-day bond cooldown lets her return within
    /// the epoch.
    function test_ARegistrationBackOnAKeyedReleaseStartsANewRun() public {
        assertTrue(_quorumWhileBetweenKeys());
        vm.warp(block.timestamp + 1 hours);
        vm.prank(alice);
        registry.deregister();
        // Her bond frees a cooldown after she leaves; the launch's cooldown outlasts an epoch.
        vm.warp(block.timestamp + COOLDOWN);
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, true);
        bytes memory authorization = _authorization(alice);
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), authorization);
        assertTrue(schedule.promotable(alice) != address(0), "her first key is promotable again");
        _moveAliceAsTheEpochEnds(11);
        _closeNext();
        (address[] memory members,,) = _members(3);
        assertEq(members.length, 2);
        assertEq(members[1], alice, "a new run");
    }

    /// Registering again on the release her kept key was registered for before the timelock approves it
    /// lists alice, and leaves her a promotable key, only once it does: that approval ends her run.
    function test_AnApprovalBackOnAKeyedReleaseStartsANewRun() public {
        assertTrue(_quorumWhileBetweenKeys());
        vm.warp(block.timestamp + 1 hours);
        vm.prank(alice);
        registry.deregister();
        vm.warp(block.timestamp + COOLDOWN);
        bytes memory authorization = _authorization(alice);
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), authorization);
        assertEq(schedule.promotable(alice), address(0), "her release is not approved yet");
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, true);
        assertTrue(schedule.promotable(alice) != address(0), "her first key is promotable again");
        _moveAliceAsTheEpochEnds(11);
        _closeNext();
        (address[] memory members,,) = _members(3);
        assertEq(members.length, 2);
        assertEq(members[1], alice, "a new run");
    }

    /// @dev Moves alice to newly approved release `seed` ten minutes before the open epoch ends.
    function _moveAliceAsTheEpochEnds(uint256 seed) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(seed), CONFIG, true);
        (uint64 epochLength,,,,,,) = registry.parameters();
        vm.warp(uint256(registry.openEpochStart()) + epochLength - 10 minutes);
        vm.prank(alice);
        registry.setRelease(_digest(seed), CONFIG);
        assertEq(schedule.promotable(alice), address(0));
    }

    /// Below the quorum the set is carried whole, the members between keys with it, so dave's keyed
    /// 1.2% never becomes the set; a later close with the quorum then carries them no further, since
    /// sets have carried them with no key for an epoch and more.
    function test_BelowTheQuorumTheMembersBetweenKeysStay() public {
        _closedWhileBetweenKeys();
        _closeNext();
        (address[] memory members,,) = _members(3);
        assertEq(members.length, 3);
        assertEq(members[0], alice);
        assertEq(members[1], bob);
        assertEq(members[2], dave);

        // The holders' 448k behind dave makes the quorum.
        _delegate(holders, dave);
        _closeNext();
        uint256[] memory weights;
        (members,, weights) = _members(4);
        assertEq(members.length, 1);
        assertEq(members[0], dave);
        assertEq(weights[0], 460_000e18);
    }

    /// The candidates between keys hold the votes that would make the quorum, so the close has none
    /// and carries the set in effect: all three members at their votes.
    function test_ACloseWhileMembersAreBetweenKeysCarriesTheSet() public {
        uint32 setId = _closedDuringReleaseMoves();
        (,, address[] memory members, uint256[] memory weights) = schedule.setOf(setId);
        assertEq(members.length, 3);
        (address[] memory before,, uint256[] memory earlier) = _members(1);
        assertEq(members, before);
        assertEq(weights, earlier);
    }

    function test_AVoteDuringACarriedSetNeedsItsMembersWeight() public {
        uint32 setId = _closedDuringReleaseMoves();
        assertLt(staking.votes(dave) * 10_000 / staking.totalStaked(), 200, "dave holds under 2%");
        vm.prank(dave);
        uint256 id = votes.propose(command, true);
        (ParameterVote.Proposal memory proposal,) = votes.proposalOf(id);
        assertEq(proposal.setId, setId);
        assertFalse(proposal.passed, "dave's own weight passes nothing");
        vm.prank(alice);
        votes.vote(id);
        vm.prank(bob);
        votes.vote(id);
        (proposal,) = votes.proposalOf(id);
        assertTrue(proposal.passed);
    }

    /// Once both have registered their new keys, the next close has the quorum and adopts all three.
    function test_TheNextCloseAfterTheNewKeysAdoptsByVotes() public {
        _closedDuringReleaseMoves();
        _closeNext();
        (,, address[] memory members, uint256[] memory weights) = schedule.setOf(3);
        assertEq(members.length, 3);
        assertEq(weights[0], 300_000e18);
        assertEq(weights[1], 240_000e18);
        assertEq(weights[2], 12_000e18);
    }

    function _members(uint32 setId)
        internal
        view
        returns (address[] memory members, uint64 effectiveAt, uint256[] memory weights)
    {
        (, effectiveAt, members, weights) = schedule.setOf(setId);
    }

    /// A listed candidate under the eligibility share is never adopted, so its votes do not count
    /// toward the quorum either.
    function test_TheVotesOfAnIneligibleCandidateDoNotMakeTheQuorum() public {
        OperatorRegistry.Parameters memory p = _parameters();
        (p.epochLength, p.finalityMargin, p.eligibilityBps, p.quorumBps) = (14 days, 1 days, 2_000, 2_500);
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (p)));
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 15_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        // 45k of 145k would meet the 25% quorum; alice's 30k, the only eligible, does not.
        _stake(holders, 100_000e18);
        assertFalse(registry.isEligible(bob), "bob is under the 20% eligibility share");
        _setRotation(true);
        _closeNext();
        assertEq(schedule.setCount(), 0, "no quorum and no set to carry");
    }

    /// With no eligibility share, a carried member whose votes are gone leaves the set, which never
    /// weighs a member at zero; with every member's votes gone the close adopts an empty set.
    function test_ACarriedMemberWithNoVotesLeavesTheSet() public {
        OperatorRegistry.Parameters memory p = _parameters();
        (p.epochLength, p.finalityMargin, p.eligibilityBps, p.quorumBps) = (14 days, 1 days, 0, 2_500);
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (p)));
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        _closeNext();
        assertEq(schedule.setCount(), 1);

        // 150k delegated to no one leaves 30k of 200k under the quorum, and bob's votes at zero.
        _stake(holders, 150_000e18);
        vm.prank(bob);
        staking.undelegate(bob, 20_000e18);
        assertTrue(registry.isEligible(bob), "bob stays eligible with no votes");
        _closeNext();
        (,, address[] memory members, uint256[] memory weights) = schedule.setOf(2);
        assertEq(members.length, 1);
        assertEq(members[0], alice);
        assertEq(weights[0], 30_000e18);

        vm.prank(alice);
        staking.undelegate(alice, 30_000e18);
        _closeNext();
        (,, members,) = schedule.setOf(3);
        assertEq(members.length, 0, "an empty set replaces the one whose members' votes are gone");
    }
}
