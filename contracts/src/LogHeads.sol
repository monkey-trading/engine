// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.26;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {ARB_SYS} from "./ArbSys.sol";
import {OperatorRegistry} from "./OperatorRegistry.sol";
import {OperatorSchedule} from "./OperatorSchedule.sol";
import {Vault} from "./Vault.sol";

/// @title The engine's on-chain log heads (monkey-trading/engine#541, #595).
/// @notice The enclave posts its newest released `(seq, link)`, signed off the sequencer. `link` is
/// the cumulative command-prefix digest: a running hash over every command's sealed bytes that
/// checkpoints do not reset, so a posted head commits to the whole history, not just the state since
/// the last checkpoint. Every post names its parent — the previously accepted head of the same signer
/// epoch — and the contract requires the parent to match what it holds, so a longer divergent copy
/// cannot overwrite the anchor without extending it. The first post of an epoch carries no parent: a
/// signer rotation isolates chains. A node restarting from a log the posted head does not hold
/// refuses to start, naming the range.
/// @notice While a turn of the vault's registry's schedule names the operator, heads are
/// `engine.log-head.v2` heartbeats: each binds the turn assignment whose keys the enclave holds and
/// the time it was signed, which must be within the schedule's `headTolerance` of the block's, or,
/// for a head signed through a benchmark-sync hold or a trusted-time halt, a recent Arbitrum block, so
/// a host cannot bank heads and post them late. The schedule counts missed heartbeats from the
/// latest head's `postedAt`. Otherwise heads are hourly `engine.log-head.v1` anchors.
/// @notice The first head binding a turn's current assignment latches it: once, immutably, the
/// block time, signer epoch, member, release and generation.
/// Trust: the same single accepted operator dependency as the vault's checkpoints — the attested
/// enclave key the vault names. There is no admin: the vault reference is immutable, and a new
/// vault deployment pairs with a new LogHeads.
contract LogHeads {
    Vault public immutable vault;

    /// @notice An epoch's newest posted head. `signedAt` is a v2 head's signing time in
    /// milliseconds, zero for v1; `boundBlock` is a held head's Arbitrum block, zero otherwise;
    /// `postedAt` is the block time of the post.
    struct Head {
        uint64 seq;
        bytes32 link;
        uint64 signedAt;
        uint64 boundBlock;
        uint64 postedAt;
    }

    /// @notice An `engine.log-head.v2` head. `turn` and `position` name the assignment whose keys the
    /// enclave holds, `release` the enclave's release and `generation` the assignment's; `stamp` is
    /// the sequencer stamp at `seq` and `signedAt` the signing time, both in milliseconds; `hold` is
    /// 0 for a live head, 1 through a benchmark-sync hold and 2 through a trusted-time halt;
    /// `confirmer` is the member whose enclave confirmed holding the log through `seq`, bound but not
    /// checked; `blockNumber` and `blockHash` are a recent Arbitrum block for a held head, zero for a
    /// live one.
    struct HeadV2 {
        uint64 seq;
        bytes32 link;
        uint64 parentSeq;
        bytes32 parentLink;
        uint64 turn;
        uint16 position;
        bytes32 release;
        uint64 generation;
        uint64 stamp;
        uint64 signedAt;
        uint8 hold;
        address confirmer;
        uint64 blockNumber;
        bytes32 blockHash;
    }

    /// @notice The first head binding an assignment of a turn, while it was the turn's current one.
    struct Latch {
        uint64 at;
        uint64 signerEpoch;
        address member;
        bytes32 release;
        uint64 generation;
    }

    uint8 private constant LIVE = 0;
    uint8 private constant TIME_HALT = 2;

    /// The newest posted head per signer epoch; empty until the first post of that epoch.
    mapping(uint64 epoch => Head) public heads;

    /// The latch of each turn's assignment, by its position; empty until the first head binding it.
    mapping(uint64 turn => mapping(uint16 position => Latch)) public latches;

    event HeadPosted(uint64 indexed epoch, uint64 indexed seq, bytes32 link);
    event HeadPostedV2(
        uint64 indexed epoch,
        uint64 indexed seq,
        bytes32 link,
        uint64 turn,
        uint16 position,
        uint8 hold,
        uint64 signedAt
    );
    event TurnLatched(
        uint64 indexed turn,
        uint16 indexed position,
        address member,
        uint64 signerEpoch,
        bytes32 release,
        uint64 generation
    );

    error NotNewer(uint64 epoch, uint64 posted, uint64 held);
    error NotParent(uint64 epoch, uint64 parentSeq, bytes32 parentLink, uint64 heldSeq, bytes32 heldLink);
    error InvalidSigner();
    error UseV1();
    error UseV2();
    error InvalidHold(uint8 hold);
    error SigningTimeNotNewer(uint64 signedAt, uint64 held);
    error SigningTimeOutsideTolerance(uint64 signedAt, uint256 blockTimeMs);
    error BlockOutsideWindow(uint64 blockNumber, uint256 current);
    error BlockHashMismatch(uint64 blockNumber);
    error BlockNotNewer(uint64 blockNumber, uint64 bound);
    error BlockBeforeMark(uint64 blockNumber, uint64 marked);

    constructor(Vault vault_) {
        vault = vault_;
    }

    /// @notice Records the engine's newest released head for the current signer epoch, extending
    /// the head the contract already holds, while no turn names the operator (`UseV2` otherwise).
    /// @dev Anyone may relay the call; the signature, not the caller, is the authority. Gas is the
    /// relay incentive: a truncated log is the engine's and its clients' problem, and the post is
    /// what lets a restarted node refuse it. The parent requirement chains posts atomically: the
    /// signer rotation starts a fresh chain, so no parent is required while the epoch's head is
    /// empty.
    function post(uint64 seq, bytes32 link, uint64 parentSeq, bytes32 parentLink, bytes calldata signature) external {
        (,, uint64 generation) = _rotation();
        if (generation != 0) revert UseV2();
        uint64 epoch = vault.signerEpoch();
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.log-head.v1", uint64(block.chainid), address(vault), epoch, seq, link, parentSeq, parentLink
            )
        );
        _checkSigner(digest, signature);
        _extend(epoch, seq, link, parentSeq, parentLink, false);
        heads[epoch] = Head(seq, link, 0, 0, uint64(block.timestamp));
        emit HeadPosted(epoch, seq, link);
    }

    /// @notice Records a v2 heartbeat for the current signer epoch while a turn names the operator
    /// (`UseV1` otherwise). It extends the held head as `post` does, except that a held head may
    /// repeat the seq and link it extends. A live head's signing time must be newer than the held
    /// head's and within `headTolerance` of the block's. A held head instead binds an Arbitrum block
    /// among the last `headBlocks`, newer than the one the held head bound and no older than the block
    /// that marked the schedule's latest marked slot, so a cached held head cannot reset a count that
    /// witnesses are advancing. The head counts for the enclave that signed it, whoever relays it.
    function postV2(HeadV2 calldata head, bytes calldata signature) external {
        (OperatorRegistry registry, bytes32 release, uint64 generation) = _rotation();
        if (generation == 0) revert UseV1();
        uint64 epoch = vault.signerEpoch();
        _checkSigner(_digestV2(epoch, head), signature);
        _extend(epoch, head.seq, head.link, head.parentSeq, head.parentLink, head.hold != LIVE);
        OperatorSchedule schedule = registry.schedule();
        uint64 bound = _checkFresh(schedule, heads[epoch], head);
        heads[epoch] = Head(head.seq, head.link, head.signedAt, bound, uint64(block.timestamp));
        emit HeadPostedV2(epoch, head.seq, head.link, head.turn, head.position, head.hold, head.signedAt);
        _latch(schedule, epoch, release, head);
    }

    /// @dev Reverts unless a live head's signing time, or a held head's block, is fresh against the
    /// epoch's `held` head and the schedule's rule; returns the block a held head binds, zero for a
    /// live one.
    function _checkFresh(OperatorSchedule schedule, Head storage held, HeadV2 calldata head)
        private
        view
        returns (uint64 bound)
    {
        (, uint64 tolerance,, uint16 blocks) = schedule.liveness();
        if (head.hold == LIVE) {
            _checkSigningTime(head.signedAt, held.signedAt, tolerance);
            return 0;
        }
        if (head.hold > TIME_HALT) revert InvalidHold(head.hold);
        (, uint64 marked) = schedule.latestMark();
        _checkBlock(head.blockNumber, head.blockHash, held.boundBlock, marked, blocks);
        return head.blockNumber;
    }

    /// @dev The vault's registry and, while a turn of its schedule names the operator, the member's
    /// release and the turn's generation; zeros without a registry or a turn.
    function _rotation() private view returns (OperatorRegistry registry, bytes32 release, uint64 generation) {
        registry = vault.registry();
        if (address(registry) != address(0)) (, release, generation,) = registry.rotationState();
    }

    function _checkSigner(bytes32 digest, bytes calldata signature) private view {
        if (ECDSA.recover(digest, signature) != vault.checkpointSigner()) revert InvalidSigner();
    }

    /// @dev Reverts unless the head extends the epoch's held head: its parent is the held head, and
    /// its seq is newer, or, with `mayRepeat`, the held head itself.
    function _extend(uint64 epoch, uint64 seq, bytes32 link, uint64 parentSeq, bytes32 parentLink, bool mayRepeat)
        private
        view
    {
        Head storage held = heads[epoch];
        uint64 heldSeq = held.seq;
        if (heldSeq == 0) return;
        bytes32 heldLink = held.link;
        if (parentSeq != heldSeq || parentLink != heldLink) {
            revert NotParent(epoch, parentSeq, parentLink, heldSeq, heldLink);
        }
        if (seq <= heldSeq && !(mayRepeat && seq == heldSeq && link == heldLink)) revert NotNewer(epoch, seq, heldSeq);
    }

    /// @dev Reverts unless a live head's signing time is newer than the held head's and within
    /// `tolerance` seconds of the block time either way.
    function _checkSigningTime(uint64 signedAt, uint64 heldSignedAt, uint64 tolerance) private view {
        if (signedAt <= heldSignedAt) revert SigningTimeNotNewer(signedAt, heldSignedAt);
        uint256 nowMs = block.timestamp * 1000;
        uint256 window = uint256(tolerance) * 1000;
        if (signedAt + window < nowMs || signedAt > nowMs + window) {
            revert SigningTimeOutsideTolerance(signedAt, nowMs);
        }
    }

    /// @dev Reverts unless a held head's block is one of the last `blocks` Arbitrum blocks, with its
    /// hash, newer than `bound` and no older than `marked`.
    function _checkBlock(uint64 blockNumber, bytes32 blockHash, uint64 bound, uint64 marked, uint16 blocks)
        private
        view
    {
        uint256 current = ARB_SYS.arbBlockNumber();
        if (current <= blockNumber || current - blockNumber > blocks) revert BlockOutsideWindow(blockNumber, current);
        if (ARB_SYS.arbBlockHash(blockNumber) != blockHash) revert BlockHashMismatch(blockNumber);
        if (blockNumber <= bound) revert BlockNotNewer(blockNumber, bound);
        if (blockNumber < marked) revert BlockBeforeMark(blockNumber, marked);
    }

    /// @dev Latches the head's assignment when it is the turn in effect's current one, on the release
    /// the registry names for its member, and nothing has latched it.
    function _latch(OperatorSchedule schedule, uint64 epoch, bytes32 release, HeadV2 calldata head) private {
        if (latches[head.turn][head.position].at != 0) return;
        (uint64 turn, OperatorSchedule.Turn memory current) = schedule.turnAt(uint64(block.timestamp));
        if (
            head.turn != turn || head.position != current.position || head.generation != current.generation
                || head.release != release
        ) return;
        latches[turn][head.position] =
            Latch(uint64(block.timestamp), epoch, current.candidate, release, current.generation);
        emit TurnLatched(turn, head.position, current.candidate, epoch, release, current.generation);
    }

    /// @dev The `engine.log-head.v2` digest: every field packed after the domain, the chain, the vault
    /// and the epoch, in two parts that concatenate to one packed encoding.
    function _digestV2(uint64 epoch, HeadV2 calldata head) private view returns (bytes32) {
        return keccak256(
            bytes.concat(
                abi.encodePacked(
                    "engine.log-head.v2",
                    uint64(block.chainid),
                    address(vault),
                    epoch,
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
}
