// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "./OperatorRegistry.sol";
import {Staking} from "./Staking.sol";

/// @title RegistryLens
/// @notice A read-only view over an operator registry, for the node's operator list: every listed
/// candidate's registration, votes, eligibility and role, and the registry's switch, parameters and
/// named, in-effect and pending operators, in a few calls instead of several per candidate. It holds
/// no funds and no state, writes nothing, and sits outside custody: nothing in the vault, the
/// registry or the engine depends on it, and it can be deployed beside a registry at any time.
/// @dev The lists are the registry's own: `candidateList` holds approved candidates only, at most
/// `CANDIDATE_LIMIT`, so the largest page set is bounded. Every call reads one block; the node pins
/// that block, so a page and the overview agree.
contract RegistryLens {
    /// @notice A candidate's part in the registry now: an `Operator` is the one in effect, `Elected`
    /// the one an election or promotion has named and that has not yet taken effect, and `Candidate`
    /// any other listed candidate.
    enum Role {
        Candidate,
        Elected,
        Operator
    }

    /// @notice The registry's switch, parameters and operators at one block.
    struct Overview {
        bool rotationEnabled;
        OperatorRegistry.Parameters parameters;
        uint64 openEpoch;
        uint64 openEpochStart;
        /// @notice MONKEY base units staked across every account, unstaking amounts excluded.
        uint256 totalStaked;
        /// @notice Listed candidates, the length of the list `candidates` pages through.
        uint256 candidateCount;
        OperatorRegistry.Operator inEffect;
        OperatorRegistry.Operator named;
        /// @notice The election waiting to take effect; zero when none is.
        OperatorRegistry.Election pending;
        bool pendingPromotion;
    }

    /// @notice One listed candidate: its registration, its votes, whether it would count in a close
    /// now, and its role. `registration` is its registration order.
    struct Row {
        address candidate;
        address adminKey;
        address enclave;
        uint256 bond;
        bytes32 release;
        uint64 registration;
        string apiHostname;
        string handoverEndpoint;
        uint256 votes;
        bool eligible;
        Role role;
    }

    /// @notice The registry's `parameters` getter reverted, so the lens has none to return.
    error ParametersUnreadable();

    /// @notice The registry this lens reads.
    OperatorRegistry public immutable registry;

    /// @notice The registry's staking contract.
    Staking public immutable staking;

    constructor(OperatorRegistry registry_) {
        registry = registry_;
        staking = registry_.staking();
    }

    /// @notice The registry's switch, parameters, open epoch, total stake and operators now.
    function overview() external view returns (Overview memory view_) {
        view_.rotationEnabled = registry.rotationEnabled();
        view_.parameters = _parameters();
        view_.openEpoch = registry.openEpoch();
        view_.openEpochStart = registry.openEpochStart();
        view_.totalStaked = staking.totalStaked();
        view_.candidateCount = registry.candidateCount();
        view_.inEffect = registry.operatorInEffect();
        view_.named = registry.namedOperator();
        view_.pending = registry.pendingElection();
        // The registry's getter has no time check; the election read above clears once it takes effect.
        view_.pendingPromotion = view_.pending.effectiveAt != 0 && view_.pending.promoted;
    }

    /// @dev The registry's parameters in force. Its getter returns the struct's seven static fields
    /// as a tuple, which encodes as the struct does, so the return data decodes into one.
    function _parameters() private view returns (OperatorRegistry.Parameters memory) {
        (bool ok, bytes memory out) = address(registry).staticcall(abi.encodeCall(registry.parameters, ()));
        if (!ok) revert ParametersUnreadable();
        return abi.decode(out, (OperatorRegistry.Parameters));
    }

    /// @notice Up to `limit` listed candidates from position `start` of the registry's list, in the
    /// list's order, which changes when a candidate leaves it. Empty from the end of the list.
    function candidates(uint256 start, uint256 limit) external view returns (Row[] memory rows) {
        uint256 count = registry.candidateCount();
        if (start >= count) return rows;
        uint256 end = limit > count - start ? count : start + limit;
        rows = new Row[](end - start);
        address inEffect = registry.operatorInEffect().candidate;
        OperatorRegistry.Election memory pending = registry.pendingElection();
        for (uint256 i = start; i < end; ++i) {
            address candidate = registry.candidateList(i);
            OperatorRegistry.Candidate memory registered = registry.candidateOf(candidate);
            Role role = Role.Candidate;
            if (candidate == inEffect) {
                role = Role.Operator;
            } else if (pending.effectiveAt != 0 && candidate == pending.candidate) {
                role = Role.Elected;
            }
            rows[i - start] = Row({
                candidate: candidate,
                adminKey: registered.adminKey,
                enclave: registered.enclave,
                bond: registered.bond,
                release: registered.release,
                registration: registered.registration,
                apiHostname: registered.apiHostname,
                handoverEndpoint: registered.handoverEndpoint,
                votes: staking.votes(candidate),
                eligible: registry.isEligible(candidate),
                role: role
            });
        }
    }
}
