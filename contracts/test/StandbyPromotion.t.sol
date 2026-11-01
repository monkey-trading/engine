// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {Vault} from "../src/Vault.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// Liveness promotion of a standby, the vault's registry coupling, and the limited `setSigner`.
contract StandbyPromotionTest is RegistryTest {
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");

    function setUp() public {
        _deployRegistry();
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setVault, (address(vault))));
        _govern(address(vault), abi.encodeCall(Vault.setRegistry, (address(registry))));
    }

    /// @dev The enclave address `_registration` derives for `seed`.
    function _enclave(uint256 seed) internal pure returns (address) {
        return address(uint160(uint256(keccak256(abi.encode("enclave", seed)))));
    }

    /// @dev Two eligible candidates, alice leading bob, and rotation on: the liveness setup.
    function _standbys() internal {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
    }

    /// @dev Past the liveness timeout: no checkpoint since the vault deployed.
    function _lapsed() internal {
        vm.warp(block.timestamp + LIVENESS);
    }

    function test_PromotionRevertsWhileRotationIsOff() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _lapsed();
        vm.expectRevert(OperatorRegistry.RotationDisabled.selector);
        registry.promote();
        assertEq(_inEffect(), deployerKey);
    }

    function test_PromotionRevertsBeforeTheLivenessTimeout() public {
        _standbys();
        uint256 dueAt = uint256(vault.lastCheckpointAt()) + LIVENESS;
        vm.warp(dueAt - 1);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.LivenessNotDue.selector, dueAt));
        registry.promote();
        assertEq(_inEffect(), deployerKey);
    }

    function test_PromotionRevertsWhileAnElectionWaits() public {
        _standbys();
        vm.warp(block.timestamp + MIN_TERM);
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
        // The liveness timeout has lapsed during the setup warps; only the pending election stops
        // the promotion, and its time has not come.
        vm.expectRevert(OperatorRegistry.ElectionPending.selector);
        registry.promote();
    }

    function test_PromotionRevertsWithoutAQualifiedStandby() public {
        _setRotation(true);
        _lapsed();
        vm.expectRevert(OperatorRegistry.NoQualifiedStandby.selector);
        registry.promote();

        // A registered but unapproved candidate does not qualify either.
        _stake(alice, 30_000e18);
        bytes memory authorization = _authorization(alice);
        vm.prank(alice);
        registry.register(_registration(_key(alice), MIN_BOND, 1), authorization);
        _delegate(alice, alice);
        vm.expectRevert(OperatorRegistry.NoQualifiedStandby.selector);
        registry.promote();
        assertEq(_inEffect(), deployerKey);
    }

    function test_PromotionPromotesTheTopEligibleStandby() public {
        _standbys();
        _lapsed();
        uint256 promotedAt = block.timestamp;
        vm.expectEmit(address(registry));
        uint64 epoch = _nextEpoch();
        emit OperatorRegistry.Elected(
            epoch, alice, _key(alice), _release(1), uint64(promotedAt + 7 days + FINALITY_MARGIN)
        );
        assertEq(registry.promote(), epoch);
        OperatorRegistry.Election memory pending = registry.pendingElection();
        assertEq(pending.candidate, alice);
        assertEq(pending.adminKey, _key(alice));
        assertEq(pending.release, _release(1));
        assertEq(pending.effectiveAt, promotedAt + 7 days + FINALITY_MARGIN);
        // The release and enclave move at once; the admin key moves at the notice.
        assertEq(registry.operatorRelease(), _release(1));
        assertEq(registry.operatorEnclave(), _enclave(1));
        assertEq(_inEffect(), deployerKey);
        vm.warp(pending.effectiveAt);
        assertEq(_inEffect(), alice);
    }

    function test_PromotionBreaksVoteTiesByEarlierRegistration() public {
        _candidate(alice, 20_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        _lapsed();
        registry.promote();
        assertEq(registry.pendingElection().candidate, alice, "the earlier registration wins the tie");
    }

    function test_PromotionSkipsAnIneligibleLeader() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        // Alice's release approval withdrawn: only bob qualifies now.
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, false))
        );
        _lapsed();
        registry.promote();
        assertEq(registry.pendingElection().candidate, bob);
    }

    function test_PromotionSettlesAnElectionWhoseTimeHasCome() public {
        _standbys();
        vm.warp(block.timestamp + MIN_TERM);
        _closeNext();
        assertEq(registry.pendingElection().candidate, alice);
        // Alice's election takes effect; promotion then finds no other standby.
        vm.warp(registry.pendingElection().effectiveAt);
        _candidate(carol, 25_000e18, 3);
        _delegate(carol, carol);
        _lapsed();
        registry.promote();
        assertEq(registry.pendingElection().candidate, carol);
        assertEq(_inEffect(), alice);
    }

    function test_PromotionEpochsStayAheadOfCloses() public {
        _standbys();
        _lapsed();
        uint64 expected = _nextEpoch();
        uint64 promoted = registry.promote();
        assertEq(promoted, expected);
        vm.warp(block.timestamp + EPOCH);
        uint64 next = _closeNext();
        assertGt(next, promoted, "a close after the promotion takes a higher epoch");
    }

    function test_SetEnclaveUpdatesTheRegisteredAddress() public {
        _candidate(alice, 30_000e18, 1);
        assertEq(registry.candidateOf(alice).enclave, _enclave(1));
        vm.prank(alice);
        registry.setEnclave(carol);
        assertEq(registry.candidateOf(alice).enclave, carol);
        assertEq(registry.operatorEnclave(), address(0), "the deployer in effect registered no enclave");
    }

    function test_SetEnclaveRevertsForTheUnregisteredAndTheZeroAddress() public {
        vm.expectRevert(OperatorRegistry.NotRegistered.selector);
        registry.setEnclave(alice);
        _candidate(alice, 30_000e18, 1);
        vm.prank(alice);
        vm.expectRevert(OperatorRegistry.ZeroAddress.selector);
        registry.setEnclave(address(0));
    }

    function test_RegistrationRequiresAnEnclaveAddress() public {
        _stake(alice, 30_000e18);
        OperatorRegistry.Registration memory registration = _registration(_key(alice), MIN_BOND, 1);
        registration.enclave = address(0);
        bytes memory authorization = _authorization(alice);
        vm.prank(alice);
        vm.expectRevert(OperatorRegistry.ZeroAddress.selector);
        registry.register(registration, authorization);
    }

    function test_SetVaultIsGovernanceOnceAndNamesCode() public {
        vm.expectRevert(OperatorRegistry.UnauthorizedGovernance.selector);
        registry.setVault(address(vault));
        vm.prank(address(governance));
        vm.expectRevert(OperatorRegistry.ZeroAddress.selector);
        registry.setVault(address(0));
        vm.prank(address(governance));
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.VaultAlreadySet.selector, address(vault)));
        registry.setVault(address(vault));
    }

    function test_SetRegistryIsGovernanceOnce() public {
        vm.expectRevert(Vault.UnauthorizedGovernance.selector);
        vault.setRegistry(address(registry));
        vm.prank(address(governance));
        vm.expectRevert(abi.encodeWithSelector(Vault.RegistryAlreadySet.selector, address(registry)));
        vault.setRegistry(address(registry));
    }

    function test_CurrentReleaseFollowsTheRegistryAtOnce() public {
        _standbys();
        _lapsed();
        registry.promote();
        assertEq(vault.currentRelease(), _release(1), "the release moved without a timelock call");
    }

    function test_SetCurrentReleaseRevertsWhileRotationIsOn() public {
        _standbys();
        vm.expectRevert(Vault.RotationActive.selector);
        vm.prank(address(governance));
        vault.setCurrentRelease(_digest(9), CONFIG);
    }

    function test_SyncReleaseRaisesTheGenerationOnce() public {
        _standbys();
        _lapsed();
        registry.promote();
        uint64 generation = vault.releaseGeneration();
        vault.syncRelease();
        assertEq(vault.currentRelease(), _release(1));
        assertEq(vault.releaseGeneration(), generation + 1);
        vault.syncRelease();
        assertEq(vault.releaseGeneration(), generation + 1, "a second sync is a no-op");
    }

    function test_SetSignerInstallsOnlyTheRegisteredEnclaveWhileRotationIsOn() public {
        _standbys();
        _lapsed();
        registry.promote();
        vm.startPrank(address(governance));
        vm.expectRevert(abi.encodeWithSelector(Vault.NotTheRegisteredEnclave.selector, bob, _enclave(1)));
        vault.setSigner(bob);
        vault.setSigner(_enclave(1));
        assertEq(vault.checkpointSigner(), _enclave(1));
        assertEq(vault.signerAtEpoch(vault.signerEpoch()), _enclave(1));
        vm.stopPrank();
    }

    function test_SetSignerFollowsAnEnclaveUpdate() public {
        _standbys();
        _lapsed();
        registry.promote();
        vm.prank(alice);
        registry.setEnclave(carol);
        vm.startPrank(address(governance));
        vm.expectRevert(abi.encodeWithSelector(Vault.NotTheRegisteredEnclave.selector, _enclave(1), carol));
        vault.setSigner(_enclave(1));
        vault.setSigner(carol);
        assertEq(vault.checkpointSigner(), carol);
        vm.stopPrank();
    }

    function test_SetSignerStaysOpenWhileRotationIsOff() public {
        _candidate(alice, 30_000e18, 1);
        _delegate(alice, alice);
        vm.prank(address(governance));
        vault.setSigner(bob);
        assertEq(vault.checkpointSigner(), bob);
    }

    /// @dev The epoch number a promotion consumes: the next unclosed one.
    function _nextEpoch() internal view returns (uint64) {
        return registry.openEpoch() + uint64((block.timestamp - registry.openEpochStart()) / EPOCH);
    }
}
