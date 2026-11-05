// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "../../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../../src/OperatorSchedule.sol";
import {SeedReceiver} from "../../src/SeedReceiver.sol";
import {Vault} from "../../src/Vault.sol";
import {RegistryTest} from "./RegistryTest.sol";

/// @notice A registry and vault naming each other, with a schedule over a seed receiver the test
/// relays to, deployed in November 2026 so every turn's seed epoch follows mainnet's beacon genesis.
abstract contract ScheduleTest is RegistryTest {
    /// Mainnet's beacon genesis time.
    uint64 internal constant GENESIS = 1606824023;
    /// 2026-11-05.
    uint256 internal constant START = 1_794_000_000;

    address internal relay = makeAddr("relay");
    SeedReceiver internal receiver;
    OperatorSchedule internal schedule;

    function _deploySchedule() internal {
        vm.warp(START);
        _deployRegistry();
        receiver = new SeedReceiver(relay, GENESIS);
        schedule = new OperatorSchedule(registry, receiver);
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setVault, (address(vault))));
        _govern(address(vault), abi.encodeCall(Vault.setRegistry, (address(registry))));
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setSchedule, (schedule)));
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
