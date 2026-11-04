// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Vm} from "forge-std/Test.sol";

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {Vault} from "../src/Vault.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// A candidate's move to a release the timelock approved for it, the operator in effect's upgrade
/// through it while rotation is on, and how moves meet the timelock's installation of an operator.
contract ReleaseChangeTest is RegistryTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");

    function setUp() public {
        _deployRegistry();
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setVault, (address(vault))));
        _govern(address(vault), abi.encodeCall(Vault.setRegistry, (address(registry))));
    }

    function test_TheOperatorInEffectUpgradesWhileRotationIsOn() public {
        uint64 effectiveAt = _elect(bob, 2);
        vm.warp(effectiveAt);
        vault.syncRelease();
        assertEq(vault.currentRelease(), _release(2));
        uint64 generation = vault.releaseGeneration();

        _approve(bob, 3);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.ReleaseApprovalChanged(bob, _release(2), false);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.CandidateReleaseChanged(bob, _release(3), _digest(3), CONFIG);
        vm.prank(bob);
        registry.setRelease(_digest(3), CONFIG);

        assertEq(registry.candidateOf(bob).release, _release(3));
        assertFalse(_approved(bob, 2), "the release left keeps no approval");
        OperatorRegistry.Operator memory operator = registry.operatorInEffect();
        assertEq(operator.candidate, bob);
        assertEq(operator.release, _release(3));
        assertEq(operator.since, effectiveAt, "an upgrade does not restart the term");
        assertEq(registry.operatorRelease(), _release(3));
        assertEq(vault.currentRelease(), _release(3), "the vault names the upgrade at once");
        // The fold raises the generation once, so no copy sealed under the old release reopens.
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), generation + 1);
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), generation + 1);
        assertTrue(registry.isEligible(bob), "the upgraded operator stays listed and eligible");
    }

    function test_AMoveIsOneWayUntilTheTimelockApprovesTheReleaseLeftAgain() public {
        uint64 effectiveAt = _elect(bob, 2);
        vm.warp(effectiveAt);
        _approve(bob, 3);
        vm.prank(bob);
        registry.setRelease(_digest(3), CONFIG);
        vault.syncRelease();
        uint64 generation = vault.releaseGeneration();

        // Going back to the release it left takes a fresh approval, and then a new generation.
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ReleaseNotApproved.selector, _release(2)));
        vm.prank(bob);
        registry.setRelease(_digest(2), CONFIG);
        assertEq(vault.currentRelease(), _release(3));
        _approve(bob, 2);
        vm.prank(bob);
        registry.setRelease(_digest(2), CONFIG);
        assertEq(vault.currentRelease(), _release(2));
        assertFalse(_approved(bob, 3));
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), generation + 1);
    }

    function test_TheInitialOperatorMovesFromItsDeploymentRelease() public {
        // The initial operator registers with another release than the deployment named; its move
        // replaces the deployment's release in the vault's eyes, and turning rotation on names it.
        _candidate(deployerKey, MIN_BOND, 1);
        _setRotation(true);
        assertEq(vault.currentRelease(), _release(0), "the deployment's release, not the candidacy's");
        vm.recordLogs();
        vm.prank(deployerKey);
        registry.setRelease(_digest(1), CONFIG);
        assertEq(registry.operatorInEffect().release, _release(1));
        assertEq(vault.currentRelease(), _release(1));
        // Moving to the release the candidacy already holds leaves its approval and its listing.
        assertTrue(_approved(deployerKey, 1));
        assertEq(registry.candidateCount(), 1);
        assertEq(registry.candidateList(0), deployerKey);
        // The deployment's release was never approved, so the move withdrew nothing to report.
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            assertTrue(logs[i].topics[0] != OperatorRegistry.ReleaseApprovalChanged.selector, "no approval changed");
        }
    }

    function test_AMoveWhileRotationIsOffIsTheReleaseRotationNamesWhenItTurnsOn() public {
        // The timelock upgrades the operator with rotation off, and the operator's candidacy follows,
        // so turning rotation on again names the upgrade rather than the old registered release.
        _candidate(deployerKey, MIN_BOND, 1);
        _govern(address(vault), abi.encodeCall(Vault.setCurrentRelease, (_digest(4), CONFIG)));
        _approve(deployerKey, 4);
        vm.prank(deployerKey);
        registry.setRelease(_digest(4), CONFIG);
        assertEq(vault.currentRelease(), _release(4), "while rotation is off the timelock's release stands");
        uint64 generation = vault.releaseGeneration();
        _setRotation(true);
        assertEq(vault.currentRelease(), _release(4));
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), generation, "turning rotation on moved no release");
    }

    function test_OnlyAReleaseTheTimelockApprovedForTheCandidateMoves() public {
        uint64 effectiveAt = _elect(bob, 2);
        vm.warp(effectiveAt);
        // Approved for another candidate, not for bob.
        _approve(alice, 3);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ReleaseNotApproved.selector, _release(3)));
        vm.prank(bob);
        registry.setRelease(_digest(3), CONFIG);
        // Approved, then withdrawn.
        _approve(bob, 3);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (bob, _digest(3), CONFIG, false))
        );
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ReleaseNotApproved.selector, _release(3)));
        vm.prank(bob);
        registry.setRelease(_digest(3), CONFIG);
        vm.expectRevert(OperatorRegistry.InvalidLaunchDigest.selector);
        vm.prank(bob);
        registry.setRelease(hex"01", CONFIG);
        assertEq(registry.candidateOf(bob).release, _release(2));
        assertEq(vault.currentRelease(), _release(2));
    }

    function test_OnlyARegisteredCandidateMoves() public {
        // An approval alone does not make a candidacy, and the deployer's unregistered operator
        // cannot move either.
        _approve(alice, 3);
        vm.expectRevert(OperatorRegistry.NotRegistered.selector);
        vm.prank(alice);
        registry.setRelease(_digest(3), CONFIG);
        _approve(deployerKey, 3);
        vm.expectRevert(OperatorRegistry.NotRegistered.selector);
        vm.prank(deployerKey);
        registry.setRelease(_digest(3), CONFIG);
        assertEq(registry.operatorInEffect().release, _release(0));
    }

    function test_AStandbysMoveLeavesTheVaultsReleaseAndIsWhatItsElectionNames() public {
        _setRotation(true);
        _candidate(alice, 30_000e18, 1);
        _delegate(alice, alice);
        _approve(alice, 5);
        vm.prank(alice);
        registry.setRelease(_digest(5), CONFIG);
        assertEq(registry.candidateOf(alice).release, _release(5));
        assertEq(registry.operatorInEffect().release, _release(0), "a standby's move leaves the operator's");
        assertEq(vault.currentRelease(), _release(0));
        vm.warp(block.timestamp + MIN_TERM);
        _closeNext();
        OperatorRegistry.Election memory pending = registry.pendingElection();
        assertEq(pending.candidate, alice);
        assertEq(pending.release, _release(5));
        vm.warp(pending.effectiveAt);
        assertEq(vault.currentRelease(), _release(5));
    }

    function test_NeitherSideOfAWaitingElectionMoves() public {
        _candidate(deployerKey, MIN_BOND, 1);
        _delegate(deployerKey, deployerKey);
        _candidate(carol, MIN_BOND, 6);
        _approve(deployerKey, 3);
        _approve(bob, 4);
        _approve(carol, 7);
        uint64 effectiveAt = _elect(bob, 2);
        vm.expectRevert(OperatorRegistry.ElectionPending.selector);
        vm.prank(deployerKey);
        registry.setRelease(_digest(3), CONFIG);
        vm.expectRevert(OperatorRegistry.ElectionPending.selector);
        vm.prank(bob);
        registry.setRelease(_digest(4), CONFIG);
        // A candidate the election does not involve still moves.
        vm.prank(carol);
        registry.setRelease(_digest(7), CONFIG);
        assertEq(registry.candidateOf(carol).release, _release(7));
        // The election and its handover source keep what they were made with.
        assertEq(registry.pendingElection().release, _release(2));
        assertEq(registry.handoverSource().release, _release(0));

        // Once the election has taken effect, the new operator moves, settling it first, and the
        // replaced operator's move no longer reaches the vault.
        vm.warp(effectiveAt);
        vm.prank(bob);
        registry.setRelease(_digest(4), CONFIG);
        assertEq(vault.currentRelease(), _release(4));
        vm.prank(deployerKey);
        registry.setRelease(_digest(3), CONFIG);
        assertEq(registry.candidateOf(deployerKey).release, _release(3));
        assertEq(vault.currentRelease(), _release(4));
    }

    function test_NeitherSideOfAPendingPromotionMoves() public {
        _candidate(deployerKey, MIN_BOND, 1);
        _delegate(deployerKey, deployerKey);
        _candidate(alice, 30_000e18, 5);
        _delegate(alice, alice);
        _setRotation(true);
        _approve(alice, 6);
        _approve(deployerKey, 3);
        vm.warp(block.timestamp + LIVENESS);
        registry.promote();
        assertEq(vault.currentRelease(), _release(5), "a promotion names its release at once");
        vm.expectRevert(OperatorRegistry.ElectionPending.selector);
        vm.prank(alice);
        registry.setRelease(_digest(6), CONFIG);
        vm.expectRevert(OperatorRegistry.ElectionPending.selector);
        vm.prank(deployerKey);
        registry.setRelease(_digest(3), CONFIG);
        assertEq(vault.currentRelease(), _release(5));
    }

    function test_AMoveToAnApprovedReleaseListsAnUnlistedCandidateUnderTheCap() public {
        _candidate(alice, MIN_BOND, 1);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, false))
        );
        assertEq(registry.candidateCount(), 0);
        _approve(alice, 2);
        vm.prank(alice);
        registry.setRelease(_digest(2), CONFIG);
        assertEq(registry.candidateCount(), 1);
        assertEq(registry.candidateList(0), alice);
        assertFalse(_approved(alice, 1), "the release left keeps no approval");

        // A move that would list a candidate over a full cap is refused, leaving it where it was.
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(2), CONFIG, false))
        );
        for (uint256 i = 0; i < MAX_CANDIDATES; ++i) {
            _candidate(makeAddr(string(abi.encode("filler", i))), MIN_BOND, 100 + i);
        }
        _approve(alice, 3);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, MAX_CANDIDATES));
        vm.prank(alice);
        registry.setRelease(_digest(3), CONFIG);
        assertEq(registry.candidateOf(alice).release, _release(2));
    }

    function test_TheOperatorInEffectTakesOnePlaceBeyondAFullCapToUpgrade() public {
        // The timelock withdrew the operator's own release, delisting it, and the list then filled.
        _candidate(deployerKey, MIN_BOND, 1);
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.setReleaseApproval, (deployerKey, _digest(1), CONFIG, false))
        );
        for (uint256 i = 0; i < MAX_CANDIDATES; ++i) {
            _candidate(makeAddr(string(abi.encode("filler", i))), MIN_BOND, 100 + i);
        }
        _setRotation(true);
        _approve(deployerKey, 2);
        vm.prank(deployerKey);
        registry.setRelease(_digest(2), CONFIG);
        assertEq(vault.currentRelease(), _release(2), "the full list did not block the upgrade");
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
        // The extra place is the operator's alone.
        address late = makeAddr("late");
        _stake(late, MIN_BOND);
        bytes memory authorization = _authorization(late);
        vm.prank(late);
        registry.register(_registration(_key(late), MIN_BOND, 200), authorization);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, MAX_CANDIDATES));
        vm.prank(address(governance));
        registry.setReleaseApproval(late, _digest(200), CONFIG, true);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
    }

    function test_TheCapExemptionFollowsTheOperatorInEffectOnceAnElectionMatures() public {
        // An election of bob has matured but nobody settled it: `operatorInEffect()` is already
        // bob's while the registry's `current` still names the deployer. The extra place beyond a
        // full list is the operator in effect's, so it is bob's, not the deployer's.
        _candidate(deployerKey, MIN_BOND, 1);
        _delegate(deployerKey, deployerKey);
        uint64 effectiveAt = _elect(bob, 2);
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.setReleaseApproval, (deployerKey, _digest(1), CONFIG, false))
        );
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (bob, _digest(2), CONFIG, false))
        );
        for (uint256 i = 0; i < MAX_CANDIDATES; ++i) {
            _candidate(makeAddr(string(abi.encode("filler", i))), MIN_BOND, 100 + i);
        }
        assertEq(registry.candidateCount(), MAX_CANDIDATES);
        vm.warp(effectiveAt);
        // Past its time the election reads as the operator in effect without anyone settling it;
        // `pendingElection()` masks it, so the listing below is what proves who holds the extra
        // place.
        assertEq(registry.operatorInEffect().candidate, bob);

        // The winner's approval lists it under the extra place; the replaced operator's does not.
        _approve(bob, 2);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (bob, _digest(2), CONFIG, false))
        );
        assertEq(registry.candidateCount(), MAX_CANDIDATES);
        // Called straight from the timelock, not through `_govern`'s schedule and execute, so the
        // expected revert is the next call.
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, MAX_CANDIDATES));
        vm.prank(address(governance));
        registry.setReleaseApproval(deployerKey, _digest(1), CONFIG, true);
        assertEq(registry.candidateCount(), MAX_CANDIDATES);
        _approve(bob, 2);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1, "the full list blocked the winner's listing");
    }

    function test_TheNextElectionSnapshotsTheUpgradedRelease() public {
        uint64 effectiveAt = _elect(bob, 2);
        vm.warp(effectiveAt);
        _approve(bob, 3);
        vm.prank(bob);
        registry.setRelease(_digest(3), CONFIG);
        _candidate(alice, 60_000e18, 1);
        _delegate(alice, alice);
        vm.warp(uint256(effectiveAt) + MIN_TERM);
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
        OperatorRegistry.HandoverSource memory source = registry.handoverSource();
        assertEq(source.candidate, bob);
        assertEq(source.release, _release(3), "the successor hands over from the release bob runs");
    }

    function test_AnInstalledOperatorMovesOnceRegisteredAndUpgradesLikeAnElectedOne() public {
        // Installed with no candidacy, carol has nothing to move: with rotation off the timelock
        // names its release.
        _install(carol, 7);
        uint64 since = registry.operatorInEffect().since;
        _approve(carol, 8);
        vm.expectRevert(OperatorRegistry.NotRegistered.selector);
        vm.prank(carol);
        registry.setRelease(_digest(8), CONFIG);

        // Registered on the installed release, it upgrades with rotation on as an elected operator.
        _candidate(carol, MIN_BOND, 7);
        _setRotation(true);
        vault.syncRelease();
        assertEq(vault.currentRelease(), _release(7));
        uint64 generation = vault.releaseGeneration();
        vm.expectEmit(address(registry));
        emit OperatorRegistry.ReleaseApprovalChanged(carol, _release(7), false);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.CandidateReleaseChanged(carol, _release(8), _digest(8), CONFIG);
        vm.prank(carol);
        registry.setRelease(_digest(8), CONFIG);
        OperatorRegistry.Operator memory operator = registry.operatorInEffect();
        assertEq(operator.candidate, carol);
        assertEq(operator.adminKey, _key(carol));
        assertEq(operator.release, _release(8));
        assertEq(operator.since, since, "an upgrade does not restart the installed term");
        assertEq(vault.currentRelease(), _release(8), "the vault names the upgrade at once");
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), generation + 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ReleaseNotApproved.selector, _release(7)));
        vm.prank(carol);
        registry.setRelease(_digest(7), CONFIG);
    }

    function test_AMoveLeavesTheReleaseAnInstallationNamedApartFromTheCandidacy() public {
        // carol's candidacy holds release 6; the timelock also approved 7 for it and installs it on
        // 7. A move back to the candidacy's release needs no new approval, since the timelock approved
        // it for carol, and leaves 7, which then keeps no approval.
        _candidate(carol, MIN_BOND, 6);
        _approve(carol, 7);
        _install(carol, 7);
        _setRotation(true);
        assertEq(vault.currentRelease(), _release(7), "the vault names the installed release");
        vm.expectEmit(address(registry));
        emit OperatorRegistry.ReleaseApprovalChanged(carol, _release(7), false);
        vm.prank(carol);
        registry.setRelease(_digest(6), CONFIG);
        assertEq(vault.currentRelease(), _release(6));
        assertTrue(_approved(carol, 6), "the candidacy's own release keeps its approval");
        assertFalse(_approved(carol, 7), "the installed release left keeps no approval");
        assertEq(registry.candidateCount(), 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ReleaseNotApproved.selector, _release(7)));
        vm.prank(carol);
        registry.setRelease(_digest(7), CONFIG);
        assertEq(vault.currentRelease(), _release(6));
    }

    function test_InstallingAnotherOperatorLeavesTheReplacedCandidacyAsItWas() public {
        // The registered deployer moved to release 2 as the operator in effect, and holds an
        // approval of 3; the timelock then installs carol on release 8.
        _candidate(deployerKey, MIN_BOND, 1);
        _approve(deployerKey, 2);
        vm.prank(deployerKey);
        registry.setRelease(_digest(2), CONFIG);
        _approve(deployerKey, 3);
        _install(carol, 8);
        assertEq(registry.candidateOf(deployerKey).release, _release(2));
        assertTrue(_approved(deployerKey, 2));
        assertTrue(_approved(deployerKey, 3));
        assertEq(registry.candidateCount(), 1);
        assertEq(registry.candidateList(0), deployerKey);
        // Its moves now change only its candidacy; rotation names the installed release.
        vm.prank(deployerKey);
        registry.setRelease(_digest(3), CONFIG);
        assertEq(registry.candidateOf(deployerKey).release, _release(3));
        assertEq(registry.operatorInEffect().release, _release(8));
        _setRotation(true);
        assertEq(vault.currentRelease(), _release(8));
    }

    function test_ThePlaceBeyondTheCapFollowsTheOperatorAnInstallationSeats() public {
        // The deployer's candidacy is delisted and the list fills; carol is registered, unlisted.
        _candidate(deployerKey, MIN_BOND, 1);
        _withdraw(deployerKey, 1);
        _fill();
        _register(carol, 5);
        _install(carol, 5);
        // The replaced operator no longer takes the place beyond the full cap.
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, MAX_CANDIDATES));
        vm.prank(address(governance));
        registry.setReleaseApproval(deployerKey, _digest(1), CONFIG, true);
        // The installed operator does, so the full list does not block its upgrade.
        _setRotation(true);
        _approve(carol, 6);
        vm.prank(carol);
        registry.setRelease(_digest(6), CONFIG);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
        assertEq(vault.currentRelease(), _release(6));
    }

    function test_AReplacedOperatorInThePlaceBeyondTheCapHoldsTheListAtTheCapPlusOne() public {
        // The deployer took the place beyond a full cap while it was the operator in effect.
        _candidate(deployerKey, MIN_BOND, 1);
        _withdraw(deployerKey, 1);
        _fill();
        _approve(deployerKey, 2);
        vm.prank(deployerKey);
        registry.setRelease(_digest(2), CONFIG);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
        // Once the timelock installs carol, carol cannot list past that place.
        _register(carol, 5);
        _install(carol, 5);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.CandidateCapReached.selector, MAX_CANDIDATES));
        vm.prank(address(governance));
        registry.setReleaseApproval(carol, _digest(5), CONFIG, true);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
        // The timelock frees it by withdrawing the replaced operator's approval; carol then lists.
        _withdraw(deployerKey, 2);
        _approve(carol, 5);
        assertEq(registry.candidateCount(), MAX_CANDIDATES + 1);
        assertEq(registry.candidateList(MAX_CANDIDATES), carol);
    }

    /// @dev A move succeeds exactly when the release is approved for the caller, and reaches the
    /// vault exactly when the caller is the operator in effect with rotation on.
    function testFuzz_AMoveLandsExactlyWhenApprovedAndReachesTheVaultOnlyFromTheOperator(
        uint256 seed,
        bool approved,
        bool asOperator,
        bool rotation
    ) public {
        seed = bound(seed, 3, 1_000);
        _candidate(deployerKey, MIN_BOND, 1);
        _candidate(alice, MIN_BOND, 2);
        if (rotation) _setRotation(true);
        address mover = asOperator ? deployerKey : alice;
        bytes32 before = registry.candidateOf(mover).release;
        bytes32 named = vault.currentRelease();
        if (approved) _approve(mover, seed);
        vm.prank(mover);
        if (!approved) {
            vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.ReleaseNotApproved.selector, _release(seed)));
        }
        registry.setRelease(_digest(seed), CONFIG);
        assertEq(registry.candidateOf(mover).release, approved ? _release(seed) : before);
        assertEq(vault.currentRelease(), approved && asOperator && rotation ? _release(seed) : named);
        assertEq(registry.operatorInEffect().release, approved && asOperator ? _release(seed) : _release(0));
        assertEq(registry.releaseApproved(keccak256(abi.encode(mover, before))), !approved, "the move is one-way");
    }

    function _approved(address candidate, uint256 seed) internal view returns (bool) {
        return registry.releaseApproved(keccak256(abi.encode(candidate, _release(seed))));
    }

    function _approve(address candidate, uint256 seed) internal {
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.setReleaseApproval, (candidate, _digest(seed), CONFIG, true))
        );
    }

    function _withdraw(address candidate, uint256 seed) internal {
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.setReleaseApproval, (candidate, _digest(seed), CONFIG, false))
        );
    }

    /// @dev Registers `candidate` with release `seed` and a minimum bond, unapproved and so unlisted.
    function _register(address candidate, uint256 seed) internal {
        _stake(candidate, MIN_BOND);
        bytes memory authorization = _authorization(candidate);
        vm.prank(candidate);
        registry.register(_registration(_key(candidate), MIN_BOND, seed), authorization);
    }

    /// @dev Fills the candidate list to the cap with approved candidates.
    function _fill() internal {
        for (uint256 i = 0; i < MAX_CANDIDATES; ++i) {
            _candidate(makeAddr(string(abi.encode("filler", i))), MIN_BOND, 100 + i);
        }
    }

    /// @dev The timelock installs `candidate`, with its test admin key, on release `seed`.
    function _install(address candidate, uint256 seed) internal {
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (candidate, _key(candidate), _digest(seed), CONFIG))
        );
    }

    /// @dev Registers `candidate` with release `seed`, stakes and self-delegates 20k for it, turns
    /// rotation on, and elects it past the deployer's term; returns the effective time.
    function _elect(address candidate, uint256 seed) internal returns (uint64) {
        _candidate(candidate, 20_000e18, seed);
        _delegate(candidate, candidate);
        if (!registry.rotationEnabled()) _setRotation(true);
        vm.warp(block.timestamp + MIN_TERM);
        _closeNext();
        OperatorRegistry.Election memory pending = registry.pendingElection();
        assertEq(pending.candidate, candidate);
        return pending.effectiveAt;
    }
}
