// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {OperatorSchedule} from "./OperatorSchedule.sol";
import {Staking} from "./Staking.sol";

/// @dev The vault the liveness clock reads: the registry asks it when a checkpoint last posted.
interface IVault {
    function lastCheckpointAt() external view returns (uint64);
}

/// @title OperatorRegistry
/// @notice Names the operator, whose key signs the engine's admin commands, by delegated MONKEY
/// stake. Anyone may register as a candidate with a self-bond of its own stake, which the staking
/// contract this registry deploys locks while the candidate is registered and for one cooldown after
/// it leaves. A candidate is eligible once the timelock has approved its release, and while its
/// self-bond and its share of total stake meet the minimums.
/// @notice Anyone may close an election epoch once it has ended; while rotation is on, the close
/// moves the role in one of two modes. A registry that names a schedule rotates daily turns: each
/// close adopts a rotation set in the schedule, and the member of the turn the schedule drew names
/// the operator from the turn's boundary. A registry without one elects: while no election is
/// waiting to take effect, the close elects the eligible candidate with the most delegated stake, if
/// that is not the operator in effect and a quorum of total stake is delegated to approved
/// candidates, seven days plus the finality margin after the close; the engine mirrors it and
/// applies the winner's admin key from then.
/// @notice Liveness, without a schedule: while rotation is on and no election waits, anyone may
/// promote the top eligible standby once the vault has had no checkpoint for
/// `parameters.livenessTimeout`. The promotion takes effect like an election, and moves the vault's
/// current release to the standby at once; the standby's registered enclave address is the only one
/// the vault's timelock-gated `setSigner` installs while rotation is on.
/// @notice Releases: a candidate moves its candidacy to a release the timelock approved for it.
/// The named operator's move is how it upgrades while rotation is on: the vault names the new
/// release at once, and the running release hands it the keys and the log.
/// @dev Not upgradeable. Rotation starts off, with the deployment's initial operator in effect, and
/// while it is off a close elects and adopts nothing. Only the timelock turns rotation on and off
/// (turning it off cancels an election not yet in effect; turns already drawn stand), names the
/// schedule, approves candidates' releases, and changes the parameters within the bounds fixed at
/// deployment. While rotation is off and no drawn turn runs, the timelock alone may also install a
/// new operator in effect (#502), the fallback for a failed operator whose key is gone. The operator
/// in effect, a candidate elected and waiting to take effect, and a member holding a turn cannot
/// leave. A candidate registers an admin key only with that key's EIP-712 authorization, so no one
/// can reserve a key the candidate advertised before it registers.
contract OperatorRegistry {
    /// @notice The election's adjustable values; the timelock changes them within deployment bounds.
    struct Parameters {
        /// @notice Election epoch length, in seconds.
        uint64 epochLength;
        /// @notice Added to the seven-day notice before an election or a rotation set takes effect,
        /// in seconds.
        uint64 finalityMargin;
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

    /// @notice The largest candidate cap any parameters may set: with the one place beyond the cap the
    /// operator in effect may take, it bounds the gas of a close, which reads every candidate.
    uint256 public constant CANDIDATE_LIMIT = 100;

    /// @notice The shortest epoch while a schedule is named: with the notice, it bounds how many
    /// rotation sets wait at once, since every close adopts one.
    uint64 public constant MIN_SCHEDULED_EPOCH = 1 days;

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

    /// @notice Whether closes elect or adopt rotation sets, and draws run; off at deployment.
    bool public rotationSwitch;

    /// @notice The schedule whose turns name the operator; set once by governance, and while it is
    /// unset the registry elects.
    OperatorSchedule public schedule;

    /// @notice The election epoch now open, and when it started.
    uint64 public openEpoch;
    uint64 public openEpochStart;

    /// @notice Registered candidates whose release the timelock has approved, in no fixed order. The
    /// candidate cap, plus one place the operator in effect may take, bounds this list's length, so
    /// unapproved registrations cannot fill it and the close's scan stays bounded.
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
    /// @notice `candidate`'s candidacy moved to `release`; for the operator in effect, the release
    /// the vault names while rotation is on.
    event CandidateReleaseChanged(
        address indexed candidate, bytes32 indexed release, bytes launchDigest, bytes32 configHash
    );
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

    /// @notice The timelock installed `candidate`, whose `adminKey` runs `release`, as the operator in
    /// effect: the governance fallback for a failed operator whose key is gone (#502).
    event OperatorInstalled(address indexed candidate, address indexed adminKey, bytes32 indexed release);

    event ParametersChanged(Parameters parameters);

    event ScheduleSet(address indexed schedule);

    error ZeroAddress();
    error InvalidGovernance();
    error UnauthorizedGovernance();
    error InvalidBounds();
    error ParameterOutOfBounds();
    error InvalidLaunchDigest();
    error InvalidEndpoint();
    error AlreadyRegistered();
    error NotRegistered();
    error ReleaseNotApproved(bytes32 release);
    error CandidateCapReached(uint256 cap);
    error AdminKeyTaken(address candidate);
    error BondBelowMinimum(uint256 bond, uint256 minimum);
    error OperatorCannotLeave();
    error UnauthorizedAdminKey();
    error RotationUnchanged(bool enabled);
    error EpochNotEnded(uint256 endsAt);
    error RotationDisabled();
    error RotationActive();
    error ElectionPending();
    error VaultNotSet();
    error VaultAlreadySet(address vault);
    error LivenessNotDue(uint256 dueAt);
    error NoQualifiedStandby();
    error InvalidSchedule();
    error ScheduleAlreadySet(address schedule);
    error ScheduleNamesTurns();
    error TurnsPending(uint64 until);
    error EpochTooShort(uint64 epochLength, uint64 minimum);

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
                || maximums_.eligibilityBps > BPS || maximums_.quorumBps > BPS
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
    /// operator in effect while no turn names one, a candidate elected and waiting to take effect,
    /// and, while a turn names the operator or drawn turns run, the member of the turn in effect, its
    /// source, or the member of the last drawn turn cannot leave.
    function deregister() external {
        Candidate memory registered = candidates[msg.sender];
        if (registered.adminKey == address(0)) revert NotRegistered();
        _settle();
        (Operator memory turn,,) = _turn();
        bool turnNamed = turn.candidate != address(0);
        if (
            (!turnNamed && msg.sender == current.candidate)
                || (pending.effectiveAt != 0 && msg.sender == pending.candidate)
                || _holdsRunningTurn(msg.sender, turnNamed)
        ) revert OperatorCannotLeave();
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

    /// @notice Moves the caller's candidacy to the release with SEV-SNP launch measurement
    /// `launchDigest` and config SHA-256 `configHash`, which the timelock must have approved for it,
    /// so no release holds the keys without the timelock's delay. When the caller is the operator in
    /// effect, its operator release moves too: while rotation is on the vault names the new release
    /// at once, the running release hands it the keys and the log, and `syncRelease` folds the change
    /// into the vault's generation. An operator the timelock installed moves the same way. A move
    /// is one-way: the approval of each release it leaves is withdrawn — the candidacy's, and for
    /// the operator in effect the one it ran, which a deployment or an installation may have named
    /// apart from its candidacy — so moving back needs the timelock to approve it again, as naming a
    /// release does while rotation is off. A candidate whose registered release was unapproved is
    /// listed again, under the candidate cap. While an election or promotion waits to take effect,
    /// neither the operator it replaces nor the candidate it names may move: the election keeps the
    /// release and the handover source it was made with. A turn member's move names the new release
    /// at once for its turn in effect and its last drawn turn, each in a new generation.
    function setRelease(bytes calldata launchDigest, bytes32 configHash) external {
        Candidate storage registered = candidates[msg.sender];
        if (registered.adminKey == address(0)) revert NotRegistered();
        bytes32 release = _release(launchDigest, configHash);
        if (!releaseApproved[keccak256(abi.encode(msg.sender, release))]) revert ReleaseNotApproved(release);
        _settle();
        bool operator = msg.sender == current.candidate;
        if (pending.effectiveAt != 0 && (operator || msg.sender == pending.candidate)) revert ElectionPending();
        _leave(msg.sender, registered.release, release);
        registered.release = release;
        if (operator) {
            _leave(msg.sender, current.release, release);
            current.release = release;
        }
        emit CandidateReleaseChanged(msg.sender, release, launchDigest, configHash);
        if (listed[msg.sender] == 0) _enlist(msg.sender);
        if (address(schedule) != address(0)) schedule.releaseMoved(msg.sender);
    }

    /// @notice Names the deployment's vault, whose `lastCheckpointAt` the liveness clock reads.
    /// Set once, after both contracts deploy.
    function setVault(address vault_) external onlyGovernance {
        if (vault_.code.length == 0) revert ZeroAddress();
        if (address(vault) != address(0)) revert VaultAlreadySet(address(vault));
        vault = IVault(vault_);
    }

    /// @notice Names the schedule whose turns name the operator, from which on closes adopt rotation
    /// sets instead of electing. Set once, while rotation is off and the epoch is at least
    /// `MIN_SCHEDULED_EPOCH`, to a schedule built for this registry.
    function setSchedule(OperatorSchedule schedule_) external onlyGovernance {
        if (address(schedule) != address(0)) revert ScheduleAlreadySet(address(schedule));
        if (rotationSwitch) revert RotationActive();
        uint64 epochLength = parameters.epochLength;
        if (epochLength < MIN_SCHEDULED_EPOCH) revert EpochTooShort(epochLength, MIN_SCHEDULED_EPOCH);
        if (schedule_.registry() != this) revert InvalidSchedule();
        schedule = schedule_;
        emit ScheduleSet(address(schedule_));
    }

    /// @notice Approves, or withdraws approval of, `candidate` running the release with SEV-SNP launch
    /// measurement `launchDigest` and config SHA-256 `configHash`. Approving a registered candidate's
    /// release adds it to `candidateList` under the candidate cap; withdrawing approval removes it from
    /// the list while it stays registered. Approving another release lets the candidate move to it
    /// (`setRelease`); withdrawing it once the candidate has moved on stops a move back.
    function setReleaseApproval(address candidate, bytes calldata launchDigest, bytes32 configHash, bool approved)
        external
        onlyGovernance
    {
        bytes32 release = _release(launchDigest, configHash);
        releaseApproved[keccak256(abi.encode(candidate, release))] = approved;
        emit ReleaseApprovalChanged(candidate, release, approved);
        Candidate memory registered = candidates[candidate];
        // Listing follows the release the candidate holds, registered or moved to, not any other
        // release the timelock names for it.
        if (registered.adminKey == address(0) || registered.release != release) return;
        if (approved) {
            if (listed[candidate] == 0) _enlist(candidate);
        } else if (listed[candidate] != 0) {
            _delist(candidate);
        }
    }

    /// @notice Turns rotation on or off. Turning it off cancels an election not yet in effect, and
    /// its handover source with it: no successor hands over while rotation is off. Turns already
    /// drawn stand.
    function setRotation(bool enabled) external onlyGovernance {
        if (enabled == rotationSwitch) revert RotationUnchanged(enabled);
        _settle();
        bool cancelled = !enabled && pending.effectiveAt != 0;
        if (cancelled) {
            delete pending;
            delete source;
        }
        rotationSwitch = enabled;
        emit RotationChanged(enabled, cancelled);
    }

    /// @notice Sets the parameters, each within the bounds fixed at deployment, and, with a schedule
    /// named, an epoch of at least `MIN_SCHEDULED_EPOCH`. A lower candidate cap removes no listed
    /// candidate; it refuses new approvals, and approved re-registrations, until the count is under it.
    function setParameters(Parameters calldata parameters_) external onlyGovernance {
        _checkBounds(parameters_);
        if (address(schedule) != address(0) && parameters_.epochLength < MIN_SCHEDULED_EPOCH) {
            revert EpochTooShort(parameters_.epochLength, MIN_SCHEDULED_EPOCH);
        }
        parameters = parameters_;
        emit ParametersChanged(parameters_);
    }

    /// @notice Closes the latest election epoch that has ended, skipping any that ended unclosed, and
    /// adopts a rotation set or elects as the contract's rule says.
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
        if (!rotationSwitch) return epoch;
        if (address(schedule) != address(0)) {
            _adopt(epoch, p);
            return epoch;
        }
        if (pending.effectiveAt != 0) return epoch;
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
    /// time. Reverts when no standby qualifies, leaving the operator in effect unchanged, and with a
    /// schedule, whose turns name the operator.
    /// @return epoch The epoch number the promotion consumes, one more than any closed or consumed.
    function promote() external returns (uint64 epoch) {
        if (address(schedule) != address(0)) revert ScheduleNamesTurns();
        if (!rotationSwitch) revert RotationDisabled();
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

    /// @notice Installs `candidate` with `adminKey` as the operator in effect, running the release
    /// `launchDigest`/`configHash` names: the timelock-only fallback for a failed operator whose key
    /// is gone and no election can move, since an election needs rotation on (#502). Rotation must be
    /// off — while it is on, elections, promotions and turns move the role — so a pending election is
    /// already cancelled, and every drawn turn must have ended: the installation ends the last turn's
    /// naming. The installed operator takes effect at once; the engine
    /// mirrors this with its seven-day log notice before its key signs. No candidacy changes: the
    /// replaced operator keeps its release, approvals and listing, and the installed candidate,
    /// when registered, keeps its own and moves with `setRelease` like any operator in effect.
    function installOperator(address candidate, address adminKey, bytes calldata launchDigest, bytes32 configHash)
        external
        onlyGovernance
    {
        if (rotationSwitch) revert RotationActive();
        if (address(schedule) != address(0)) {
            uint64 until = schedule.turnsRunUntil();
            if (until > block.timestamp) revert TurnsPending(until);
        }
        if (candidate == address(0) || adminKey == address(0)) revert ZeroAddress();
        bytes32 release = _release(launchDigest, configHash);
        current = Operator(candidate, adminKey, release, uint64(block.timestamp));
        emit OperatorInstalled(candidate, adminKey, release);
    }

    /// @notice The operator in effect: the member of the schedule's turn while it names one, else
    /// the last election once its time has come, or before it the operator it replaces.
    function operatorInEffect() external view returns (Operator memory) {
        (Operator memory turn,,) = _turn();
        return turn.candidate != address(0) ? turn : _inEffect();
    }

    /// @notice The operator the vault names while rotation is on: the member of the schedule's turn
    /// from its boundary, while it names one; else a pending promotion from the moment it is made, a
    /// pending election from its effective time, else the operator in effect. A node's hostname and
    /// release checks read this, not `operatorInEffect`: the vault names a promoted standby's release
    /// at once, while the promoted key takes effect only after its notice.
    function namedOperator() external view returns (Operator memory) {
        return _named();
    }

    /// @notice Whether the vault follows this registry's operator: while the switch is on, and while
    /// a drawn turn names the operator after it turns off.
    function rotationEnabled() external view returns (bool) {
        (Operator memory turn,,) = _turn();
        return rotationSwitch || turn.candidate != address(0);
    }

    /// @notice The generation of the turn that names the operator, zero when no turn does.
    function operatorGeneration() external view returns (uint64 generation) {
        (, generation,) = _turn();
    }

    /// @notice What the vault reads in one call: `rotationEnabled`, `operatorRelease`, the
    /// `operatorGeneration`, and when the schedule's turn that started last ends, zero before any.
    function rotationState() external view returns (bool enabled, bytes32 release, uint64 generation, uint64 until) {
        Operator memory turn;
        (turn, generation, until) = _turn();
        enabled = rotationSwitch || turn.candidate != address(0);
        release = turn.candidate != address(0) ? turn.release : _elected().release;
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
    /// an election's release moves at its effective time, as #359 §10 decides; the operator in
    /// effect's `setRelease` moves it at once.
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

    /// @dev The operator the vault names while rotation is on: the turn's member while a turn names
    /// one, else as `_elected`.
    function _named() private view returns (Operator memory) {
        (Operator memory turn,,) = _turn();
        return turn.candidate != address(0) ? turn : _elected();
    }

    /// @dev The schedule's turn that started last, and when it ends: its member and generation while
    /// it names the operator, which it does until an installation after its end, zeros otherwise.
    function _turn() private view returns (Operator memory named, uint64 generation, uint64 until) {
        OperatorSchedule turns = schedule;
        if (address(turns) == address(0)) return (named, 0, 0);
        address candidate;
        uint64 since;
        (candidate, generation, since, until) = turns.namedAt(uint64(block.timestamp));
        if (candidate == address(0) || current.since >= until) return (named, 0, until);
        Candidate storage member = candidates[candidate];
        named = Operator(candidate, member.adminKey, member.release, since);
    }

    /// @dev Whether `candidate` holds a turn while turns still matter: a turn names the operator
    /// (`turnNamed`), or a drawn turn has not ended. After an installation past the last turn's end
    /// neither holds.
    function _holdsRunningTurn(address candidate, bool turnNamed) private view returns (bool) {
        OperatorSchedule turns = schedule;
        if (address(turns) == address(0)) return false;
        return (turnNamed || turns.turnsRunUntil() > block.timestamp) && turns.holdsTurn(candidate);
    }

    /// @dev The operator elections name: a pending promotion from the moment it is made, a pending
    /// election from its effective time, else the operator in effect.
    function _elected() private view returns (Operator memory) {
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

    /// @notice Whether `candidate` is registered and its release approved, which a drawn member needs.
    function isListed(address candidate) external view returns (bool) {
        return listed[candidate] != 0;
    }

    /// @notice Whether `candidate` would count in a close now: registered, its release approved, its
    /// self-bond at the minimum, and its votes at the eligibility share of total stake.
    function isEligible(address candidate) external view returns (bool) {
        return
            listed[candidate] != 0 && _eligible(candidate, staking.votes(candidate), staking.totalStaked(), parameters);
    }

    /// @dev The eligible candidate with the most votes, the earlier registration on a tie, when the
    /// role moves to it: it is not the operator in effect, and a quorum of total stake is delegated
    /// to approved candidates. The zero address otherwise.
    function _winner(Parameters memory p) private view returns (address winner) {
        uint256 total = staking.totalStaked();
        uint256 delegated = 0;
        uint256 best = 0;
        uint64 earliest = 0;
        for (uint256 i = 0; i < candidateList.length; ++i) {
            address candidate = candidateList[i];
            uint256 cast = staking.votes(candidate);
            delegated += cast;
            if (!_eligible(candidate, cast, total, p)) continue;
            uint64 registration = candidates[candidate].registration;
            if (winner == address(0) || cast > best || (cast == best && registration < earliest)) {
                (winner, best, earliest) = (candidate, cast, registration);
            }
        }
        if (winner == current.candidate || delegated * BPS < uint256(p.quorumBps) * total) return address(0);
    }

    /// @dev Adopts the rotation set the close of `epoch` makes in the schedule, to take effect seven
    /// days plus the finality margin from now: every eligible listed candidate with its admin key
    /// and votes, and whether a quorum of total stake is delegated to listed candidates.
    function _adopt(uint64 epoch, Parameters memory p) private {
        uint256 total = staking.totalStaked();
        uint256 delegated = 0;
        uint256 count = 0;
        uint256 listedCount = candidateList.length;
        address[] memory eligible = new address[](listedCount);
        address[] memory keys = new address[](listedCount);
        uint256[] memory votes = new uint256[](listedCount);
        for (uint256 i = 0; i < listedCount; ++i) {
            address candidate = candidateList[i];
            uint256 cast = staking.votes(candidate);
            delegated += cast;
            if (!_eligible(candidate, cast, total, p)) continue;
            (eligible[count], keys[count], votes[count]) = (candidate, candidates[candidate].adminKey, cast);
            ++count;
        }
        assembly ("memory-safe") {
            mstore(eligible, count)
            mstore(keys, count)
            mstore(votes, count)
        }
        schedule.adopt(
            epoch,
            uint64(block.timestamp + NOTICE + p.finalityMargin),
            eligible,
            keys,
            votes,
            delegated * BPS >= uint256(p.quorumBps) * total
        );
    }

    /// @dev The eligible candidate with the most votes, the earlier registration on a tie, other
    /// than the operator in effect: the standby the liveness rule promotes. Unlike `_winner`, no
    /// quorum: the incumbent has failed, so any eligible standby serves.
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
            p.epochLength < lo.epochLength || p.epochLength > hi.epochLength || p.finalityMargin < lo.finalityMargin
                || p.finalityMargin > hi.finalityMargin || p.eligibilityBps < lo.eligibilityBps
                || p.eligibilityBps > hi.eligibilityBps || p.quorumBps < lo.quorumBps || p.quorumBps > hi.quorumBps
                || p.maxCandidates < lo.maxCandidates || p.maxCandidates > hi.maxCandidates
                || p.minSelfBond < lo.minSelfBond || p.minSelfBond > hi.minSelfBond
                || p.livenessTimeout < lo.livenessTimeout || p.livenessTimeout > hi.livenessTimeout
        ) revert ParameterOutOfBounds();
    }

    /// @dev Adds registered `candidate` to `candidateList`, refusing a full approved cap. Only
    /// approved candidates count toward the cap, so unapproved registrations cannot fill it. The
    /// operator in effect may take one place beyond it, so a full list never blocks its listing,
    /// and with it its upgrade — unless an operator an installation replaced took that place
    /// first: the list stays bounded at the cap plus one, and the timelock frees a place by
    /// withdrawing an approval or raising the cap. The exemption reads the pending-or-current
    /// operator, not the settled `current`: an election past its time that nobody settled yet has
    /// already moved the operator in effect, and entry points that enlist do not settle first.
    function _enlist(address candidate) private {
        Parameters memory p = parameters;
        uint256 places = uint256(p.maxCandidates) + (candidate == _named().candidate ? 1 : 0);
        if (candidateList.length >= places) revert CandidateCapReached(p.maxCandidates);
        candidateList.push(candidate);
        listed[candidate] = candidateList.length;
    }

    /// @dev Withdraws `candidate`'s approval of `left`, a release its move to `release` leaves,
    /// reporting the withdrawal when there was an approval to withdraw.
    function _leave(address candidate, bytes32 left, bytes32 release) private {
        bytes32 key = keccak256(abi.encode(candidate, left));
        if (left == release || !releaseApproved[key]) return;
        delete releaseApproved[key];
        emit ReleaseApprovalChanged(candidate, left, false);
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
