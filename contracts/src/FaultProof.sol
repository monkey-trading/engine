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

    /// @notice One head of a proof: the head, its signature, and the index of the assignment's revision
    /// whose release and generation it binds.
    struct Evidence {
        LogHeads.HeadV2 head;
        bytes signature;
        uint256 revision;
    }

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
    /// release and generation are the assignment's revision its evidence names (`ReleaseNotAssigned`).
    function prove(uint64 epoch, Evidence calldata a, Evidence calldata b) external returns (address member) {
        LogHeads.HeadV2 calldata x = a.head;
        LogHeads.HeadV2 calldata y = b.head;
        if (x.seq != y.seq || x.link == y.link) revert NotConflicting();
        if (x.turn != y.turn || x.position != y.position || x.release != y.release) revert DifferentAssignment();
        if (proven[x.turn][x.position]) revert AlreadyProven(x.turn, x.position);
        address signer = vault.signerAtEpoch(epoch);
        if (ECDSA.recover(_digest(epoch, x), a.signature) != signer) revert InvalidSigner();
        if (ECDSA.recover(_digest(epoch, y), b.signature) != signer) revert InvalidSigner();
        member = _member(a, b);
        proven[x.turn][x.position] = true;
        emit FaultProven(member, x.turn, x.position, epoch, x.seq);
    }

    /// @dev The member of the heads' assignment, once each head's release and generation are the
    /// revision its evidence names.
    function _member(Evidence calldata a, Evidence calldata b) private view returns (address member) {
        OperatorRegistry registry = vault.registry();
        if (address(registry) == address(0)) revert NoSchedule();
        OperatorSchedule schedule = registry.schedule();
        if (address(schedule) == address(0)) revert NoSchedule();
        member = _checkRevision(schedule, a);
        _checkRevision(schedule, b);
    }

    /// @dev The assignment's member, once `evidence`'s head binds the release and generation of the
    /// revision it names; generations count from one, so a revision past the last never matches.
    function _checkRevision(OperatorSchedule schedule, Evidence calldata evidence) private view returns (address) {
        LogHeads.HeadV2 calldata head = evidence.head;
        (address candidate, OperatorSchedule.Revision memory revision) =
            schedule.revisionOf(head.turn, head.position, evidence.revision);
        if (candidate == address(0)) revert NoAssignment(head.turn, head.position);
        if (revision.generation == 0 || revision.release != head.release || revision.generation != head.generation) {
            revert ReleaseNotAssigned(head.release, head.generation);
        }
        return candidate;
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
