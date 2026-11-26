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
/// block time, signer epoch, member, release and generation. Each signer epoch whose head binds it
/// while it is current records that the assignment held office under that epoch (`boundUnder`), the
/// only epochs `FaultProof` takes a fault of the assignment in. Each epoch's newest v2 head's release
/// names the enclave holding the keys, whichever assignment it binds, and `keyHolder` names that
/// enclave's candidate for the schedule's skips and promotions.
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
    /// @dev Any other `hold` reverts `InvalidHold`.
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

    /// @dev `OperatorSchedule.Turn.reason` for a promotion, as the schedule numbers it.
    uint8 private constant REASON_PROMOTED = 2;

    /// The newest posted head per signer epoch; empty until the first post of that epoch.
    mapping(uint64 epoch => Head) public heads;

    /// The latch of each turn's assignment, by its position; empty until the first head binding it.
    mapping(uint64 turn => mapping(uint16 position => Latch)) public latches;

    /// Whether a head of signer epoch `epoch` bound turn `turn`'s assignment at `position` while it was
    /// the turn in effect's current one, as a latch requires: the epochs the assignment held office under.
    mapping(uint64 turn => mapping(uint16 position => mapping(uint64 epoch => bool))) public boundUnder;

    /// The release of each signer epoch's newest v2 head: the enclave that signed it, which holds the
    /// keys whatever assignment it binds, so a member taking a turn asks it first (#596).
    mapping(uint64 epoch => bytes32 release) public headRelease;

    event HeadPosted(uint64 indexed epoch, uint64 indexed seq, bytes32 link);
    /// @notice A v2 heartbeat was recorded as the epoch's newest head.
    event HeadPostedV2(
        uint64 indexed epoch,
        uint64 indexed seq,
        bytes32 link,
        uint64 turn,
        uint16 position,
        uint8 hold,
        uint64 signedAt
    );
    /// @notice A head latched turn `turn`'s assignment at `position`, as `latches` records it.
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
        headRelease[epoch] = head.release;
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

    /// @notice The address whose key signed `digest` with `signature`, with no message prefix; zero for a
    /// malformed signature or one with a high `s`. The schedule checks standby vouches with it.
    function recover(bytes32 digest, bytes calldata signature) external pure returns (address signer) {
        (signer,,) = ECDSA.tryRecover(digest, signature);
    }

    /// @notice The candidate whose enclave holds the vault's signer epoch's keys while a turn of the registry's
    /// schedule names the operator, zero when nothing says. While the epoch's newest head is a v2 head its
    /// enclave signed since the schedule's `registryTurn` began, however late it was relayed, it is the
    /// candidate the registry binds the head's release to for good (`OperatorRegistry.releaseOwner`), so a
    /// release its candidate moved off still names it; zero for a release the registry binds to none, which
    /// leaves the holder unknown. Otherwise no member has taken the keys in the epoch since then, so they are
    /// where it found them: walking back from the turn in effect over turns whose latest assignment no head
    /// latched and no promotion made, the member of the first turn reached whose latest assignment one did; in
    /// a turn the epoch began in, the member the vault's timelock installed as the operator the registry named,
    /// which a skip that moved the turn on since recorded (`skippedHolder`); else the source of `registryTurn`,
    /// where the walk ends. One step per turn run with no head, each at a cost that does not grow with the
    /// turn's revisions.
    function keyHolder() external view returns (address holder, uint64 epoch) {
        OperatorRegistry registry = vault.registry();
        OperatorSchedule schedule = registry.schedule();
        epoch = vault.signerEpoch();
        uint64 first = schedule.registryTurn();
        bytes32 release = headRelease[epoch];
        if (release != 0 && heads[epoch].signedAt >= uint256(schedule.boundary(first)) * 1000) {
            return (registry.releaseOwner(release), epoch);
        }
        uint64 since = vault.signerSince();
        (uint64 turn, OperatorSchedule.Turn memory named) = schedule.turnAt(uint64(block.timestamp));
        while (named.reason != REASON_PROMOTED && latches[turn][named.position].at == 0) {
            if (since >= schedule.boundary(turn)) {
                holder = schedule.skippedHolder(epoch);
                return (holder == address(0) ? named.candidate : holder, epoch);
            }
            holder = named.source;
            if (turn == first) return (holder, epoch);
            turn = named.prior;
            named = schedule.turnOf(turn);
        }
        return (named.candidate, epoch);
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

    /// @dev When the head's assignment is the turn in effect's current one, on the release the registry
    /// names for its member, records that it held office under `epoch` and latches it if nothing has. A
    /// head live or held may; one binding a superseded assignment still counts as a heartbeat but records
    /// nothing.
    function _latch(OperatorSchedule schedule, uint64 epoch, bytes32 release, HeadV2 calldata head) private {
        mapping(uint64 => bool) storage bound = boundUnder[head.turn][head.position];
        bool latched = latches[head.turn][head.position].at != 0;
        if (latched && bound[epoch]) return;
        (uint64 turn, OperatorSchedule.Turn memory current) = schedule.turnAt(uint64(block.timestamp));
        if (
            head.turn != turn || head.position != current.position || head.generation != current.generation
                || head.release != release
        ) return;
        bound[epoch] = true;
        if (latched) return;
        latches[turn][head.position] =
            Latch(uint64(block.timestamp), epoch, current.candidate, release, current.generation);
        emit TurnLatched(turn, head.position, current.candidate, epoch, release, current.generation);
    }

    /// @dev The `engine.log-head.v2` digest: every field packed after the domain, the chain, the vault
    /// and the epoch, in two parts that concatenate to one packed encoding of 261 bytes.
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
