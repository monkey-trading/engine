// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {FaultProof} from "./FaultProof.sol";
import {OperatorRegistry} from "./OperatorRegistry.sol";
import {OperatorSchedule} from "./OperatorSchedule.sol";
import {ParameterVote} from "./ParameterVote.sol";
import {Vault} from "./Vault.sol";

/// @title GovernanceRecord
/// @notice The vote contract and fault proof contract a schedule's nodes mirror, recorded once by the
/// registry's timelock. A schedule creates its record at its own deployment
/// (`OperatorSchedule.governanceRecord`), so the record names that schedule and the vault its log heads
/// follow, and a node binds the contracts it mirrors to the record rather than to their own answers about
/// their parents.
contract GovernanceRecord {
    /// @notice The schedule that created this record, whose rotation sets the vote contract must vote by.
    OperatorSchedule public immutable schedule;

    /// @notice The schedule's registry, whose timelock records the contracts.
    OperatorRegistry public immutable registry;

    /// @notice The vault the schedule's log heads follow, the only one the registry names with the schedule,
    /// whose signer history the fault proof contract must check proofs against.
    Vault public immutable vault;

    /// @notice The recorded contracts; zero until `setGovernanceContracts`.
    ParameterVote public vote;
    FaultProof public faultProof;

    event GovernanceContractsSet(address vote, address faultProof);

    error UnauthorizedGovernance();
    error GovernanceContractsRecorded();
    error InvalidGovernanceContracts();

    /// @dev Created by the schedule's constructor, which is `msg.sender`.
    constructor(OperatorRegistry registry_, Vault vault_) {
        schedule = OperatorSchedule(msg.sender);
        registry = registry_;
        vault = vault_;
    }

    /// @notice Records, once, the vote contract over the schedule and the fault proof contract over its
    /// vault: only the registry's timelock.
    function setGovernanceContracts(ParameterVote vote_, FaultProof faultProof_) external {
        if (msg.sender != address(registry.governance())) revert UnauthorizedGovernance();
        if (address(vote) != address(0)) revert GovernanceContractsRecorded();
        if (vote_.schedule() != schedule || faultProof_.vault() != vault) revert InvalidGovernanceContracts();
        vote = vote_;
        faultProof = faultProof_;
        emit GovernanceContractsSet(address(vote_), address(faultProof_));
    }
}
