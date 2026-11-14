// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../../src/LogHeads.sol";
import {OperatorSchedule} from "../../src/OperatorSchedule.sol";
import {MockArbSys} from "../mocks/MockArbSys.sol";
import {ScheduleTest} from "./ScheduleTest.sol";

/// @notice A daily turn naming the operator, drawn from a set of four members with standby keys, with
/// an `ArbSys` stand-in at 0x64 whose block number the tests move apart from `block.number`, and
/// helpers that sign `engine.log-head.v2` heads, witness slots and lapse the operator.
abstract contract LivenessTest is ScheduleTest {
    MockArbSys internal constant ARB = MockArbSys(address(0x64));
    uint64 internal constant INTERVAL = 60;
    uint64 internal constant MISSES = 5;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");
    address internal dave = makeAddr("dave");
    address internal confirmer = makeAddr("confirmer");

    uint32 internal setId;
    /// @dev The turn the tests run in, and its seed.
    uint64 internal turn;
    bytes32 internal seed;

    function _deployLiveness() internal {
        vm.etch(address(ARB), address(new MockArbSys()).code);
        ARB.setBlockNumber(100_000_000);
        _deploySchedule();
    }

    /// @dev Adopts alice, bob, carol and dave, weighted 4:3:2:1, draws `turn` with a seed whose first
    /// positions pick `order`, and warps to its boundary, from which it names the operator. Turn
    /// `turn + 1`'s seed picks alice first.
    function _startTurn(address[] memory order) internal {
        address[] memory members = new address[](4);
        uint256[] memory stakes = new uint256[](4);
        (members[0], members[1], members[2], members[3]) = (alice, bob, carol, dave);
        (stakes[0], stakes[1], stakes[2], stakes[3]) = (40_000e18, 30_000e18, 20_000e18, 10_000e18);
        setId = _rotate(members, stakes);
        turn = _nextTurn() + 1;
        seed = _relayOrder(turn, order);
        _relayPicking(turn + 1, setId, alice);
        _draw(turn);
        vm.warp(schedule.boundary(turn));
    }

    /// @dev `_startTurn` with alice at position 0.
    function _startTurn() internal {
        address[] memory order = new address[](1);
        order[0] = alice;
        _startTurn(order);
    }

    /// @dev Relays for `turn` a seed whose first positions in the set pick `order`.
    function _relayOrder(uint64 at, address[] memory order) internal returns (bytes32 found) {
        for (uint256 i = 0;; ++i) {
            found = keccak256(abi.encode("ordered seed", at, i));
            bool matches = true;
            for (uint16 p = 0; p < order.length && matches; ++p) {
                matches = schedule.memberAt(setId, found, at, p) == order[p];
            }
            if (matches) break;
        }
        _relay(at, found);
    }

    /// @dev The member `promote` names now by the rule, walked independently over `memberAt`: the first
    /// position after the current one whose member has held no assignment of the turn, the registry
    /// lists under its adopted key, and holds a promotable standby key.
    function _expectedPromotion() internal view returns (uint16 position, address member) {
        OperatorSchedule.Turn memory current = schedule.turnOf(turn);
        (,, address[] memory candidates,) = schedule.setOf(setId);
        uint256 end = schedule.MAX_DRAW_POSITIONS() + candidates.length;
        for (position = current.position + 1; position < end; ++position) {
            member = schedule.memberAt(setId, seed, turn, position);
            if (
                !schedule.heldTurn(turn, member) && registry.isListed(member)
                    && registry.keyHolder(_key(member)) == member && schedule.promotable(member) != address(0)
            ) return (position, member);
        }
        return (0, address(0));
    }

    function _slot() internal view returns (uint64) {
        return uint64(block.timestamp) / INTERVAL;
    }

    /// @dev Warps `offset` seconds into the next slot and moves the Arbitrum block on by `blocks`.
    function _nextSlot(uint256 offset, uint256 blocks) internal {
        vm.warp((uint256(_slot()) + 1) * INTERVAL + offset);
        ARB.setBlockNumber(ARB.arbBlockNumber() + blocks);
    }

    /// @dev Witnesses the current slot and each of the next `count - 1`, ten seconds into each, so the
    /// clock ends in the last slot witnessed.
    function _witnessFromNow(uint256 count) internal {
        for (uint256 i = 0; i < count; ++i) {
            if (i > 0) _nextSlot(10, 240);
            schedule.witness();
        }
    }

    /// @dev Witnesses `MISSES` slots in a row from the next one and moves into the slot after them,
    /// where, with no head since, a promotion is due.
    function _lapse() internal {
        _nextSlot(10, 240);
        _witnessFromNow(MISSES);
        _nextSlot(5, 240);
    }

    /// @dev The private key of the vault's current signer.
    function _signerKey() internal view returns (uint256) {
        return privateKeyOf[vault.checkpointSigner()];
    }

    /// @dev A live head one seq past the epoch's held head, binding the turn in effect's current
    /// assignment and its member's release, stamped and signed now.
    function _nextHead() internal view returns (LogHeads.HeadV2 memory head) {
        uint64 epoch = vault.signerEpoch();
        (uint64 heldSeq, bytes32 heldLink, uint64 heldSignedAt,,) = logHeads.heads(epoch);
        (uint64 inEffect, OperatorSchedule.Turn memory current) = schedule.turnAt(uint64(block.timestamp));
        uint64 nowMs = uint64(block.timestamp) * 1000;
        head.seq = heldSeq + 1;
        head.link = keccak256(abi.encode("link", epoch, heldSeq + 1));
        (head.parentSeq, head.parentLink) = (heldSeq, heldLink);
        (head.turn, head.position, head.generation) = (inEffect, current.position, current.generation);
        head.release = registry.candidateOf(current.candidate).release;
        head.stamp = nowMs;
        head.signedAt = nowMs > heldSignedAt ? nowMs : heldSignedAt + 1;
        head.confirmer = confirmer;
    }

    /// @dev `head` as a held head under `hold`, binding Arbitrum block `blockNumber` and its hash.
    function _held(LogHeads.HeadV2 memory head, uint8 hold, uint64 blockNumber)
        internal
        pure
        returns (LogHeads.HeadV2 memory)
    {
        (head.hold, head.blockNumber, head.blockHash) = (hold, blockNumber, ARB.hashOf(blockNumber));
        return head;
    }

    /// @dev The `engine.log-head.v2` digest for the vault's current epoch, built by hand as the node
    /// builds it.
    function _digestV2(LogHeads.HeadV2 memory head) internal view returns (bytes32) {
        return keccak256(
            bytes.concat(
                abi.encodePacked(
                    "engine.log-head.v2",
                    uint64(block.chainid),
                    address(vault),
                    vault.signerEpoch(),
                    head.seq,
                    head.link,
                    head.parentSeq,
                    head.parentLink,
                    head.turn
                ),
                abi.encodePacked(
                    head.position,
                    head.release,
                    head.generation,
                    head.stamp,
                    head.signedAt,
                    head.hold,
                    head.confirmer,
                    head.blockNumber,
                    head.blockHash
                )
            )
        );
    }

    function _sign(LogHeads.HeadV2 memory head, uint256 key) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, _digestV2(head));
        return abi.encodePacked(r, s, v);
    }

    /// @dev Posts `head` signed by the vault's current signer.
    function _post(LogHeads.HeadV2 memory head) internal {
        logHeads.postV2(head, _sign(head, _signerKey()));
    }

    /// @dev Posts the next live head.
    function _beat() internal returns (LogHeads.HeadV2 memory head) {
        head = _nextHead();
        _post(head);
    }

    /// @dev The epoch's held head.
    function _heldHead() internal view returns (LogHeads.Head memory head) {
        (head.seq, head.link, head.signedAt, head.boundBlock, head.postedAt) = logHeads.heads(vault.signerEpoch());
    }
}
