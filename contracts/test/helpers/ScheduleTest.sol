// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../../src/LogHeads.sol";
import {OperatorRegistry} from "../../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../../src/OperatorSchedule.sol";
import {SeedReceiver} from "../../src/SeedReceiver.sol";
import {Vault} from "../../src/Vault.sol";
import {RegistryTest} from "./RegistryTest.sol";

/// @notice A registry and vault naming each other, with a schedule over a seed receiver the test
/// relays to and the vault's log heads, deployed in November 2026 so every turn's seed epoch follows
/// mainnet's beacon genesis. Every candidate the tests register also registers a standby key,
/// vouched by the vault's signer, so closes adopt it.
abstract contract ScheduleTest is RegistryTest {
    /// Mainnet's beacon genesis time.
    uint64 internal constant GENESIS = 1606824023;
    /// 2026-11-05.
    uint256 internal constant START = 1_794_000_000;
    /// @dev Test values only: the switch budget and block time #593 and the runbook pin.
    uint64 internal constant SWITCH_BUDGET = 120;
    uint64 internal constant BLOCK_MILLIS = 250;

    address internal relay = makeAddr("relay");
    SeedReceiver internal receiver;
    OperatorSchedule internal schedule;
    LogHeads internal logHeads;

    /// @dev The private key of each address the tests sign with: the vault's first signer and every
    /// standby key.
    mapping(address signer => uint256 key) internal privateKeyOf;
    uint256 internal standbyKeys;

    function _deploySchedule() internal {
        vm.warp(START);
        _deployRegistry();
        receiver = new SeedReceiver(relay, GENESIS);
        schedule = _newSchedule();
        logHeads = schedule.logHeads();
        privateKeyOf[vault.checkpointSigner()] = uint256(keccak256(abi.encodePacked("enclave")));
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setVault, (address(vault))));
        _govern(address(vault), abi.encodeCall(Vault.setRegistry, (address(registry))));
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setSchedule, (schedule)));
    }

    /// @dev A schedule for the current registry and receiver, with log heads for the current vault and
    /// the test heartbeat rule.
    function _newSchedule() internal returns (OperatorSchedule) {
        return new OperatorSchedule(
            registry,
            receiver,
            new LogHeads(vault),
            _liveness(),
            _livenessMinimums(),
            _livenessMaximums(),
            SWITCH_BUDGET,
            BLOCK_MILLIS
        );
    }

    /// @dev 60 s slots, 30 s tolerance, five misses and 200 blocks: test values only.
    function _liveness() internal pure returns (OperatorSchedule.Liveness memory) {
        return OperatorSchedule.Liveness(60, 30, 5, 200);
    }

    function _livenessMinimums() internal pure returns (OperatorSchedule.Liveness memory) {
        return OperatorSchedule.Liveness(30, 5, 2, 10);
    }

    function _livenessMaximums() internal pure returns (OperatorSchedule.Liveness memory) {
        return OperatorSchedule.Liveness(120, 60, 9, 256);
    }

    /// @dev Registers a candidate as the registry helper does, then a fresh standby key for it.
    function _candidate(address candidate, uint256 stake, uint256 seed) internal virtual override {
        super._candidate(candidate, stake, seed);
        _registerStandby(candidate);
    }

    /// @dev Registers a fresh standby key for `candidate` on its current release, vouched by the
    /// vault's current signer, and returns it.
    function _registerStandby(address candidate) internal returns (address key) {
        key = _freshStandbyKey();
        _registerStandby(candidate, key, privateKeyOf[vault.checkpointSigner()]);
    }

    /// @dev Registers `key` for `candidate` on its current release, vouched by `voucherKey`.
    function _registerStandby(address candidate, address key, uint256 voucherKey) internal {
        uint64 expiry = uint64(block.timestamp + 1 days);
        bytes memory vouch = _vouch(voucherKey, candidate, key, registry.candidateOf(candidate).release, 1, expiry);
        vm.prank(candidate);
        schedule.registerStandby(key, 1, expiry, vouch);
    }

    /// @dev A standby key no test has used, whose private key the tests hold.
    function _freshStandbyKey() internal returns (address key) {
        uint256 secret = uint256(keccak256(abi.encode("standby key", standbyKeys++)));
        key = vm.addr(secret);
        privateKeyOf[key] = secret;
    }

    /// @dev `voucherKey`'s vouch for `candidate`'s standby `key` on `release`, over the digest built by
    /// hand as the vouching enclave builds it.
    function _vouch(uint256 voucherKey, address candidate, address key, bytes32 release, uint64 nonce, uint64 expiry)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = keccak256(
            abi.encode(
                keccak256("engine.standby-vouch.v1"),
                block.chainid,
                address(vault),
                address(registry),
                address(schedule),
                candidate,
                key,
                release,
                nonce,
                expiry
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(voucherKey, digest);
        return abi.encodePacked(r, s, v);
    }

    /// @dev The turn drawable now: the one whose boundary comes next.
    function _nextTurn() internal view returns (uint64) {
        return uint64((block.timestamp - schedule.BOUNDARY_OFFSET()) / 1 days + 1);
    }

    function _relay(uint64 turn, bytes32 seed) internal {
        uint64 epoch = receiver.seedEpoch(turn);
        vm.prank(receiver.relayAlias());
        receiver.receiveSeed(turn, epoch, seed);
    }

    /// @dev Relays for `turn` a seed whose first position in set `setId` picks `member`.
    function _relayPicking(uint64 turn, uint32 setId, address member) internal returns (bytes32 seed) {
        for (uint256 i = 0;; ++i) {
            seed = keccak256(abi.encode("seed", turn, i));
            if (schedule.memberAt(setId, seed, turn, 0) == member) break;
        }
        _relay(turn, seed);
    }

    /// @dev Registers and self-delegates each of `members` with its stake and release `i + 1`, turns
    /// rotation on, closes, and warps to when the adopted set takes effect.
    function _rotate(address[] memory members, uint256[] memory stakes) internal returns (uint32 setId) {
        for (uint256 i = 0; i < members.length; ++i) {
            _candidate(members[i], stakes[i], i + 1);
            _delegate(members[i], members[i]);
        }
        _setRotation(true);
        _closeNext();
        setId = schedule.pendingSetId();
        (, uint64 effectiveAt,,) = schedule.setOf(setId);
        vm.warp(effectiveAt);
    }

    function _rotateOne(address member) internal returns (uint32) {
        (address[] memory members, uint256[] memory stakes) = _one(member);
        return _rotate(members, stakes);
    }

    function _rotateTwo(address a, address b) internal returns (uint32) {
        (address[] memory members, uint256[] memory stakes) = _two(a, b);
        return _rotate(members, stakes);
    }

    function _one(address member) internal pure returns (address[] memory members, uint256[] memory stakes) {
        members = new address[](1);
        stakes = new uint256[](1);
        (members[0], stakes[0]) = (member, 30_000e18);
    }

    function _two(address a, address b) internal pure returns (address[] memory members, uint256[] memory stakes) {
        members = new address[](2);
        stakes = new uint256[](2);
        (members[0], stakes[0], members[1], stakes[1]) = (a, 30_000e18, b, 20_000e18);
    }

    /// @dev Warps to `turn`'s draw deadline, unless past it, and draws it.
    function _draw(uint64 turn) internal returns (address) {
        uint64 opensAt = schedule.drawDeadline(turn);
        if (block.timestamp < opensAt) vm.warp(opensAt);
        return schedule.draw(turn);
    }
}
