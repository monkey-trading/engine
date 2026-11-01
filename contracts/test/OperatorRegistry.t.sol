// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {Staking} from "../src/Staking.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// Candidacy, release approval, the election close, and the rotation switch.
contract OperatorRegistryTest is RegistryTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal voter = makeAddr("voter");

    function setUp() public {
        _deployRegistry();
    }

    function test_DeploymentStartsWithTheDeployerInEffectAndRotationOff() public view {
        OperatorRegistry.Operator memory operator = registry.operatorInEffect();
        assertEq(operator.candidate, deployerKey);
        assertEq(operator.adminKey, deployerKey);
        assertEq(operator.release, _release(0));
        assertEq(operator.since, block.timestamp);
        assertFalse(registry.rotationEnabled());
        assertEq(staking.registry(), address(registry));
        assertEq(address(staking.monkey()), address(monkey));
        assertEq(staking.cooldown(), COOLDOWN);
    }

    function test_ConstructorRefusesAnInvalidSetup() public {
        OperatorRegistry.InitialOperator memory initial =
            OperatorRegistry.InitialOperator(deployerKey, _digest(0), CONFIG);
        vm.expectRevert(OperatorRegistry.InvalidGovernance.selector);
        new OperatorRegistry(
            monkey,
            COOLDOWN,
            DISTRIBUTION_EPOCH,
            TimelockController(payable(alice)),
            initial,
            _parameters(),
            _minimums(),
            _maximums()
        );
        vm.expectRevert(OperatorRegistry.ZeroAddress.selector);
        new OperatorRegistry(
            monkey,
            COOLDOWN,
            DISTRIBUTION_EPOCH,
            governance,
            OperatorRegistry.InitialOperator(address(0), _digest(0), CONFIG),
            _parameters(),
            _minimums(),
            _maximums()
        );
        vm.expectRevert(OperatorRegistry.InvalidLaunchDigest.selector);
        new OperatorRegistry(
            monkey,
            COOLDOWN,
            DISTRIBUTION_EPOCH,
            governance,
            OperatorRegistry.InitialOperator(deployerKey, hex"01", CONFIG),
            _parameters(),
            _minimums(),
            _maximums()
        );
        OperatorRegistry.Parameters memory zeroEpoch = _minimums();
        zeroEpoch.epochLength = 0;
        vm.expectRevert(OperatorRegistry.InvalidBounds.selector);
        new OperatorRegistry(
            monkey, COOLDOWN, DISTRIBUTION_EPOCH, governance, initial, _parameters(), zeroEpoch, _maximums()
        );
        OperatorRegistry.Parameters memory wideCap = _maximums();
        wideCap.maxCandidates = uint16(registry.CANDIDATE_LIMIT() + 1);
        vm.expectRevert(OperatorRegistry.InvalidBounds.selector);
        new OperatorRegistry(
            monkey, COOLDOWN, DISTRIBUTION_EPOCH, governance, initial, _parameters(), _minimums(), wideCap
        );
        OperatorRegistry.Parameters memory overShare = _maximums();
        overShare.quorumBps = 10_001;
        vm.expectRevert(OperatorRegistry.InvalidBounds.selector);
        new OperatorRegistry(
            monkey, COOLDOWN, DISTRIBUTION_EPOCH, governance, initial, _parameters(), _minimums(), overShare
        );
        OperatorRegistry.Parameters memory outside = _parameters();
        outside.minSelfBond = _maximums().minSelfBond + 1;
        vm.expectRevert(OperatorRegistry.ParameterOutOfBounds.selector);
        new OperatorRegistry(
            monkey, COOLDOWN, DISTRIBUTION_EPOCH, governance, initial, outside, _minimums(), _maximums()
        );
    }

    function test_RegistrationRecordsTheCandidateAndLocksItsBond() public {
        _stake(alice, 5_000e18);
        OperatorRegistry.Registration memory registration = _registration(_key(alice), 2_000e18, 1);
        bytes memory authorization = _authorization(alice);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.CandidateRegistered(
            alice,
            _key(alice),
            _release(1),
            registration.enclave,
            2_000e18,
            registration.launchDigest,
            CONFIG,
            registration.apiHostname,
            registration.handoverEndpoint
        );
        vm.prank(alice);
        registry.register(registration, authorization);
        OperatorRegistry.Candidate memory candidate = registry.candidateOf(alice);
        assertEq(candidate.adminKey, _key(alice));
        assertEq(candidate.bond, 2_000e18);
        assertEq(candidate.release, _release(1));
        assertEq(candidate.apiHostname, "api.candidate.example");
        assertEq(candidate.handoverEndpoint, "handover.candidate.example:9443");
        assertEq(registry.candidateCount(), 0, "unlisted until its release is approved");
        assertEq(registry.keyHolder(_key(alice)), alice);
        assertEq(staking.lockedBond(alice), 2_000e18);
        // Registration alone is not a vote: the stake abstains until it is delegated.
        assertEq(staking.votes(alice), 0);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, true))
        );
        assertEq(registry.candidateCount(), 1);
        assertEq(registry.candidateList(0), alice);
    }

    function test_RegistrationRefusesEachInvalidCandidacy() public {
        _stake(alice, 5_000e18);
        // Every check past the signature must see a valid one, so the caller's own key authorizes
        // each registration below.
        bytes memory authorization = _authorization(alice);
        vm.startPrank(alice);
        vm.expectRevert(OperatorRegistry.ZeroAddress.selector);
        registry.register(_registration(address(0), MIN_BOND, 1), "");
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.BondBelowMinimum.selector, MIN_BOND - 1, MIN_BOND));
        registry.register(_registration(_key(alice), MIN_BOND - 1, 1), authorization);
        vm.expectRevert(abi.encodeWithSelector(Staking.InsufficientStake.selector, 5_000e18, 5_001e18));
        registry.register(_registration(_key(alice), 5_001e18, 1), authorization);
        OperatorRegistry.Registration memory bad = _registration(_key(alice), MIN_BOND, 1);
        bad.launchDigest = new bytes(47);
        vm.expectRevert(OperatorRegistry.InvalidLaunchDigest.selector);
        registry.register(bad, authorization);
        bad = _registration(_key(alice), MIN_BOND, 1);
        bad.apiHostname = "";
        vm.expectRevert(OperatorRegistry.InvalidEndpoint.selector);
        registry.register(bad, authorization);
        bad = _registration(_key(alice), MIN_BOND, 1);
        bad.handoverEndpoint = string(new bytes(513));
        vm.expectRevert(OperatorRegistry.InvalidEndpoint.selector);
        registry.register(bad, authorization);
        registry.register(_registration(_key(alice), MIN_BOND, 1), authorization);
        vm.expectRevert(OperatorRegistry.AlreadyRegistered.selector);
        registry.register(_registration(_key(bob), MIN_BOND, 1), authorization);
        vm.stopPrank();

        // Two candidates cannot share an admin key, even with the key's authorization: alice's key
        // authorizes bob's registration, and the reservation still refuses the second holder. The
        // signature is made before the expectRevert: the view call would otherwise satisfy it.
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(_adminKey(alice), registry.registrationDigest(bob, _key(alice)));
        _stake(bob, 5_000e18);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.AdminKeyTaken.selector, alice));
        vm.prank(bob);
        registry.register(_registration(_key(alice), MIN_BOND, 2), abi.encodePacked(r, s, v));
    }

    function test_RegistrationRefusesWithoutTheAdminKeysAuthorization() public {
        _stake(bob, 5_000e18);
        // A staker front-running a candidate's advertised key with its own self-bond cannot register
        // it: the signature must be the admin key's own. The signature is made before the
        // expectRevert: the view call would otherwise satisfy it.
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(_adminKey(bob), registry.registrationDigest(bob, _key(alice)));
        vm.expectRevert(OperatorRegistry.UnauthorizedAdminKey.selector);
        vm.prank(bob);
        registry.register(_registration(_key(alice), MIN_BOND, 1), abi.encodePacked(r, s, v));
        assertEq(registry.candidateOf(bob).adminKey, address(0), "the refusal registers nothing");
    }

    function test_TheAdminKeysAuthorizationBindsTheCandidate() public {
        _stake(bob, 5_000e18);
        // Alice's authorization of alice's own registration does not serve bob registering the key.
        bytes memory authorization = _authorization(alice);
        vm.expectRevert(OperatorRegistry.UnauthorizedAdminKey.selector);
        vm.prank(bob);
        registry.register(_registration(_key(alice), MIN_BOND, 1), authorization);
    }

    function test_TheAdminKeysAuthorizationBindsTheRegistryAndChain() public {
        assertEq(
            registry.registrationDigest(alice, _key(alice)),
            _registrationDigest(registry, alice, _key(alice)),
            "the digest matches the hand-built typed data"
        );
        // A digest built for another chain or another registry does not authorize registration here.
        _stake(bob, 5_000e18);
        vm.chainId(block.chainid + 1);
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(_adminKey(alice), registry.registrationDigest(bob, _key(alice)));
        vm.chainId(block.chainid - 1);
        vm.expectRevert(OperatorRegistry.UnauthorizedAdminKey.selector);
        vm.prank(bob);
        registry.register(_registration(_key(alice), MIN_BOND, 1), abi.encodePacked(r, s, v));
    }

    function test_UnapprovedRegistrationsDoNotFillTheCandidateCap() public {
        // Twice the cap registers freely while none of them counts toward it.
        for (uint256 i = 0; i < 2 * MAX_CANDIDATES; ++i) {
            address candidate = makeAddr(string(abi.encode(i)));
            _stake(candidate, MIN_BOND);
            vm.prank(candidate);
            registry.register(_registration(_key(candidate), MIN_BOND, i + 1), _authorization(candidate));
            assertEq(registry.candidateCount(), 0);
        }
        // Approving fills the cap; one more approval reverts until an approval is withdrawn.
        for (uint256 i = 0; i < MAX_CANDIDATES; ++i) {
            address candidate = makeAddr(string(abi.encode(i)));
            _govern(
                address(registry),
                abi.encodeCall(OperatorRegistry.setReleaseApproval, (candidate, _digest(i + 1), CONFIG, true))
            );
        }
        assertEq(registry.candidateCount(), MAX_CANDIDATES);
        address extra = makeAddr(string(abi.encode(uint256(2 * MAX_CANDIDATES))));
        _stake(extra, MIN_BOND);
        vm.prank(extra);
        registry.register(_registration(_key(extra), MIN_BOND, 99), _authorization(extra));
        // The full cap refuses the approval; the timelock must schedule before the execute reverts.
        bytes memory approval = abi.encodeCall(OperatorRegistry.setReleaseApproval, (extra, _digest(99), CONFIG, true));
        vm.prank(multisig);
        governance.schedule(address(registry), 0, approval, bytes32(0), bytes32("extra approval"), GOVERNANCE_DELAY);
        vm.warp(block.timestamp + GOVERNANCE_DELAY);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, MAX_CANDIDATES));
        governance.execute(address(registry), 0, approval, bytes32(0), bytes32("extra approval"));
        _govern(
            address(registry),
            abi.encodeCall(
                OperatorRegistry.setReleaseApproval, (makeAddr(string(abi.encode(0))), _digest(1), CONFIG, false)
            )
        );
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (extra, _digest(99), CONFIG, true))
        );
        assertEq(registry.candidateCount(), MAX_CANDIDATES);
        assertEq(registry.candidateOf(extra).adminKey, _key(extra), "unlisted candidates stay registered");
    }

    function test_WithdrawnApprovalDelistsACandidateThatStaysRegistered() public {
        _candidate(alice, MIN_BOND, 1);
        assertEq(registry.candidateCount(), 1);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, false))
        );
        assertEq(registry.candidateCount(), 0);
        assertEq(registry.candidateOf(alice).adminKey, _key(alice));
        // Its candidacy is no longer a vote: a close sees none.
        _delegate(alice, alice);
        assertFalse(registry.isEligible(alice));
    }

    function test_ApprovalOfAReleaseTheCandidateDidNotRegisterNeitherListsNorDelistsIt() public {
        _candidate(alice, MIN_BOND, 1);
        // Approving another release neither takes a second seat nor disturbs the listed one.
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(2), CONFIG, true))
        );
        assertEq(registry.candidateCount(), 1);
        assertEq(registry.candidateList(0), alice);
        // Withdrawing another release does not delist it.
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(2), CONFIG, false))
        );
        assertEq(registry.candidateCount(), 1);
        // Once its own release is withdrawn it is unlisted, and another release cannot list it.
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, false))
        );
        assertEq(registry.candidateCount(), 0);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(2), CONFIG, true))
        );
        assertEq(registry.candidateCount(), 0);
        assertFalse(registry.isEligible(alice));
    }

    function test_AReregisteredCandidateWhoseReleaseIsStillApprovedRelistsAtOnce() public {
        _stake(alice, 2 * MIN_BOND);
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), _authorization(alice));
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, true))
        );
        vm.prank(alice);
        registry.deregister();
        vm.warp(block.timestamp + COOLDOWN);
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), _authorization(alice));
        assertEq(registry.candidateCount(), 1);
        assertEq(registry.candidateList(0), alice);
    }

    function test_ASelfBondCannotBeUnstakedWhileRegisteredNorForOneCooldownAfter() public {
        _stake(alice, 3_000e18);
        vm.prank(alice);
        registry.register(_registration(_key(alice), 2_000e18, 1), _authorization(alice));

        vm.startPrank(alice);
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 2_000e18));
        staking.startUnstake(1_000e18 + 1);
        // Stake above the bond leaves freely.
        staking.startUnstake(1_000e18);
        registry.deregister();
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 2_000e18));
        staking.startUnstake(1);
        vm.stopPrank();

        // Registering again waits for the released bond too.
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 2_000e18));
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), _authorization(alice));

        vm.warp(block.timestamp + COOLDOWN - 1);
        vm.expectRevert(abi.encodeWithSelector(Staking.SelfBondLocked.selector, 2_000e18));
        vm.prank(alice);
        staking.startUnstake(1);
        vm.warp(block.timestamp + 1);
        assertEq(staking.lockedBond(alice), 0);
        vm.prank(alice);
        staking.startUnstake(2_000e18);
        assertEq(staking.stakes(alice), 0);
    }

    function test_OnlyTheTimelockApprovesReleasesSwitchesRotationAndSetsParameters() public {
        vm.expectRevert(OperatorRegistry.UnauthorizedGovernance.selector);
        registry.setReleaseApproval(alice, _digest(1), CONFIG, true);
        vm.expectRevert(OperatorRegistry.UnauthorizedGovernance.selector);
        registry.setRotation(true);
        vm.expectRevert(OperatorRegistry.UnauthorizedGovernance.selector);
        registry.setParameters(_parameters());

        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, true))
        );
        assertTrue(registry.releaseApproved(keccak256(abi.encode(alice, _release(1)))));
        _setRotation(true);
        assertTrue(registry.rotationEnabled());
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.RotationUnchanged.selector, true));
        vm.prank(address(governance));
        registry.setRotation(true);
    }

    function test_ParametersChangeOnlyWithinTheirBounds() public {
        OperatorRegistry.Parameters memory changed = _parameters();
        changed.marginBps = 2_500;
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (changed)));
        (,,, uint16 marginBps,,,,,) = registry.parameters();
        assertEq(marginBps, 2_500);
        changed.epochLength = _maximums().epochLength + 1;
        vm.expectRevert(OperatorRegistry.ParameterOutOfBounds.selector);
        vm.prank(address(governance));
        registry.setParameters(changed);
        changed = _parameters();
        changed.maxCandidates = 0;
        vm.expectRevert(OperatorRegistry.ParameterOutOfBounds.selector);
        vm.prank(address(governance));
        registry.setParameters(changed);
    }

    function test_EligibilityNeedsTheCandidatesOwnApprovedReleaseBondAndShare() public {
        _stake(alice, 10_000e18);
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), _authorization(alice));
        _delegate(alice, alice);
        // Approving the same release for another candidate does not make this one eligible.
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (bob, _digest(1), CONFIG, true)));
        assertFalse(registry.isEligible(alice));
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, true))
        );
        assertTrue(registry.isEligible(alice));

        // Below the eligibility share of total stake: 10k of 210k is under 5%, 10.6k of 210.6k is not.
        _stake(voter, 200_000e18);
        assertFalse(registry.isEligible(alice));
        _stake(alice, 600e18);
        assertTrue(registry.isEligible(alice));

        // A raised minimum self-bond, then a withdrawn approval, each make it ineligible.
        OperatorRegistry.Parameters memory raised = _parameters();
        raised.minSelfBond = MIN_BOND + 1;
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (raised)));
        assertFalse(registry.isEligible(alice));
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (_parameters())));
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, false))
        );
        assertFalse(registry.isEligible(alice));
        assertFalse(registry.isEligible(carol));
    }

    function test_AnEpochClosesOnlyOnceEndedAndSkipsUnclosedOnes() public {
        uint256 endsAt = uint256(registry.openEpochStart()) + EPOCH;
        vm.warp(endsAt - 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EpochNotEnded.selector, endsAt));
        registry.closeEpoch();
        vm.warp(endsAt);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.EpochClosed(0);
        assertEq(registry.closeEpoch(), 0);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EpochNotEnded.selector, endsAt + EPOCH));
        registry.closeEpoch();
        // Three epochs pass unclosed: the close closes the latest that ended.
        vm.warp(endsAt + 3 * EPOCH + 5);
        assertEq(registry.closeEpoch(), 3);
        assertEq(registry.openEpoch(), 4);
        assertEq(registry.openEpochStart(), endsAt + 3 * EPOCH);
    }

    function test_WhileRotationIsOffACloseElectsNothing() public {
        _candidate(alice, 10_000e18, 1);
        _delegate(alice, alice);
        vm.warp(block.timestamp + MIN_TERM);
        vm.recordLogs();
        _closeNext();
        assertEq(vm.getRecordedLogs().length, 1, "only EpochClosed");
        assertEq(_inEffect(), deployerKey);
        assertEq(registry.pendingElection().effectiveAt, 0);
    }

    function test_TheTopEligibleCandidateIsElectedToTakeEffectAfterTheNotice() public {
        _candidate(alice, 10_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _stake(voter, 5_000e18);
        _delegate(voter, alice);
        _setRotation(true);
        vm.warp(block.timestamp + MIN_TERM);
        uint256 closeAt = block.timestamp;
        uint64 epoch = uint64((closeAt - registry.openEpochStart()) / EPOCH) + registry.openEpoch() - 1;
        vm.expectEmit(address(registry));
        emit OperatorRegistry.Elected(epoch, bob, _key(bob), _release(2), uint64(closeAt + 7 days + FINALITY_MARGIN));
        assertEq(registry.closeEpoch(), epoch);
        OperatorRegistry.Election memory pending = registry.pendingElection();
        assertEq(pending.candidate, bob);
        assertEq(pending.effectiveAt, closeAt + 7 days + FINALITY_MARGIN);

        vm.warp(pending.effectiveAt - 1);
        assertEq(_inEffect(), deployerKey);
        vm.warp(pending.effectiveAt);
        OperatorRegistry.Operator memory operator = registry.operatorInEffect();
        assertEq(operator.candidate, bob);
        assertEq(operator.adminKey, _key(bob));
        assertEq(operator.release, _release(2));
        assertEq(operator.since, pending.effectiveAt);
        assertEq(registry.pendingElection().effectiveAt, 0);
    }

    function test_NoCloseElectsWhileAnElectionWaitsToTakeEffect() public {
        _elect(bob);
        // Alice now leads by far, but bob's election has not yet taken effect.
        _candidate(alice, 100_000e18, 1);
        _delegate(alice, alice);
        vm.recordLogs();
        _closeNext();
        assertEq(vm.getRecordedLogs().length, 1, "only EpochClosed");
        assertEq(registry.pendingElection().candidate, bob);
    }

    function test_TheRoleMovesOnlyAfterTheIncumbentsMinimumTerm() public {
        uint64 effectiveAt = _elect(bob);
        _candidate(alice, 100_000e18, 1);
        _delegate(alice, alice);
        // Bob's term starts at its effective time; until it has run, alice cannot replace it.
        vm.warp(uint256(effectiveAt) + MIN_TERM - EPOCH);
        _closeNext();
        assertEq(registry.pendingElection().effectiveAt, 0);
        vm.warp(uint256(effectiveAt) + MIN_TERM);
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
    }

    function test_AChallengerMustBeatTheIncumbentByTheMargin() public {
        uint64 effectiveAt = _elect(bob);
        vm.warp(uint256(effectiveAt) + MIN_TERM);
        uint256 bobVotes = staking.votes(bob);
        // Exactly 10% more is not enough; one base unit beyond it is.
        _candidate(alice, bobVotes * 11 / 10, 1);
        _delegate(alice, alice);
        _closeNext();
        assertEq(registry.pendingElection().effectiveAt, 0);
        _stake(alice, 1);
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
    }

    function test_AnIneligibleIncumbentCountsNoVotes() public {
        uint64 effectiveAt = _elect(bob);
        vm.warp(uint256(effectiveAt) + MIN_TERM);
        _candidate(alice, 15_000e18, 1);
        _delegate(alice, alice);
        _closeNext();
        assertEq(registry.pendingElection().effectiveAt, 0, "bob leads alice");
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (bob, _digest(2), CONFIG, false))
        );
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
    }

    function test_TheRoleMovesOnlyWithAQuorumDelegated() public {
        _candidate(alice, 10_000e18, 1);
        _delegate(alice, alice);
        _setRotation(true);
        vm.warp(block.timestamp + MIN_TERM);
        // 10k of 60k delegated is under the 20% quorum; delegation to an address that is not a
        // candidate counts for nobody.
        _stake(voter, 50_000e18);
        _delegate(voter, carol);
        _closeNext();
        assertEq(registry.pendingElection().effectiveAt, 0);
        _stake(alice, 2_500e18);
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
    }

    function test_ATieGoesToTheEarlierRegistration() public {
        _candidate(bob, 10_000e18, 2);
        _candidate(alice, 10_000e18, 1);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        vm.warp(block.timestamp + MIN_TERM);
        _closeNext();
        assertEq(registry.pendingElection().candidate, bob);
    }

    function test_DelegationFollowsStakeAndCanChangeAtAnyTime() public {
        _candidate(alice, 10_000e18, 1);
        _candidate(bob, 10_000e18, 2);
        _stake(voter, 30_000e18);
        _delegate(voter, alice);
        assertEq(staking.votes(alice), 30_000e18);
        vm.prank(voter);
        staking.startUnstake(5_000e18);
        assertEq(staking.votes(alice), 25_000e18, "unstaking stake stops voting");
        _delegate(voter, bob);
        assertEq(staking.votes(alice), 0);
        assertEq(staking.votes(bob), 25_000e18);
        _stake(voter, 1_000e18);
        assertEq(staking.votes(bob), 26_000e18);
        _delegate(voter, address(0));
        assertEq(staking.votes(bob), 0);
        assertEq(staking.totalStaked(), 46_000e18);
    }

    function test_TurningRotationOffCancelsAnElectionNotYetInEffect() public {
        _elect(bob);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.RotationChanged(false, true);
        vm.prank(address(governance));
        registry.setRotation(false);
        assertEq(registry.pendingElection().effectiveAt, 0);
        vm.warp(block.timestamp + 30 days);
        assertEq(_inEffect(), deployerKey);
    }

    function test_TurningRotationOffKeepsAnElectionAlreadyInEffect() public {
        uint64 effectiveAt = _elect(bob);
        vm.warp(effectiveAt);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.RotationChanged(false, false);
        vm.prank(address(governance));
        registry.setRotation(false);
        assertEq(_inEffect(), bob);
    }

    function test_TheOperatorInEffectAndAPendingWinnerCannotLeave() public {
        _elect(bob);
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
        OperatorRegistry.Election memory pending = registry.pendingElection();
        vm.warp(pending.effectiveAt);
        vm.expectRevert(OperatorRegistry.OperatorCannotLeave.selector);
        vm.prank(bob);
        registry.deregister();
        vm.expectRevert(OperatorRegistry.NotRegistered.selector);
        vm.prank(carol);
        registry.deregister();
    }

    function test_DeregisteringKeepsTheOtherCandidatesListed() public {
        _candidate(alice, MIN_BOND, 1);
        _candidate(bob, MIN_BOND, 2);
        _candidate(carol, MIN_BOND, 3);
        vm.prank(alice);
        registry.deregister();
        assertEq(registry.candidateCount(), 2);
        assertEq(registry.candidateList(0), carol);
        assertEq(registry.candidateList(1), bob);
        assertEq(registry.keyHolder(_key(alice)), address(0));
        assertEq(registry.candidateOf(alice).adminKey, address(0));
        vm.prank(carol);
        registry.deregister();
        vm.prank(bob);
        registry.deregister();
        assertEq(registry.candidateCount(), 0);
    }

    function test_AnElectionSnapshotsTheIncumbentsHandoverSource() public {
        // The operator in effect is a registered candidate: the first election snapshots its
        // endpoint, not an empty one.
        _candidate(deployerKey, MIN_BOND, 1);
        _delegate(deployerKey, deployerKey);
        assertEq(registry.handoverSource().handoverEndpoint, "");
        uint64 effectiveAt = _elect(bob);
        OperatorRegistry.HandoverSource memory source = registry.handoverSource();
        assertEq(source.candidate, deployerKey);
        // The release the operator in effect ran: the initial release, not the one its candidacy
        // registered.
        assertEq(source.release, _release(0));
        assertEq(source.apiHostname, "api.candidate.example");
        assertEq(source.handoverEndpoint, "handover.candidate.example:9443");
        // The snapshot exists from the election's making, without a transaction to settle it, and
        // survives the election's taking effect and the incumbent's departure.
        vm.warp(effectiveAt);
        vm.prank(deployerKey);
        registry.deregister();
        source = registry.handoverSource();
        assertEq(source.candidate, deployerKey);
        assertEq(source.handoverEndpoint, "handover.candidate.example:9443");
        assertEq(_inEffect(), bob);
    }

    function test_TheNextElectionReplacesTheHandoverSourceWithItsOwnIncumbent() public {
        _candidate(deployerKey, MIN_BOND, 1);
        _delegate(deployerKey, deployerKey);
        uint64 effectiveAt = _elect(bob);
        vm.warp(effectiveAt);
        assertEq(_inEffect(), bob);
        // Past bob's minimum term, a close elects alice; the snapshot now names bob, with bob's
        // own release, as the incumbent alice's successor hands over from.
        uint64 secondEffectiveAt = _electWith(alice, 30_000e18);
        assertGt(secondEffectiveAt, 0);
        OperatorRegistry.HandoverSource memory source = registry.handoverSource();
        assertEq(source.candidate, bob);
        assertEq(source.release, _release(2));
        assertEq(source.apiHostname, "api.candidate.example");
        assertEq(source.handoverEndpoint, "handover.candidate.example:9443");
    }

    function test_ANeverRegisteredIncumbentLeavesAnEmptyHandoverSource() public {
        // The initial operator is not a registered candidate: the first election snapshots no
        // endpoint, and the successor refuses to start until the runbook registers it.
        uint64 effectiveAt = _elect(bob);
        assertGt(effectiveAt, 0);
        OperatorRegistry.HandoverSource memory source = registry.handoverSource();
        assertEq(source.candidate, deployerKey);
        assertEq(source.handoverEndpoint, "");
        assertEq(source.apiHostname, "");
    }

    function test_TurningRotationOffClearsTheCancelledElectionsHandoverSource() public {
        _candidate(deployerKey, MIN_BOND, 1);
        _delegate(deployerKey, deployerKey);
        _elect(bob);
        assertEq(registry.handoverSource().candidate, deployerKey);
        _setRotation(false);
        assertEq(registry.handoverSource().candidate, address(0));
        assertEq(registry.handoverSource().handoverEndpoint, "");
    }

    function testFuzz_TheChallengerIsElectedExactlyWhenItBeatsTheMargin(uint256 incumbentStake, uint256 challengerStake)
        public
    {
        incumbentStake = bound(incumbentStake, MIN_BOND, 1e27);
        challengerStake = bound(challengerStake, MIN_BOND, 1e27);
        uint64 effectiveAt = _electWith(bob, incumbentStake);
        vm.warp(uint256(effectiveAt) + MIN_TERM);
        _candidate(alice, challengerStake, 1);
        _delegate(alice, alice);
        uint256 total = staking.totalStaked();
        bool eligible = challengerStake * 10_000 >= uint256(ELIGIBILITY_BPS) * total;
        bool incumbentEligible = incumbentStake * 10_000 >= uint256(ELIGIBILITY_BPS) * total;
        uint256 incumbentVotes = incumbentEligible ? incumbentStake : 0;
        bool beats = challengerStake * 10_000 > incumbentVotes * (10_000 + MARGIN_BPS);
        bool leads = challengerStake > incumbentStake || !incumbentEligible;
        _closeNext();
        assertEq(registry.pendingElection().candidate == alice, eligible && leads && beats);
    }

    /// @dev Elects `candidate`, staking and self-delegating 20k for it, past the deployer's term.
    function _elect(address candidate) internal returns (uint64) {
        return _electWith(candidate, 20_000e18);
    }

    function _electWith(address candidate, uint256 stake) internal returns (uint64) {
        _candidate(candidate, stake, 2);
        _delegate(candidate, candidate);
        if (!registry.rotationEnabled()) _setRotation(true);
        vm.warp(block.timestamp + MIN_TERM);
        _closeNext();
        OperatorRegistry.Election memory pending = registry.pendingElection();
        assertEq(pending.candidate, candidate);
        return pending.effectiveAt;
    }
}
