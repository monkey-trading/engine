// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {LogHeads} from "./LogHeads.sol";
import {OperatorRegistry} from "./OperatorRegistry.sol";
import {OperatorSchedule} from "./OperatorSchedule.sol";
import {Vault} from "./Vault.sol";

/// @title FaultProof (monkey-trading/engine#597)
/// @notice Proves that the enclave holding a daily turn's keys signed two histories: two
/// `engine.log-head.v2` heads, both signed by one signer epoch's key, that bind the same turn, position
/// and release and name different links at the same seq. A link is the cumulative digest of every
/// command up to its seq, so two links at one seq are two logs. The schedule's assignment at that turn
/// and position names the member, and the release each head binds, with its generation, must be one
/// the assignment recorded. `FaultProven` names that member, and the engine cuts its operator share by
/// a haircut that decays back to nothing.
/// @notice Heads under different signer epochs prove nothing: a promotion starts a new epoch and
/// discards the outgoing epoch's unposted history by design. Neither do heads naming different
/// assignments, or censorship and delay, which leave no two signed histories.
/// @notice Each assignment is proven once, so one rollback's many conflicting seqs restart the haircut
/// once, not once per seq. Anyone may submit a proof; stakers, who receive the withheld share, have
/// the reason to pay its gas. There is no admin: the vault reference is immutable, and the schedule is
/// the one the vault's registry names when the proof is submitted.
contract FaultProof {
    /// @notice The vault whose signer history and registry the proofs are checked against.
    Vault public immutable vault;

    /// @notice Whether a fault has been proven for turn `turn`'s assignment at `position`.
    mapping(uint64 turn => mapping(uint16 position => bool)) public proven;

    /// @notice The enclave holding `member`'s assignment of `turn` at `position` signed two heads under
    /// `signerEpoch` naming different links at `seq`.
    event FaultProven(address indexed member, uint64 indexed turn, uint16 position, uint64 signerEpoch, uint64 seq);

    error NotConflicting();
    error DifferentAssignment();
    error AlreadyProven(uint64 turn, uint16 position);
    error InvalidSigner();
    error NoSchedule();
    error NoAssignment(uint64 turn, uint16 position);
    error ReleaseNotAssigned(bytes32 release, uint64 generation);

    constructor(Vault vault_) {
        vault = vault_;
    }

    /// @notice Proves the fault `a` and `b` show under `epoch`, and returns the member it names.
    /// @dev Reverts unless the heads name one seq with different links (`NotConflicting`) and one turn,
    /// position and release (`DifferentAssignment`), the assignment is not yet proven (`AlreadyProven`),
    /// both signatures recover the vault's signer of `epoch` (`InvalidSigner`), the vault's registry
    /// names a schedule (`NoSchedule`) that assigned the position (`NoAssignment`), and each head's
    /// release and generation are a revision of that assignment (`ReleaseNotAssigned`).
    function prove(
        uint64 epoch,
        LogHeads.HeadV2 calldata a,
        bytes calldata signatureA,
        LogHeads.HeadV2 calldata b,
        bytes calldata signatureB
    ) external returns (address member) {
        if (a.seq != b.seq || a.link == b.link) revert NotConflicting();
        if (a.turn != b.turn || a.position != b.position || a.release != b.release) revert DifferentAssignment();
        if (proven[a.turn][a.position]) revert AlreadyProven(a.turn, a.position);
        address signer = vault.signerAtEpoch(epoch);
        if (ECDSA.recover(_digest(epoch, a), signatureA) != signer) revert InvalidSigner();
        if (ECDSA.recover(_digest(epoch, b), signatureB) != signer) revert InvalidSigner();
        member = _member(a, b);
        proven[a.turn][a.position] = true;
        emit FaultProven(member, a.turn, a.position, epoch, a.seq);
    }

    /// @dev The member of the heads' assignment, once each head's release and generation are among
    /// its revisions.
    function _member(LogHeads.HeadV2 calldata a, LogHeads.HeadV2 calldata b) private view returns (address) {
        OperatorRegistry registry = vault.registry();
        if (address(registry) == address(0)) revert NoSchedule();
        OperatorSchedule schedule = registry.schedule();
        if (address(schedule) == address(0)) revert NoSchedule();
        (OperatorSchedule.Assignment memory assignment, OperatorSchedule.Revision[] memory revisions) =
            schedule.assignmentOf(a.turn, a.position);
        if (assignment.candidate == address(0)) revert NoAssignment(a.turn, a.position);
        _checkRevision(revisions, a.release, a.generation);
        _checkRevision(revisions, b.release, b.generation);
        return assignment.candidate;
    }

    /// @dev Reverts unless `release` under `generation` is one of `revisions`.
    function _checkRevision(OperatorSchedule.Revision[] memory revisions, bytes32 release, uint64 generation)
        private
        pure
    {
        for (uint256 i = 0; i < revisions.length; ++i) {
            if (revisions[i].release == release && revisions[i].generation == generation) return;
        }
        revert ReleaseNotAssigned(release, generation);
    }

    /// @dev The `engine.log-head.v2` digest of `head` under `epoch`, as `LogHeads` computes it for the
    /// current epoch: every field packed after the domain, the chain, the vault and the epoch.
    function _digest(uint64 epoch, LogHeads.HeadV2 calldata head) private view returns (bytes32) {
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
