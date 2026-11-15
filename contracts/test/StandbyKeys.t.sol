// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {Vault} from "../src/Vault.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// Standby keys: registration vouched by the vault's signer or another member's promotable key,
/// what voids a key, the keys that never register, the registry refusing them as enclaves, and a close
/// leaving out a candidate without one.
contract StandbyKeysTest is LivenessTest {
    address internal voter = makeAddr("voter");

    function setUp() public {
        _deployLiveness();
    }

    function _signer() internal view returns (uint256) {
        return privateKeyOf[vault.checkpointSigner()];
    }

    function _register(address candidate, address key, uint64 nonce, uint64 expiry, bytes memory vouch) internal {
        vm.prank(candidate);
        schedule.registerStandby(key, nonce, expiry, vouch);
    }

    /// @dev `voucher`'s vouch for `candidate`'s `key` on its current release, expiring in a day.
    function _vouchFor(uint256 voucher, address candidate, address key) internal view returns (bytes memory) {
        return
            _vouch(
                voucher, candidate, key, registry.candidateOf(candidate).release, 7, uint64(block.timestamp + 1 days)
            );
    }

    function _expiry() internal view returns (uint64) {
        return uint64(block.timestamp + 1 days);
    }

    function _moveRelease(address candidate, uint256 seed_) internal {
        vm.prank(address(governance));
        registry.setReleaseApproval(candidate, _digest(seed_), CONFIG, true);
        vm.prank(candidate);
        registry.setRelease(_digest(seed_), CONFIG);
    }

    function test_ACandidateRegistersAStandbyKeyTheSignerVouchesFor() public {
        _candidate(alice, 30_000e18, 1);
        (address first,,) = schedule.standbyOf(alice);
        address key = _freshStandbyKey();
        bytes memory vouch = _vouchFor(_signer(), alice, key);
        address signer = vault.checkpointSigner();
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.StandbyRegistered(alice, key, _release(1), signer);
        _register(alice, key, 7, _expiry(), vouch);
        (address recorded, bytes32 release, uint64 at) = schedule.standbyOf(alice);
        assertEq(recorded, key);
        assertEq(release, _release(1));
        assertEq(at, block.timestamp);
        assertEq(schedule.standbyHolder(key), alice);
        assertEq(schedule.promotable(alice), key);
        // The replaced key is void, and stays registered to alice so it never registers again.
        assertEq(schedule.standbyHolder(first), alice);
        bytes memory again = _vouchFor(_signer(), alice, first);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidStandbyKey.selector, first));
        _register(alice, first, 7, _expiry(), again);
    }

    function test_TheVouchDigestBindsTheChainTheContractsAndTheRecord() public {
        _candidate(alice, 30_000e18, 1);
        address key = _freshStandbyKey();
        bytes32 expected = keccak256(
            abi.encode(
                keccak256("engine.standby-vouch.v1"),
                block.chainid,
                address(vault),
                address(registry),
                address(schedule),
                alice,
                key,
                _release(1),
                uint64(7),
                uint64(99)
            )
        );
        assertEq(schedule.vouchDigest(alice, key, _release(1), 7, 99), expected);
    }

    function test_ASubstitutedKeyIsRefused() public {
        _candidate(alice, 30_000e18, 1);
        address vouched = _freshStandbyKey();
        address substitute = _freshStandbyKey();
        bytes memory vouch = _vouchFor(_signer(), alice, vouched);
        vm.expectPartialRevert(OperatorSchedule.InvalidVouch.selector);
        _register(alice, substitute, 7, _expiry(), vouch);
        assertEq(schedule.standbyHolder(substitute), address(0));
    }

    function test_AReplayedVouchIsRefused() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        address key = _freshStandbyKey();
        bytes memory vouch = _vouchFor(_signer(), alice, key);
        _register(alice, key, 7, _expiry(), vouch);
        // The same registration again, and by another candidate, finds the key taken.
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidStandbyKey.selector, key));
        _register(alice, key, 7, _expiry(), vouch);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidStandbyKey.selector, key));
        _register(bob, key, 7, _expiry(), vouch);
        // Alice's vouch does not register a fresh key for bob, nor one under another nonce or expiry.
        address fresh = _freshStandbyKey();
        bytes memory forAlice = _vouchFor(_signer(), alice, fresh);
        vm.expectPartialRevert(OperatorSchedule.InvalidVouch.selector);
        _register(bob, fresh, 7, _expiry(), forAlice);
        vm.expectPartialRevert(OperatorSchedule.InvalidVouch.selector);
        _register(alice, fresh, 8, _expiry(), forAlice);
        vm.expectPartialRevert(OperatorSchedule.InvalidVouch.selector);
        _register(alice, fresh, 7, _expiry() + 1, forAlice);
    }

    function test_AnExpiredVouchIsRefused() public {
        _candidate(alice, 30_000e18, 1);
        address key = _freshStandbyKey();
        uint64 expiry = uint64(block.timestamp - 1);
        bytes memory expired = _vouch(_signer(), alice, key, _release(1), 7, expiry);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.VouchExpired.selector, expiry));
        _register(alice, key, 7, expiry, expired);
        bytes memory lastSecond = _vouch(_signer(), alice, key, _release(1), 7, uint64(block.timestamp));
        _register(alice, key, 7, uint64(block.timestamp), lastSecond);
        assertEq(schedule.promotable(alice), key);
    }

    function test_AMalformedVouchIsRefused() public {
        _candidate(alice, 30_000e18, 1);
        address key = _freshStandbyKey();
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidVouch.selector, address(0)));
        _register(alice, key, 7, _expiry(), new bytes(65));
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidVouch.selector, address(0)));
        _register(alice, key, 7, _expiry(), hex"1234");
    }

    /// The vault's signer drops alice's registration; bob's promotable standby key vouches for it.
    function test_ARegistrationTheIncumbentDropsCarriesAnotherMembersVouch() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        address bobs = schedule.promotable(bob);
        address key = _freshStandbyKey();
        bytes memory vouch = _vouchFor(privateKeyOf[bobs], alice, key);
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.StandbyRegistered(alice, key, _release(1), bobs);
        _register(alice, key, 7, _expiry(), vouch);
        assertEq(schedule.promotable(alice), key);
    }

    function test_AVouchFromAVoidedKeyIsRefused() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _candidate(carol, 25_000e18, 3);
        _candidate(dave, 10_000e18, 4);
        address replaced = schedule.promotable(alice);
        _registerStandby(alice);
        address moved = schedule.promotable(bob);
        _moveRelease(bob, 20);
        address withdrawn = schedule.promotable(carol);
        vm.prank(address(governance));
        registry.setReleaseApproval(carol, _digest(3), CONFIG, false);

        address[3] memory voided = [replaced, moved, withdrawn];
        for (uint256 i = 0; i < 3; ++i) {
            address key = _freshStandbyKey();
            bytes memory vouch = _vouchFor(privateKeyOf[voided[i]], dave, key);
            vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidVouch.selector, voided[i]));
            _register(dave, key, 7, _expiry(), vouch);
        }
        assertEq(schedule.promotable(bob), address(0));
        assertEq(schedule.promotable(carol), address(0));
    }

    /// A promotion consumes the record: the key vouches only while it is the vault's signer.
    function test_AConsumedKeyVouchesOnlyAsTheSigner() public {
        _startTurn();
        _lapse();
        schedule.promote();
        address promoted = schedule.turnOf(turn).candidate;
        address consumed = vault.checkpointSigner();
        assertEq(schedule.promotable(promoted), address(0));
        address key = _freshStandbyKey();
        _register(alice, key, 7, _expiry(), _vouchFor(privateKeyOf[consumed], alice, key));
        assertEq(schedule.promotable(alice), key);

        // The timelock installs the promoted member's registered enclave; the consumed key is no
        // longer the signer and vouches for nothing.
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (registry.operatorEnclave())));
        key = _freshStandbyKey();
        bytes memory vouch = _vouchFor(privateKeyOf[consumed], alice, key);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidVouch.selector, consumed));
        _register(alice, key, 7, _expiry(), vouch);
    }

    function test_AReleaseMoveOrAWithdrawnApprovalVoidsTheKey() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        address key = schedule.promotable(alice);
        _moveRelease(alice, 30);
        assertEq(schedule.promotable(alice), address(0));
        // The key's release approved again does not revive it for a candidate running another.
        vm.prank(address(governance));
        registry.setReleaseApproval(alice, _digest(1), CONFIG, true);
        assertEq(schedule.promotable(alice), address(0));
        (address recorded, bytes32 release,) = schedule.standbyOf(alice);
        assertEq(recorded, key, "the record stays, void");
        assertEq(release, _release(1));
        address fresh = _registerStandby(alice);
        assertEq(schedule.promotable(alice), fresh);

        address bobs = schedule.promotable(bob);
        vm.prank(address(governance));
        registry.setReleaseApproval(bob, _digest(2), CONFIG, false);
        assertEq(schedule.promotable(bob), address(0));
        vm.prank(address(governance));
        registry.setReleaseApproval(bob, _digest(2), CONFIG, true);
        assertEq(schedule.promotable(bob), bobs, "approved again, the same release's key serves again");
    }

    function test_AReusedKeyIsRefused() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        address alices = schedule.promotable(alice);
        address enclave = vault.checkpointSigner();
        address[3] memory refused = [alices, enclave, address(0)];
        for (uint256 i = 0; i < 3; ++i) {
            bytes memory vouch = _vouchFor(_signer(), bob, refused[i]);
            vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidStandbyKey.selector, refused[i]));
            _register(bob, refused[i], 7, _expiry(), vouch);
        }
        // A key that was the signer once never registers, after the signer moves on too.
        uint256 nextSecret = uint256(keccak256("next signer"));
        address next = vm.addr(nextSecret);
        privateKeyOf[next] = nextSecret;
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (next)));
        bytes memory late = _vouchFor(_signer(), bob, enclave);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.InvalidStandbyKey.selector, enclave));
        _register(bob, enclave, 7, _expiry(), late);
    }

    /// A candidate sets an enclave address first and registers it as its standby key after, which
    /// the registry cannot refuse; once the timelock installs it as the signer it is no longer
    /// promotable.
    function test_AKeyThatBecameTheSignerIsNotPromotable() public {
        _candidate(alice, 30_000e18, 1);
        address key = _freshStandbyKey();
        vm.prank(alice);
        registry.setEnclave(key);
        _register(alice, key, 7, _expiry(), _vouchFor(_signer(), alice, key));
        assertEq(schedule.promotable(alice), key);
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (key)));
        assertTrue(vault.wasSigner(key));
        assertEq(schedule.promotable(alice), address(0));
    }

    function test_RegisterAndSetEnclaveRefuseAStandbyKey() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        address key = schedule.promotable(alice);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EnclaveIsStandbyKey.selector, key));
        vm.prank(alice);
        registry.setEnclave(key);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EnclaveIsStandbyKey.selector, key));
        vm.prank(bob);
        registry.setEnclave(key);

        _stake(dave, 10_000e18);
        OperatorRegistry.Registration memory registration = _registration(_key(dave), MIN_BOND, 4);
        registration.enclave = key;
        bytes memory authorization = _authorization(dave);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.EnclaveIsStandbyKey.selector, key));
        vm.prank(dave);
        registry.register(registration, authorization);
    }

    function test_AnUnregisteredOrUnapprovedCandidateCannotRegisterAKey() public {
        address key = _freshStandbyKey();
        bytes memory vouch = _vouch(_signer(), dave, key, bytes32(0), 7, _expiry());
        vm.expectRevert(OperatorSchedule.NotRegistered.selector);
        _register(dave, key, 7, _expiry(), vouch);

        _stake(dave, 10_000e18);
        bytes memory authorization = _authorization(dave);
        vm.prank(dave);
        registry.register(_registration(_key(dave), MIN_BOND, 4), authorization);
        vouch = _vouchFor(_signer(), dave, key);
        vm.expectRevert(abi.encodeWithSelector(OperatorSchedule.ReleaseNotApproved.selector, _release(4)));
        _register(dave, key, 7, _expiry(), vouch);
    }

    /// Carol has no standby key at the close, so the set leaves her out; her votes still make the
    /// quorum, which alice's and bob's alone would miss.
    function test_ACandidateWithoutAPromotableKeyIsLeftOutAtAClose() public {
        _candidate(alice, 30_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        RegistryTest._candidate(carol, 25_000e18, 3);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _delegate(carol, carol);
        // 225k staked and delegated to no one: 75k of 300k meets the 20% quorum, 50k would not.
        _stake(voter, 225_000e18);
        _setRotation(true);
        (uint64 epochLength,,,,,,) = registry.parameters();
        assertGe(block.timestamp, uint256(registry.openEpochStart()) + epochLength, "the open epoch has ended");
        address[] memory members = new address[](2);
        address[] memory keys = new address[](2);
        uint256[] memory weights = new uint256[](2);
        (members[0], members[1], keys[0], keys[1]) = (alice, bob, _key(alice), _key(bob));
        (weights[0], weights[1]) = (30_000e18, 20_000e18);
        uint64 epoch = registry.openEpoch() + uint64((block.timestamp - registry.openEpochStart()) / epochLength) - 1;
        vm.expectEmit(address(schedule));
        emit OperatorSchedule.RotationSetAdopted(
            1, epoch, uint64(block.timestamp + 7 days + FINALITY_MARGIN), true, members, keys, weights
        );
        registry.closeEpoch();

        // With a key registered, the next close adopts her.
        _registerStandby(carol);
        _closeNext();
        (,, address[] memory adopted,) = schedule.setOf(2);
        assertEq(adopted.length, 3);
        assertEq(adopted[2], carol);
    }
}
