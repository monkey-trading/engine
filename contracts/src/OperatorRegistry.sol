// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {Staking} from "./Staking.sol";

/// @dev The vault the liveness clock reads: the registry asks it when a checkpoint last posted.
interface IVault {
    function lastCheckpointAt() external view returns (uint64);
}

/// @title OperatorRegistry
/// @notice Elects the operator, whose key signs the engine's admin commands, by delegated MONKEY
/// stake. Anyone may register as a candidate with a self-bond of its own stake, which the staking
/// contract this registry deploys locks while the candidate is registered and for one cooldown after
/// it leaves. A candidate is eligible once the timelock has approved its release, and while its
/// self-bond and its share of total stake meet the minimums.
/// @notice Anyone may close an election epoch once it has ended. While rotation is on and no
/// election is waiting to take effect, the close elects the eligible candidate with the most
/// delegated stake, if that is not the operator in effect, a quorum of total stake is delegated to
/// approved candidates, the operator in effect has served its minimum term, and the winner's votes exceed the
/// incumbent's by the margin. The election takes effect seven days plus the finality margin after
/// the close; the engine mirrors it and applies the winner's admin key from then.
/// @notice Liveness: while rotation is on and no election waits, anyone may promote the top
/// eligible standby once the vault has had no checkpoint for `parameters.livenessTimeout`. The
/// promotion takes effect like an election, and moves the vault's current release to the standby
/// at once; the standby's registered enclave address is the only one the vault's timelock-gated
/// `setSigner` installs while rotation is on.
/// @dev Not upgradeable. Rotation starts off, with the deployment's initial operator in effect, and
/// while it is off a close elects nothing. Only the timelock turns rotation on and off (turning it
/// off cancels an election not yet in effect), approves candidates' releases, and changes the
/// parameters within the bounds fixed at deployment. The operator in effect, and a candidate elected
/// and waiting to take effect, cannot leave: an operator leaves by losing a vote. A candidate
/// registers an admin key only with that key's EIP-712 authorization, so no one can reserve a key
/// the candidate advertised before it registers.
contract OperatorRegistry {
    /// @notice The election's adjustable values; the timelock changes them within deployment bounds.
    struct Parameters {
        /// @notice Election epoch length, in seconds.
        uint64 epochLength;
        /// @notice How long the operator in effect serves before the role can move, in seconds.
        uint64 minTerm;
        /// @notice Added to the seven-day notice before an election takes effect, in seconds.
        uint64 finalityMargin;
        /// @notice How far a challenger's votes must exceed the incumbent's, in basis points of the
        /// incumbent's.
        uint16 marginBps;
        /// @notice A candidate's smallest share of total stake to be eligible, in basis points.
        uint16 eligibilityBps;
        /// @notice The smallest share of total stake delegated to approved candidates for the role to
        /// move, in basis points.
        uint16 quorumBps;
        /// @notice The most approved candidates listed at once.
        uint16 maxCandidates;
        /// @notice The smallest self-bond, in MONKEY base units.
        uint256 minSelfBond;
        /// @notice Seconds without a vault checkpoint after which anyone may promote the top
        /// eligible standby.
        uint64 livenessTimeout;
    }

    /// @notice What a candidate registers. The release is the enclave build and config it runs:
    /// its 48-byte SEV-SNP launch measurement and the SHA-256 of its config file. The enclave is
    /// the address its enclave's key attests, which the vault's limited `setSigner` installs.
    struct Registration {
        address adminKey;
        address enclave;
        uint256 bond;
        bytes launchDigest;
        bytes32 configHash;
        string apiHostname;
        string handoverEndpoint;
    }

    /// @notice A registered candidate; `release` is `keccak256(launchDigest ‖ configHash)`, as the
    /// vault names a release.
    struct Candidate {
        address adminKey;
        address enclave;
        uint256 bond;
        bytes32 release;
        /// @notice Registrations so far when this one was made, which breaks a tie in votes.
        uint64 registration;
        string apiHostname;
        string handoverEndpoint;
    }

    /// @notice The operator in effect, and since when: its term starts there.
    struct Operator {
        address candidate;
        address adminKey;
        bytes32 release;
        uint64 since;
    }

    /// @notice The operator the election in effect replaced, and the handover data its candidacy
    /// registered when the election was made: the successor it hands the keys to reads this, since
    /// the incumbent's candidacy may be gone by the time the successor starts. Empty before any
    /// election has been made.
    struct HandoverSource {
        address candidate;
        bytes32 release;
        string apiHostname;
        string handoverEndpoint;
    }

    /// @notice An election that takes effect at `effectiveAt`. A promotion (`promoted`) moves
    /// the vault's release at once; an election moves it at the effective time, as #359 §10
    /// decides.
    struct Election {
        uint64 epoch;
        address candidate;
        address adminKey;
        bytes32 release;
        uint64 effectiveAt;
        bool promoted;
    }

    /// @notice The initial operator in effect: the deployment's key and release. Its candidate
    /// identity is its admin key's address.
    struct InitialOperator {
        address adminKey;
        bytes launchDigest;
        bytes32 configHash;
    }

    /// @notice How long after its close an election takes effect, before the finality margin: the
    /// engine's notice for an operator key rotation.
    uint256 public constant NOTICE = 7 days;

    /// @notice The EIP-712 type the admin key signs to authorize its registration by a candidate.
    bytes32 public constant REGISTRATION_AUTHORIZATION_TYPEHASH =
        keccak256("RegistrationAuthorization(address candidate,address adminKey)");

    /// @dev The EIP-712 domain this registry signs registrations under, as the vault's vocabulary
    /// builds it, without the inherited contract: the registry sits at the contract size limit.
    bytes32 private constant DOMAIN_TYPEHASH =
        keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");

    /// @notice The largest candidate cap any parameters may set: it bounds the gas of a close, which
    /// reads every candidate.
    uint256 public constant CANDIDATE_LIMIT = 100;

    uint256 private constant BPS = 10_000;
    uint256 private constant MAX_HOSTNAME_BYTES = 253;
    uint256 private constant MAX_ENDPOINT_BYTES = 512;

    /// @notice The staking contract this registry deployed, whose delegations vote.
    Staking public immutable staking;

    /// @notice The deployment's timelock.
    TimelockController public immutable governance;

    /// @notice The vault whose checkpoints the liveness clock reads; set once by governance after
    /// deployment, since the vault and the registry name each other.
    IVault public vault;

    /// @notice The parameters in force.
    Parameters public parameters;

    /// @notice The lowest and highest value of each parameter, fixed at deployment.
    Parameters public minimums;
    Parameters public maximums;

    /// @notice Whether closes elect; off at deployment.
    bool public rotationEnabled;

    /// @notice The election epoch now open, and when it started.
    uint64 public openEpoch;
    uint64 public openEpochStart;

    /// @notice Registered candidates whose release the timelock has approved, in no fixed order. The
    /// candidate cap bounds this list's length, so unapproved registrations cannot fill it and the
    /// close's scan stays bounded.
    address[] public candidateList;

    /// @notice Each registered candidate's admin key's holder, so no two share a key.
    mapping(address adminKey => address candidate) public keyHolder;

    /// @notice Releases the timelock approved, keyed by `keccak256(abi.encode(candidate, release))`.
    mapping(bytes32 approval => bool approved) public releaseApproved;

    Operator private current;
    Election private pending;
    /// @notice The operator the last election to take effect replaced, for its successor's handover.
    HandoverSource private source;
    mapping(address candidate => Candidate) private candidates;
    /// @notice One more than each approved candidate's index in `candidateList`: nonzero while it is
    /// registered and approved, zero when it is registered but unapproved or not registered.
    mapping(address candidate => uint256 position) private listed;
    uint64 private registrations;

    event CandidateRegistered(
        address indexed candidate,
        address indexed adminKey,
        bytes32 indexed release,
        address enclave,
        uint256 bond,
        bytes launchDigest,
        bytes32 configHash,
        string apiHostname,
        string handoverEndpoint
    );
    event CandidateDeregistered(address indexed candidate);
    event CandidateEnclaveChanged(address indexed candidate, address indexed enclave);
    event ReleaseApprovalChanged(address indexed candidate, bytes32 indexed release, bool approved);

    /// @notice Rotation turned on or off; `electionCancelled` when turning it off cancelled an
    /// election that had not yet taken effect.
    event RotationChanged(bool enabled, bool electionCancelled);

    event EpochClosed(uint64 indexed epoch);

    /// @notice The close of `epoch` elected `candidate`, whose `adminKey` and `release` take effect at
    /// `effectiveAt`.
    event Elected(
        uint64 indexed epoch, address indexed candidate, address adminKey, bytes32 release, uint64 effectiveAt
    );

    event ParametersChanged(Parameters parameters);

    error ZeroAddress();
    error InvalidGovernance();
    error UnauthorizedGovernance();
    error InvalidBounds();
    error ParameterOutOfBounds();
    error InvalidLaunchDigest();
    error InvalidEndpoint();
    error AlreadyRegistered();
    error NotRegistered();
    error CandidateCapReached(uint256 cap);
    error AdminKeyTaken(address candidate);
    error BondBelowMinimum(uint256 bond, uint256 minimum);
    error OperatorCannotLeave();
    error UnauthorizedAdminKey();
    error RotationUnchanged(bool enabled);
    error EpochNotEnded(uint256 endsAt);
    error RotationDisabled();
    error ElectionPending();
    error VaultNotSet();
    error VaultAlreadySet(address vault);
    error LivenessNotDue(uint256 dueAt);
    error NoQualifiedStandby();

    /// @param monkey_ The MONKEY token the staking contract holds.
    /// @param cooldownSeconds_ The staking contract's unstake cooldown, in seconds, which also keeps
    /// a departed candidate's self-bond locked.
    /// @param epochSeconds_ The engine's distribution epoch length, in seconds.
    /// @param governance_ The deployment's timelock.
    /// @param initial The operator in effect until an election takes effect.
    /// @param parameters_ The parameters in force at deployment, within the bounds.
    /// @param minimums_ The lowest value of each parameter.
    /// @param maximums_ The highest value of each parameter.
    constructor(
        IERC20 monkey_,
        uint256 cooldownSeconds_,
        uint256 epochSeconds_,
        TimelockController governance_,
        InitialOperator memory initial,
        Parameters memory parameters_,
        Parameters memory minimums_,
        Parameters memory maximums_
    ) {
        if (address(governance_).code.length == 0 || governance_.getMinDelay() == 0) {
            revert InvalidGovernance();
        }
        if (initial.adminKey == address(0)) revert ZeroAddress();
        if (
            minimums_.epochLength == 0 || minimums_.livenessTimeout == 0 || minimums_.maxCandidates == 0
                || maximums_.marginBps > BPS || maximums_.eligibilityBps > BPS || maximums_.quorumBps > BPS
                || maximums_.maxCandidates > CANDIDATE_LIMIT
        ) revert InvalidBounds();
        minimums = minimums_;
        maximums = maximums_;
        _checkBounds(parameters_);
        parameters = parameters_;
        governance = governance_;
        staking = new Staking(monkey_, cooldownSeconds_, epochSeconds_);
        current = Operator(
            initial.adminKey,
            initial.adminKey,
            _release(initial.launchDigest, initial.configHash),
            uint64(block.timestamp)
        );
        openEpochStart = uint64(block.timestamp);
    }

    modifier onlyGovernance() {
        if (msg.sender != address(governance)) revert UnauthorizedGovernance();
        _;
    }

    /// @notice Registers the caller as a candidate, locking `registration.bond` of its stake as its
    /// self-bond, with `signature` the admin key's EIP-712 authorization of this candidate under
    /// `registrationDigest`, so a key is registered only for a candidate it authorized. The candidate
    /// enters `candidateList`, and counts toward the candidate cap, once the
    /// timelock approves its release; until then it is registered but unlisted, cannot be elected, and
    /// does not count toward the cap, so unapproved registrations cannot fill it. A candidate that
    /// left may register again once its previous self-bond is free; it re-enters the list at once when
    /// its release is still approved, under the cap in force then.
    function register(Registration calldata registration, bytes calldata signature) external {
        if (candidates[msg.sender].adminKey != address(0)) revert AlreadyRegistered();
        address adminKey = registration.adminKey;
        if (adminKey == address(0) || registration.enclave == address(0)) revert ZeroAddress();
        if (!_authorizedByAdminKey(msg.sender, adminKey, signature)) revert UnauthorizedAdminKey();
        if (keyHolder[adminKey] != address(0)) revert AdminKeyTaken(keyHolder[adminKey]);
        if (registration.bond < parameters.minSelfBond) {
            revert BondBelowMinimum(registration.bond, parameters.minSelfBond);
        }
        _checkEndpoint(registration);
        bytes32 release = _release(registration.launchDigest, registration.configHash);
        candidates[msg.sender] = Candidate(
            adminKey,
            registration.enclave,
            registration.bond,
            release,
            registrations++,
            registration.apiHostname,
            registration.handoverEndpoint
        );
        keyHolder[adminKey] = msg.sender;
        emit CandidateRegistered(
            msg.sender,
            adminKey,
            release,
            registration.enclave,
            registration.bond,
            registration.launchDigest,
            registration.configHash,
            registration.apiHostname,
            registration.handoverEndpoint
        );
        staking.lockBond(msg.sender, registration.bond);
        if (releaseApproved[keccak256(abi.encode(msg.sender, release))]) _enlist(msg.sender);
        // A release approved before this registration (or while it was away) lists it at once.
    }

    /// @notice The EIP-712 digest the admin key signs to authorize `candidate`'s registration of
    /// `adminKey`; the domain names this registry on this chain.
    function registrationDigest(address candidate, address adminKey) public view returns (bytes32) {
        bytes32 domain = keccak256(
            abi.encode(
                DOMAIN_TYPEHASH, keccak256("EngineOperatorRegistry"), keccak256("1"), block.chainid, address(this)
            )
        );
        return keccak256(
            abi.encodePacked(
                hex"1901", domain, keccak256(abi.encode(REGISTRATION_AUTHORIZATION_TYPEHASH, candidate, adminKey))
            )
        );
    }

    /// @dev Reverts unless the registration names a nonempty, bounded endpoint.
    function _checkEndpoint(Registration calldata registration) private pure {
        uint256 hostname = bytes(registration.apiHostname).length;
        uint256 endpoint = bytes(registration.handoverEndpoint).length;
        if (hostname == 0 || hostname > MAX_HOSTNAME_BYTES || endpoint == 0 || endpoint > MAX_ENDPOINT_BYTES) {
            revert InvalidEndpoint();
        }
    }

    /// @dev Whether `signature` is the admin key's authorization of `candidate`'s registration of
    /// `adminKey`. `tryRecover`, not `recover`: the comparison below is the refusal, and a malformed
    /// signature recovering no one is one.
    function _authorizedByAdminKey(address candidate, address adminKey, bytes calldata signature)
        private
        view
        returns (bool)
    {
        (address recovered,,) = ECDSA.tryRecover(registrationDigest(candidate, adminKey), signature);
        return recovered == adminKey;
    }

    /// @notice Removes the caller's candidacy; its self-bond stays locked for one cooldown. The
    /// operator in effect, and a candidate elected and waiting to take effect, cannot leave.
    function deregister() external {
        Candidate memory registered = candidates[msg.sender];
        if (registered.adminKey == address(0)) revert NotRegistered();
        _settle();
        if (msg.sender == current.candidate || (pending.effectiveAt != 0 && msg.sender == pending.candidate)) {
            revert OperatorCannotLeave();
        }
        if (listed[msg.sender] != 0) _delist(msg.sender);
        delete keyHolder[registered.adminKey];
        delete candidates[msg.sender];
        emit CandidateDeregistered(msg.sender);
        staking.releaseBond(msg.sender);
    }

    /// @notice Updates the enclave address the caller's candidacy registers: the only address the
    /// vault's limited `setSigner` installs while rotation is on. A standby promotes with fresh
    /// enclave keys, which it registers here before the timelock installs them.
    function setEnclave(address enclave) external {
        Candidate storage registered = candidates[msg.sender];
        if (registered.adminKey == address(0)) revert NotRegistered();
        if (enclave == address(0)) revert ZeroAddress();
        registered.enclave = enclave;
        emit CandidateEnclaveChanged(msg.sender, enclave);
    }

    /// @notice Names the deployment's vault, whose `lastCheckpointAt` the liveness clock reads.
    /// Set once, after both contracts deploy.
    function setVault(address vault_) external onlyGovernance {
        if (vault_.code.length == 0) revert ZeroAddress();
        if (address(vault) != address(0)) revert VaultAlreadySet(address(vault));
        vault = IVault(vault_);
    }

    /// @notice Approves, or withdraws approval of, `candidate` running the release with SEV-SNP launch
    /// measurement `launchDigest` and config SHA-256 `configHash`. Approving a registered candidate
    /// adds it to `candidateList` under the candidate cap; withdrawing approval removes it from the
    /// list while it stays registered.
    function setReleaseApproval(address candidate, bytes calldata launchDigest, bytes32 configHash, bool approved)
        external
        onlyGovernance
    {
        bytes32 release = _release(launchDigest, configHash);
        releaseApproved[keccak256(abi.encode(candidate, release))] = approved;
        emit ReleaseApprovalChanged(candidate, release, approved);
        Candidate memory registered = candidates[candidate];
        // Listing follows the release the candidate registered, not any other release the timelock
        // names for it.
        if (registered.adminKey == address(0) || registered.release != release) return;
        if (approved) {
            if (listed[candidate] == 0) _enlist(candidate);
        } else if (listed[candidate] != 0) {
            _delist(candidate);
        }
    }

    /// @notice Turns rotation on or off. Turning it off cancels an election not yet in effect, and
    /// its handover source with it: no successor hands over while rotation is off.
    function setRotation(bool enabled) external onlyGovernance {
        if (enabled == rotationEnabled) revert RotationUnchanged(enabled);
        _settle();
        bool cancelled = !enabled && pending.effectiveAt != 0;
        if (cancelled) {
            delete pending;
            delete source;
        }
        rotationEnabled = enabled;
        emit RotationChanged(enabled, cancelled);
    }

    /// @notice Sets the parameters, each within the bounds fixed at deployment. A lower candidate cap
    /// removes no listed candidate; it refuses new approvals, and approved re-registrations, until the
    /// count is under it.
    function setParameters(Parameters calldata parameters_) external onlyGovernance {
        _checkBounds(parameters_);
        parameters = parameters_;
        emit ParametersChanged(parameters_);
    }

    /// @notice Closes the latest election epoch that has ended, skipping any that ended unclosed, and
    /// elects as the contract's rule says.
    /// @return epoch The epoch closed.
    function closeEpoch() external returns (uint64 epoch) {
        Parameters memory p = parameters;
        uint256 endsAt = uint256(openEpochStart) + p.epochLength;
        if (block.timestamp < endsAt) revert EpochNotEnded(endsAt);
        uint256 elapsed = (block.timestamp - openEpochStart) / p.epochLength;
        epoch = openEpoch + uint64(elapsed) - 1;
        openEpoch += uint64(elapsed);
        openEpochStart += uint64(elapsed * p.epochLength);
        _settle();
        emit EpochClosed(epoch);
        if (!rotationEnabled || pending.effectiveAt != 0) return epoch;
        address winner = _winner(p);
        if (winner == address(0)) return epoch;
        Candidate storage elected = candidates[winner];
        uint64 effectiveAt = uint64(block.timestamp + NOTICE + p.finalityMargin);
        pending = Election(epoch, winner, elected.adminKey, elected.release, effectiveAt, false);
        source = _source();
        emit Elected(epoch, winner, elected.adminKey, elected.release, effectiveAt);
    }

    /// @notice Promotes the top eligible standby after the liveness timeout: anyone may call once
    /// the vault has had no checkpoint for `parameters.livenessTimeout`. The promoted candidate
    /// becomes the pending operator in effect: the vault's current release is its at once, and its
    /// admin key takes effect at `now + NOTICE + finalityMargin`, as an election's does. The vault
    /// installs the standby's registered enclave address through its limited `setSigner` in that
    /// time. Reverts when no standby qualifies, leaving the operator in effect unchanged.
    /// @return epoch The epoch number the promotion consumes, one more than any closed or consumed.
    function promote() external returns (uint64 epoch) {
        if (!rotationEnabled) revert RotationDisabled();
        _settle();
        if (pending.effectiveAt != 0) revert ElectionPending();
        IVault liveness = vault;
        if (address(liveness) == address(0)) revert VaultNotSet();
        Parameters memory p = parameters;
        uint256 dueAt = uint256(liveness.lastCheckpointAt()) + p.livenessTimeout;
        if (block.timestamp < dueAt) revert LivenessNotDue(dueAt);
        address standby = _standby(p);
        if (standby == address(0)) revert NoQualifiedStandby();
        // Consume the epoch number a close would next assign, so every election and promotion the
        // engine mirrors carries a strictly increasing epoch.
        uint256 elapsed = (block.timestamp - openEpochStart) / p.epochLength;
        epoch = openEpoch + uint64(elapsed);
        openEpoch = epoch + 1;
        openEpochStart = uint64(block.timestamp);
        Candidate storage promoted = candidates[standby];
        uint64 effectiveAt = uint64(block.timestamp + NOTICE + p.finalityMargin);
        pending = Election(epoch, standby, promoted.adminKey, promoted.release, effectiveAt, true);
        source = _source();
        emit Elected(epoch, standby, promoted.adminKey, promoted.release, effectiveAt);
    }

    /// @notice The operator in effect: the last election once its time has come, or before it the
    /// operator it replaces.
    function operatorInEffect() external view returns (Operator memory) {
        return _inEffect();
    }

    /// @notice The enclave address the named operator's candidate registered: the only address the
    /// vault's limited `setSigner` installs while rotation is on. A promotion names its candidate
    /// at once; an election names the winner only from its effective time. Zero when the named
    /// operator is not a registered candidate.
    function operatorEnclave() external view returns (address) {
        return candidates[_named().candidate].enclave;
    }

    /// @notice The release the vault names while rotation is on: the named operator's. A promoted
    /// standby's enclave unseals only under the release it runs, so a promotion names it at once;
    /// an election's release moves at its effective time, as #359 §10 decides.
    function operatorRelease() external view returns (bytes32) {
        return _named().release;
    }

    /// @notice The operator the election in effect replaced, and the handover data its candidacy
    /// registered when the election was made: the successor connects at `handoverEndpoint` to take
    /// the keys (#44), from the moment the election exists, without a transaction to settle it.
    /// Empty before any election has been made, and when the replaced operator was never a
    /// registered candidate, so its endpoint was never on chain; the runbook registers the
    /// operator in effect as a candidate.
    function handoverSource() external view returns (HandoverSource memory) {
        return source;
    }

    /// @dev The operator in effect, pending one settled.
    function _inEffect() private view returns (Operator memory) {
        if (pending.effectiveAt != 0 && block.timestamp >= pending.effectiveAt) {
            return Operator(pending.candidate, pending.adminKey, pending.release, pending.effectiveAt);
        }
        return current;
    }

    /// @dev The handover data of the operator in effect now: the operator a new election names
    /// replaces, snapshotted when the election is made, since the incumbent's candidacy may be
    /// gone by the time the successor starts.
    function _source() private view returns (HandoverSource memory) {
        Candidate memory leaving = candidates[current.candidate];
        return HandoverSource(current.candidate, current.release, leaving.apiHostname, leaving.handoverEndpoint);
    }

    /// @dev The operator the vault names while rotation is on: a pending promotion from the moment
    /// it is made, a pending election from its effective time, else the operator in effect.
    function _named() private view returns (Operator memory) {
        if (pending.effectiveAt != 0 && (pending.promoted || block.timestamp >= pending.effectiveAt)) {
            return Operator(pending.candidate, pending.adminKey, pending.release, pending.effectiveAt);
        }
        return current;
    }

    /// @notice The election waiting to take effect, or a zero election when none is.
    function pendingElection() external view returns (Election memory election) {
        if (block.timestamp < pending.effectiveAt) election = pending;
    }

    /// @notice Whether the election waiting to take effect is a liveness promotion: only a
    /// promotion moves the release at once and admits a standby's takeover; an ordinary election
    /// takes effect at its time without one.
    function pendingPromotion() external view returns (bool) {
        return pending.effectiveAt != 0 && pending.promoted;
    }

    /// @notice `candidate`'s registration, zero when it is not registered.
    function candidateOf(address candidate) external view returns (Candidate memory) {
        return candidates[candidate];
    }

    /// @notice How many approved candidates are registered.
    function candidateCount() external view returns (uint256) {
        return candidateList.length;
    }

    /// @notice Whether `candidate` would count in a close now: registered, its release approved, its
    /// self-bond at the minimum, and its votes at the eligibility share of total stake.
    function isEligible(address candidate) external view returns (bool) {
        return
            listed[candidate] != 0 && _eligible(candidate, staking.votes(candidate), staking.totalStaked(), parameters);
    }

    /// @dev The eligible candidate with the most votes, the earlier registration on a tie, when the
    /// role moves to it: it is not the operator in effect, a quorum of total stake is delegated to
    /// approved candidates, the operator in effect has served its minimum term, and the winner's votes exceed
    /// the incumbent's by the margin. An incumbent that is not an eligible candidate has no votes.
    /// The zero address otherwise.
    function _winner(Parameters memory p) private view returns (address winner) {
        uint256 total = staking.totalStaked();
        uint256 delegated = 0;
        uint256 best = 0;
        uint256 incumbent = 0;
        uint64 earliest = 0;
        for (uint256 i = 0; i < candidateList.length; ++i) {
            address candidate = candidateList[i];
            uint256 cast = staking.votes(candidate);
            delegated += cast;
            if (!_eligible(candidate, cast, total, p)) continue;
            if (candidate == current.candidate) incumbent = cast;
            uint64 registration = candidates[candidate].registration;
            if (winner == address(0) || cast > best || (cast == best && registration < earliest)) {
                (winner, best, earliest) = (candidate, cast, registration);
            }
        }
        if (winner == address(0) || winner == current.candidate) return address(0);
        if (delegated * BPS < uint256(p.quorumBps) * total) return address(0);
        if (block.timestamp < uint256(current.since) + p.minTerm) return address(0);
        if (best * BPS <= incumbent * (BPS + p.marginBps)) return address(0);
    }

    /// @dev The eligible candidate with the most votes, the earlier registration on a tie, other
    /// than the operator in effect: the standby the liveness rule promotes. Unlike `_winner`, no
    /// quorum, margin, or minimum term: the incumbent has failed, so any eligible standby serves.
    function _standby(Parameters memory p) private view returns (address standby) {
        uint256 total = staking.totalStaked();
        uint256 best = 0;
        uint64 earliest = 0;
        for (uint256 i = 0; i < candidateList.length; ++i) {
            address candidate = candidateList[i];
            if (candidate == current.candidate) continue;
            uint256 cast = staking.votes(candidate);
            if (!_eligible(candidate, cast, total, p)) continue;
            uint64 registration = candidates[candidate].registration;
            if (standby == address(0) || cast > best || (cast == best && registration < earliest)) {
                (standby, best, earliest) = (candidate, cast, registration);
            }
        }
    }

    /// @dev Whether registered `candidate`, with `cast` votes of `total` stake, is eligible under `p`.
    function _eligible(address candidate, uint256 cast, uint256 total, Parameters memory p)
        private
        view
        returns (bool)
    {
        Candidate storage registered = candidates[candidate];
        return registered.bond >= p.minSelfBond && releaseApproved[keccak256(abi.encode(candidate, registered.release))]
            && cast * BPS >= uint256(p.eligibilityBps) * total;
    }

    /// @dev Makes an election whose time has come the operator in effect.
    function _settle() private {
        if (pending.effectiveAt == 0 || block.timestamp < pending.effectiveAt) return;
        current = Operator(pending.candidate, pending.adminKey, pending.release, pending.effectiveAt);
        delete pending;
    }

    /// @dev Reverts unless each of `p`'s values is within the deployment's bounds.
    function _checkBounds(Parameters memory p) private view {
        Parameters memory lo = minimums;
        Parameters memory hi = maximums;
        if (
            p.epochLength < lo.epochLength || p.epochLength > hi.epochLength || p.minTerm < lo.minTerm
                || p.minTerm > hi.minTerm || p.finalityMargin < lo.finalityMargin
                || p.finalityMargin > hi.finalityMargin || p.marginBps < lo.marginBps || p.marginBps > hi.marginBps
                || p.eligibilityBps < lo.eligibilityBps || p.eligibilityBps > hi.eligibilityBps
                || p.quorumBps < lo.quorumBps || p.quorumBps > hi.quorumBps || p.maxCandidates < lo.maxCandidates
                || p.maxCandidates > hi.maxCandidates || p.minSelfBond < lo.minSelfBond
                || p.minSelfBond > hi.minSelfBond || p.livenessTimeout < lo.livenessTimeout
                || p.livenessTimeout > hi.livenessTimeout
        ) revert ParameterOutOfBounds();
    }

    /// @dev Adds registered `candidate` to `candidateList`, refusing a full approved cap. Only
    /// approved candidates count toward the cap, so unapproved registrations cannot fill it.
    function _enlist(address candidate) private {
        Parameters memory p = parameters;
        if (candidateList.length >= p.maxCandidates) revert CandidateCapReached(p.maxCandidates);
        candidateList.push(candidate);
        listed[candidate] = candidateList.length;
    }

    /// @dev Removes `candidate` from `candidateList`, keeping it registered.
    function _delist(address candidate) private {
        uint256 position = listed[candidate];
        address last = candidateList[candidateList.length - 1];
        candidateList[position - 1] = last;
        listed[last] = position;
        candidateList.pop();
        delete listed[candidate];
    }

    /// @dev The vault's name for a release: `keccak256` of its 48-byte launch measurement and its
    /// config's SHA-256.
    function _release(bytes memory launchDigest, bytes32 configHash) private pure returns (bytes32) {
        if (launchDigest.length != 48) revert InvalidLaunchDigest();
        return keccak256(abi.encodePacked(launchDigest, configHash));
    }
}
