// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";
import {EIP712} from "@openzeppelin-contracts/utils/cryptography/EIP712.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";
import {Math} from "@openzeppelin-contracts/utils/math/Math.sol";

import {ExitVerifier} from "./ExitVerifier.sol";
import {MonkeyToken} from "./MonkeyToken.sol";
import {OperatorRegistry} from "./OperatorRegistry.sol";
import {StateTree} from "./StateTree.sol";

/// @title Vault
/// @notice Holds the engine's USDC collateral. The chain handles custody only: the engine credits
/// each deposit event once it is final, keyed by transaction hash and log index.
/// @notice A trading account must be able to sign the engine's EIP-712 requests with its own key:
/// the engine accepts no contract signatures. A contract wallet funds a key that signs for it
/// through `depositFor`; a balance credited to a contract account can be recovered only through
/// the exit (#48).
/// @notice MONKEY emissions: the vault deploys the MONKEY token as its only minter, and mints to an
/// account what its cumulative entitlement, proven against the latest posted checkpoint root, adds
/// to what it was already minted.
/// @notice Distributions: each epoch the engine sets aside house NAV above the liquidity floor for
/// stakers; the vault pays an account the USDC its cumulative distribution, proven against the
/// latest posted checkpoint root, adds to what it was already paid.
/// @notice Exit: once no checkpoint has been posted for `exitPeriod`, anyone may start the exit,
/// which permanently ends signed withdrawals, checkpoints, deposits and distribution claims. For
/// `claimWindow`, accounts register what the last posted root proves them owed: their equity at its
/// marks, their unpaid approvals and queued requests, and, ranked behind those, their unpaid
/// distributions. Then each class is paid the same fraction of what the vault holds beyond the
/// refunds it owes and the held principal it keeps, and the timelock may release the rest. Deposits
/// the root does not hold, or holds pending, are refunded to their payers. Neither pause nor the
/// signer epoch applies to the exit.
/// @dev The attested signer authorizes checkpoints, withdrawals, and distribution claims; the
/// deployment's multisig controls signer rotation and pauses through its timelock, and as the
/// guardian may pause withdrawals for a week or retire the signer without the timelock's delay;
/// the timelock also names the enclave release that holds the signer's key. This contract is not
/// upgradeable.
contract Vault is ReentrancyGuardTransient, EIP712 {
    using SafeERC20 for IERC20;

    struct Withdrawal {
        address account;
        uint256 amount;
        uint64 nonce;
        uint64 approvedSeq;
        uint64 signerEpoch;
        uint64 issuedAt;
        uint64 expiresAt;
    }

    bytes32 public constant WITHDRAWAL_TYPEHASH = keccak256(
        "Withdrawal(address account,uint256 amount,uint64 nonce,uint64 approvedSeq,uint64 signerEpoch,uint64 issuedAt,uint64 expiresAt)"
    );
    uint256 public constant WITHDRAWAL_LIFETIME = 86400;

    /// @notice How long a guardian pause holds withdrawals; the timelock renews it by pausing them
    /// itself.
    uint256 public constant GUARDIAN_PAUSE = 7 days;

    /// @notice Smallest single deposit in USDC base units: 10 USDC. Every deposit becomes a
    /// permanent entry in the engine's state, so a floor makes each one cost locked capital, not
    /// only gas.
    uint256 public constant MIN_DEPOSIT = 1e7;

    /// @notice Largest single deposit in USDC base units: 1e9 USDC, the engine's input bound.
    uint256 public constant MAX_DEPOSIT = 1e15;

    /// @notice The most deposit ids below the exit cursor the terms may name as gaps, which bounds
    /// the gas the exit's refund accounting spends on them; the engine names the lowest.
    uint256 public constant MAX_EXIT_GAPS = 64;

    /// @notice Emission periods between halvings, as the engine counts them.
    uint256 public constant PERIODS_PER_HALVING = 26_280;

    /// @notice The largest initial period budget the engine accepts: the one whose nominal maximum
    /// supply fits a `uint128`.
    uint256 public constant MAX_PERIOD_BUDGET = type(uint128).max / (2 * PERIODS_PER_HALVING);

    /// @notice The first byte of an emission entitlement leaf's key in the engine's state tree.
    uint8 private constant ENTITLEMENT_LEAF = 8;

    /// @notice The first byte of a withdrawal approval leaf's key in the engine's state tree.
    uint8 private constant APPROVAL_LEAF = 32;
    /// @notice The first byte of a distribution leaf's key in the engine's state tree.
    uint8 private constant DISTRIBUTION_LEAF = 13;

    /// @notice One USDC base unit in the engine's nine-decimal raw units.
    int256 private constant USDC_UNIT = 1e3;

    /// @notice A deposit's funding caller and the USDC base units it brought in; a booked surplus
    /// has none.
    struct DepositRecord {
        address payer;
        uint96 amount;
    }

    /// @notice The collateral token: USDC, 6 decimals.
    IERC20 public immutable usdc;

    /// @notice MONKEY, which this vault alone mints, capped at the schedule's nominal maximum supply.
    MonkeyToken public immutable monkey;

    /// @notice MONKEY base units minted to each account so far against its entitlement.
    mapping(address account => uint256 claimed) public emissionsClaimed;

    /// @notice USDC base units paid to each account so far against its distribution.
    mapping(address account => uint256 claimed) public distributionsClaimed;

    /// @notice USDC base units paid out in distributions so far; the node's checkpoint
    /// reconciliation adds it to the vault's balance against the engine's ledger.
    uint256 public distributionsPaid;

    /// @notice Deposits accepted so far, trader and house alike; the next deposit's id.
    uint256 public depositCount;
    uint256 public totalWithdrawn;
    /// @notice USDC base units received across every deposit, house deposits and booked surpluses
    /// included.
    uint256 public totalDeposited;
    /// @notice `totalDeposited` just after deposit `id`, so a ledger that has credited deposits
    /// `0..=id` can be compared with exactly those deposits.
    mapping(uint256 id => uint256 total) public depositedThrough;

    /// @notice Authorized enclave address, provisioned after verifying its attestation.
    address public checkpointSigner;
    uint64 public signerEpoch = 1;
    /// @notice Historical signer identity for replay; custody still requires the current signer.
    mapping(uint64 epoch => address signer) public signerAtEpoch;
    /// @notice The enclave release that holds the signer's key: `keccak256` of its 48-byte SEV-SNP
    /// launch measurement and the SHA-256 of the config file it runs with. A node opens its sealed
    /// keys only while it is this release, and hands them only to this release, so no code or config
    /// receives the key without the timelock delay, and setting another release supersedes this one.
    /// While the operator registry has rotation on, this is instead the pending-or-current
    /// operator's release, which `syncRelease` folds into this contract's state.
    bytes32 public namedRelease;
    /// @notice Raised by every `setCurrentRelease`, and by `syncRelease` when the effective release
    /// changes on the registry's path: a node seals its keys under the generation it runs in and
    /// opens only a copy sealed under the current one, so a release set current again takes the
    /// keys afresh from the running release instead of reopening an old copy.
    uint64 public releaseGeneration;
    /// @notice The effective release the generation last synced at; the first `syncRelease` after an
    /// election or promotion takes effect bumps the generation when it differs. A node reads this
    /// beside `currentRelease()` and refuses to seal keys until the two agree, so a key service
    /// never seals under a generation a later sync would supersede.
    bytes32 public syncedRelease;
    /// @notice The operator registry this vault follows while rotation is on, set once by
    /// governance; the zero address behaves as rotation off.
    OperatorRegistry public registry;
    TimelockController public immutable governance;
    /// @notice The deployment's multisig, which may pause withdrawals and retire the signer without
    /// the timelock's delay, and nothing else.
    address public immutable guardian;
    bool public depositsPaused;
    /// @notice Unix seconds from which withdrawals are no longer paused: `type(uint256).max` while
    /// the timelock pauses them, a guardian pause's end, or zero.
    uint256 public withdrawalsPausedUntil;
    /// @notice Guardian pauses so far.
    uint256 public guardianPauses;
    bool public hasCheckpoint;
    uint64 public latestSeq;
    bytes32 public latestRoot;
    mapping(uint64 seq => bytes32 root) public checkpointRoots;
    mapping(uint64 seq => bool accepted) public hasCheckpointAt;
    mapping(uint64 seq => address signer) public checkpointSignerAt;
    mapping(uint64 seq => uint64 epoch) public checkpointEpochAt;
    mapping(address account => mapping(uint64 nonce => bool paid)) public usedNonces;

    /// @notice Seconds without a posted checkpoint after which anyone may start the exit.
    uint64 public immutable exitPeriod;
    /// @notice Seconds from the exit's start during which claims register.
    uint64 public immutable claimWindow;
    /// @notice Seconds without a posted checkpoint after which deposits are refused.
    uint64 public immutable staleAfter;
    /// @notice The stateless checker of the leaves the exit pays against.
    ExitVerifier public immutable exitVerifier;
    /// @notice When the latest checkpoint was posted, or, before the first, when the vault was
    /// deployed.
    uint64 public lastCheckpointAt;
    /// @notice Each deposit's payer and amount, by id, for the exit's refunds.
    mapping(uint256 id => DepositRecord record) public depositRecords;
    /// @notice USDC base units every payer-funded deposit brought in so far, booked surpluses aside.
    uint256 public totalRefundable;
    /// @notice `totalRefundable` just after deposit `id`.
    mapping(uint256 id => uint256 total) public refundableThrough;

    /// @notice When the exit started, or zero while it has not.
    uint64 public exitStartedAt;
    /// @notice One past the highest deposit id the exit root holds; every deposit from it on is
    /// refunded, but for those `exitRefundableGap` marks.
    uint64 public exitDepositCursor;
    /// @notice The deposit ids below the cursor the exit root does not hold, which it refunds.
    mapping(uint256 id => bool refundable) public exitRefundableGap;
    /// @notice USDC base units the exit still owes as refunds of deposits the root does not hold, or
    /// holds pending.
    uint256 public exitRefundReserve;
    /// @notice USDC base units of held principal the exit never pays.
    uint256 public exitHeldReserve;
    /// @notice Each account's registered claim on its equity, approvals and queued requests.
    mapping(address account => uint256 claim) public exitClaims;
    uint256 public totalExitClaims;
    /// @notice Each account's registered claim on its unpaid distribution, ranked behind `exitClaims`.
    mapping(address account => uint256 claim) public exitDistributionClaims;
    uint256 public totalExitDistributionClaims;
    mapping(address account => bool registered) public exitAccountRegistered;
    mapping(address account => bool registered) public exitDistributionRegistered;
    mapping(uint64 requestedSeq => bool registered) public exitQueuedRegistered;
    mapping(uint256 id => bool refunded) public depositRefunded;
    /// @notice Whether the pools below are fixed, which the first payout after the claim window does.
    bool public exitSettled;
    /// @notice USDC base units shared by `exitClaims`, in proportion to each claim.
    uint256 public exitClientPool;
    /// @notice USDC base units shared by `exitDistributionClaims`, in proportion to each claim.
    uint256 public exitDistributionPool;
    /// @notice USDC base units left once claims, refunds and held principal are set aside, which the
    /// timelock may release.
    uint256 public houseRemainder;

    event CheckpointPosted(uint64 indexed seq, bytes32 root, bytes signature);
    event WithdrawalPaid(address indexed account, uint256 amount, uint64 indexed nonce, uint64 approvedSeq);
    event SignerChanged(address indexed previousSigner, address indexed signer, uint64 signerEpoch);
    event PausesChanged(bool depositsPaused, bool withdrawalsPaused);
    /// @notice The guardian paused withdrawals until `until`, its `pauses`-th pause.
    event GuardianPausedWithdrawals(uint256 until, uint256 pauses);
    /// @notice The guardian retired `signer`, the signer of epoch `signerEpoch`.
    event SignerRetired(address indexed signer, uint64 signerEpoch);
    event CurrentReleaseChanged(bytes32 indexed release, uint64 generation, bytes launchDigest, bytes32 configHash);
    /// @notice Governance named the operator registry the vault follows while rotation is on.
    event RegistrySet(address indexed registry);
    /// @notice The registry's path changed the effective release, raising the generation.
    event ReleaseSynced(bytes32 indexed release, uint64 generation);

    /// @notice `amount` USDC base units deposited to trader account `account` by `from`, the caller
    /// whose tokens the vault pulled.
    event Deposited(uint256 indexed id, address indexed account, address indexed from, uint256 amount);

    /// @notice `amount` USDC base units added to house capital by `from`.
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);

    /// @notice Existing vault holdings `amount` beyond recorded deposits booked as house capital.
    /// Unlike HouseDeposited, a surplus books no funding address: whoever calls is not a payer.
    event SurplusBooked(uint256 indexed id, uint256 amount);

    /// @notice `amount` MONKEY base units minted to `account`, bringing its total to `entitled`, the
    /// entitlement checkpoint `seq` proves.
    event EmissionsClaimed(address indexed account, uint256 amount, uint256 entitled, uint64 seq);

    /// @notice `amount` USDC base units paid to `account`, bringing its total to `entitled`, the
    /// distribution checkpoint `seq` proves.
    event DistributionClaimed(address indexed account, uint256 amount, uint256 entitled, uint64 seq);

    /// @notice The exit started against checkpoint `seq`'s `root`, owing `refundReserve` in refunds
    /// of deposits from `depositCursor` on or pending in the root, and keeping `heldReserve`.
    event ExitStarted(
        uint64 indexed seq, bytes32 root, uint64 depositCursor, uint256 refundReserve, uint256 heldReserve
    );
    /// @notice `amount` USDC base units added to `account`'s exit claim.
    event ExitClaimRegistered(address indexed account, uint256 amount);
    /// @notice `amount` USDC base units of unpaid distribution registered for `account`.
    event ExitDistributionRegistered(address indexed account, uint256 amount);
    /// @notice The pools fixed once the claim window closed.
    event ExitSettled(uint256 clientPool, uint256 distributionPool, uint256 houseRemainder);
    event ExitPaid(address indexed account, uint256 amount);
    event DepositRefunded(uint256 indexed id, address indexed payer, uint256 amount);
    event HouseRemainderReleased(address indexed to, uint256 amount);

    error ZeroAddress();
    error InvalidAmount(uint256 amount);
    error CheckpointNotExtended();
    error InvalidCheckpointSigner();
    error CheckpointParentMismatch();
    error UnsupportedChain();
    error InvalidGovernance();
    error UnauthorizedGovernance();
    error UnauthorizedGuardian();
    error NoSigner();
    error DepositsPaused();
    error WithdrawalsPaused();
    error InvalidWithdrawal();
    error InvalidWithdrawalTime();
    error InvalidSignerEpoch();
    error WithdrawalAlreadyPaid();
    error UnknownCheckpoint();
    error InvalidApprovalProof();
    error InvalidWithdrawalSigner();
    error InexactWithdrawalTransfer();
    error NoSurplus(uint256 surplus);
    error InvalidPeriodBudget(uint256 periodBudget);
    error NoCheckpoint();
    error InvalidEntitlementProof();
    error NothingToClaim(uint256 entitled, uint256 claimed);
    error InvalidLaunchDigest();
    error InvalidExitSchedule();
    error CheckpointsStale();
    error ExitActive();
    error ExitNotDue(uint256 dueAt);
    error ExitNotStarted();
    error ClaimWindowClosed();
    error ClaimWindowOpen(uint256 closesAt);
    error InvalidExitProof();
    error AlreadyRegistered();
    error NoExitClaim();
    error NoHouseRemainder();
    error NotRefundable(uint256 id);
    error PendingDepositMismatch();
    error RegistryAlreadySet(address registry);
    error RotationActive();
    error NotTheRegisteredEnclave(address signer, address enclave);

    /// @param governance_ The deployment's timelock, which alone rotates the signer, sets the
    /// release that holds its key, and unpauses.
    /// @param guardian_ The deployment's multisig, which may also pause withdrawals and retire the
    /// signer at once.
    /// @param periodBudget_ The engine's initial emission period budget in MONKEY base units, which
    /// sets the token's cap at `2 * PERIODS_PER_HALVING * periodBudget_`.
    /// @param exitPeriod_ Seconds without a checkpoint before the exit may start; longer than the
    /// timelock's delay, so governance can install a new signer first.
    /// @param claimWindow_ Seconds during which exit claims register.
    /// @param staleAfter_ Seconds without a checkpoint before deposits are refused, at most
    /// `exitPeriod_`.
    constructor(
        IERC20 usdc_,
        address checkpointSigner_,
        TimelockController governance_,
        address guardian_,
        uint256 periodBudget_,
        uint64 exitPeriod_,
        uint64 claimWindow_,
        uint64 staleAfter_
    ) EIP712("EngineWithdrawal", "1") {
        if (address(usdc_) == address(0) || checkpointSigner_ == address(0) || guardian_ == address(0)) {
            revert ZeroAddress();
        }
        if (block.chainid > type(uint64).max) revert UnsupportedChain();
        if (address(governance_).code.length == 0 || governance_.getMinDelay() == 0) revert InvalidGovernance();
        if (periodBudget_ == 0 || periodBudget_ > MAX_PERIOD_BUDGET) revert InvalidPeriodBudget(periodBudget_);
        if (
            exitPeriod_ <= governance_.getMinDelay() || claimWindow_ == 0 || staleAfter_ == 0
                || staleAfter_ > exitPeriod_
        ) revert InvalidExitSchedule();
        usdc = usdc_;
        checkpointSigner = checkpointSigner_;
        signerAtEpoch[signerEpoch] = checkpointSigner_;
        governance = governance_;
        guardian = guardian_;
        monkey = new MonkeyToken(2 * PERIODS_PER_HALVING * periodBudget_);
        exitPeriod = exitPeriod_;
        claimWindow = claimWindow_;
        staleAfter = staleAfter_;
        exitVerifier = new ExitVerifier();
        lastCheckpointAt = uint64(block.timestamp);
    }

    modifier onlyGovernance() {
        if (msg.sender != address(governance)) revert UnauthorizedGovernance();
        _;
    }

    modifier onlyGuardian() {
        if (msg.sender != guardian) revert UnauthorizedGuardian();
        _;
    }

    /// @notice Retires existing withdrawal authorizations, including when reusing the signer address.
    /// While the registry has rotation on, only the enclave address the operator in effect
    /// registered installs, so a promotion or election's standby is the only new signer.
    function setSigner(address signer) external onlyGovernance {
        if (signer == address(0)) revert ZeroAddress();
        OperatorRegistry reg = registry;
        if (address(reg) != address(0) && reg.rotationEnabled()) {
            address enclave = reg.operatorEnclave();
            if (signer != enclave) revert NotTheRegisteredEnclave(signer, enclave);
        }
        address previous = checkpointSigner;
        checkpointSigner = signer;
        signerEpoch++;
        signerAtEpoch[signerEpoch] = signer;
        emit SignerChanged(previous, signer, signerEpoch);
    }

    /// @notice Names the operator registry the vault follows while rotation is on. Set once, after
    /// both contracts deploy.
    function setRegistry(address registry_) external onlyGovernance {
        if (registry_.code.length == 0) revert ZeroAddress();
        if (address(registry) != address(0)) revert RegistryAlreadySet(address(registry));
        registry = OperatorRegistry(registry_);
        emit RegistrySet(registry_);
    }

    /// @notice The release that holds the enclave's keys: what governance named while rotation is
    /// off, or the pending-or-current operator's while the registry has rotation on.
    function currentRelease() public view returns (bytes32) {
        OperatorRegistry reg = registry;
        if (address(reg) != address(0) && reg.rotationEnabled()) return reg.operatorRelease();
        return namedRelease;
    }

    /// @notice Folds a registry-driven release change into this contract's state, raising the
    /// release generation: anyone calls it once an election or promotion moves the operator. A
    /// standby seals its keys only after this, so they are sealed under the generation the release
    /// change raised. Every state-changing entry point syncs first, so the generation a key
    /// service observes can never lag the release it names for longer than one read: once the
    /// operator acts, identity and generation move together.
    function syncRelease() public {
        bytes32 effective = currentRelease();
        if (effective == syncedRelease) return;
        syncedRelease = effective;
        uint64 generation = ++releaseGeneration;
        emit ReleaseSynced(effective, generation);
    }

    /// @notice Makes the release with SEV-SNP launch measurement `launchDigest`, running the config
    /// whose SHA-256 is `configHash`, the one that holds the enclave's keys, in a new generation: the
    /// running release hands them to it, and no copy sealed in an earlier generation opens. Applies
    /// only while rotation is off; while it is on, the registry's operator names the release.
    function setCurrentRelease(bytes calldata launchDigest, bytes32 configHash) external onlyGovernance {
        if (launchDigest.length != 48) revert InvalidLaunchDigest();
        OperatorRegistry reg = registry;
        if (address(reg) != address(0) && reg.rotationEnabled()) revert RotationActive();
        bytes32 release = keccak256(abi.encodePacked(launchDigest, configHash));
        namedRelease = release;
        syncedRelease = release;
        uint64 generation = ++releaseGeneration;
        emit CurrentReleaseChanged(release, generation, launchDigest, configHash);
    }

    /// @notice Independently pauses deposits (including house capital) and withdrawal claims. A
    /// withdrawal pause holds until the timelock lifts it, which also ends a guardian pause.
    function setPauses(bool depositsPaused_, bool withdrawalsPaused_) external onlyGovernance {
        depositsPaused = depositsPaused_;
        withdrawalsPausedUntil = withdrawalsPaused_ ? type(uint256).max : 0;
        emit PausesChanged(depositsPaused_, withdrawalsPaused_);
    }

    /// @notice Pauses withdrawal and distribution claims for `GUARDIAN_PAUSE` from now. A pause in
    /// force cannot be extended this way; only the timelock holds one longer.
    function pauseWithdrawals() external onlyGuardian {
        if (withdrawalsPaused()) revert WithdrawalsPaused();
        uint256 until = block.timestamp + GUARDIAN_PAUSE;
        withdrawalsPausedUntil = until;
        emit GuardianPausedWithdrawals(until, ++guardianPauses);
    }

    /// @notice Clears the signer, so neither a withdrawal signed in the current epoch nor any
    /// checkpoint is accepted again; installing a signer through the timelock starts the next epoch.
    function retireSigner() external onlyGuardian {
        address retired = checkpointSigner;
        if (retired == address(0)) revert NoSigner();
        checkpointSigner = address(0);
        emit SignerRetired(retired, signerEpoch);
    }

    /// @notice Whether withdrawal and distribution claims are paused, by the timelock or the guardian.
    function withdrawalsPaused() public view returns (bool) {
        return block.timestamp < withdrawalsPausedUntil;
    }

    /// @notice Anyone may relay an enclave checkpoint. Only increasing seqs with the exact signed parent can
    /// commit, fencing concurrent instances and log rollback. A successful receipt must precede withdrawal signing.
    function postCheckpoint(
        uint64 seq,
        bytes32 root,
        bool hasParent,
        uint64 parentSeq,
        bytes32 parentRoot,
        bytes calldata signature
    ) external {
        if (exitStartedAt != 0) revert ExitActive();
        if (hasCheckpoint && seq <= latestSeq) revert CheckpointNotExtended();
        if (hasParent != hasCheckpoint || parentSeq != latestSeq || parentRoot != latestRoot) {
            revert CheckpointParentMismatch();
        }
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(this),
                seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        if (ECDSA.recover(digest, signature) != checkpointSigner) revert InvalidCheckpointSigner();
        hasCheckpoint = true;
        latestSeq = seq;
        latestRoot = root;
        checkpointRoots[seq] = root;
        hasCheckpointAt[seq] = true;
        checkpointSignerAt[seq] = checkpointSigner;
        checkpointEpochAt[seq] = signerEpoch;
        lastCheckpointAt = uint64(block.timestamp);
        emit CheckpointPosted(seq, root, signature);
    }

    /// @notice Mints to `account` the MONKEY its cumulative entitlement `entitled` adds to what it
    /// was already minted, once `siblings` prove that entitlement against the latest posted root.
    /// Anyone may relay a claim; the tokens always go to `account`. A proof against an earlier root
    /// stops working once a later checkpoint is posted; fetch a fresh one.
    /// @return amount The base units minted.
    function claimEmissions(address account, uint256 entitled, bytes32[] calldata siblings)
        external
        nonReentrant
        returns (uint256 amount)
    {
        if (!hasCheckpoint) revert NoCheckpoint();
        bytes32 key = keccak256(abi.encodePacked(ENTITLEMENT_LEAF, account));
        if (!_proves(latestRoot, key, keccak256(abi.encode(entitled)), siblings)) revert InvalidEntitlementProof();
        uint256 claimed = emissionsClaimed[account];
        if (entitled <= claimed) revert NothingToClaim(entitled, claimed);
        amount = entitled - claimed;
        emissionsClaimed[account] = entitled;
        emit EmissionsClaimed(account, amount, entitled, latestSeq);
        monkey.mint(account, amount);
    }

    /// @notice Pays a signed approval included in an accepted checkpoint, once per account and nonce.
    function withdraw(
        Withdrawal calldata withdrawal,
        uint64 checkpointSeq,
        bytes32[] calldata siblings,
        bytes calldata signature
    ) external nonReentrant {
        if (exitStartedAt != 0) revert ExitActive();
        if (withdrawalsPaused()) revert WithdrawalsPaused();
        if (withdrawal.account == address(0) || withdrawal.account == address(this) || withdrawal.amount == 0) {
            revert InvalidWithdrawal();
        }
        if (usedNonces[withdrawal.account][withdrawal.nonce]) revert WithdrawalAlreadyPaid();
        if (withdrawal.signerEpoch != signerEpoch) revert InvalidSignerEpoch();
        if (
            uint256(withdrawal.expiresAt) != uint256(withdrawal.issuedAt) + WITHDRAWAL_LIFETIME
                || withdrawal.issuedAt > block.timestamp || withdrawal.expiresAt <= block.timestamp
        ) revert InvalidWithdrawalTime();
        if (!hasCheckpointAt[checkpointSeq] || checkpointSeq < withdrawal.approvedSeq) revert UnknownCheckpoint();
        if (!_verifyApproval(withdrawal, checkpointRoots[checkpointSeq], siblings)) revert InvalidApprovalProof();
        if (ECDSA.recover(withdrawalDigest(withdrawal), signature) != checkpointSigner) {
            revert InvalidWithdrawalSigner();
        }
        usedNonces[withdrawal.account][withdrawal.nonce] = true;
        totalWithdrawn += withdrawal.amount;
        _pay(withdrawal.account, withdrawal.amount);
        emit WithdrawalPaid(withdrawal.account, withdrawal.amount, withdrawal.nonce, withdrawal.approvedSeq);
    }

    /// @notice The EIP-712 digest signed by the currently attested enclave.
    function withdrawalDigest(Withdrawal calldata withdrawal) public view returns (bytes32) {
        return _hashTypedDataV4(
            keccak256(
                abi.encode(
                    WITHDRAWAL_TYPEHASH,
                    withdrawal.account,
                    withdrawal.amount,
                    withdrawal.nonce,
                    withdrawal.approvedSeq,
                    withdrawal.signerEpoch,
                    withdrawal.issuedAt,
                    withdrawal.expiresAt
                )
            )
        );
    }

    /// @dev Whether `siblings` prove `withdrawal`'s approval leaf, keyed by account and nonce, against
    /// `root`.
    function _verifyApproval(Withdrawal calldata withdrawal, bytes32 root, bytes32[] calldata siblings)
        private
        pure
        returns (bool)
    {
        bytes32 key = keccak256(abi.encodePacked(APPROVAL_LEAF, withdrawal.account, withdrawal.nonce));
        bytes32 valueHash =
            keccak256(abi.encode(withdrawal.account, withdrawal.amount, withdrawal.nonce, withdrawal.approvedSeq));
        return _proves(root, key, valueHash, siblings);
    }

    function _pay(address account, uint256 amount) private {
        if (withdrawalsPaused()) revert WithdrawalsPaused();
        _transferExact(account, amount);
    }

    /// @dev Transfers exactly `amount` to `account`, reverting on any other debit or credit.
    function _transferExact(address account, uint256 amount) private {
        uint256 vaultBefore = usdc.balanceOf(address(this));
        uint256 accountBefore = usdc.balanceOf(account);
        usdc.safeTransfer(account, amount);
        uint256 vaultAfter = usdc.balanceOf(address(this));
        uint256 accountAfter = usdc.balanceOf(account);
        if (
            vaultAfter > vaultBefore || vaultBefore - vaultAfter != amount || accountAfter < accountBefore
                || accountAfter - accountBefore != amount
        ) revert InexactWithdrawalTransfer();
    }

    /// @notice Pays `account` the USDC its cumulative distribution `entitled` adds to what it was
    /// already paid, once `siblings` prove that distribution against the latest posted root. Anyone
    /// may relay a claim; the USDC always goes to `account`. A proof against an earlier root stops
    /// working once a later checkpoint is posted; fetch a fresh one.
    /// @return amount The USDC base units paid.
    function claimDistribution(address account, uint256 entitled, bytes32[] calldata siblings)
        external
        nonReentrant
        returns (uint256 amount)
    {
        if (exitStartedAt != 0) revert ExitActive();
        if (!hasCheckpoint) revert NoCheckpoint();
        bytes32 key = keccak256(abi.encodePacked(DISTRIBUTION_LEAF, account));
        if (!_proves(latestRoot, key, keccak256(abi.encode(entitled)), siblings)) revert InvalidEntitlementProof();
        uint256 claimed = distributionsClaimed[account];
        if (entitled <= claimed) revert NothingToClaim(entitled, claimed);
        amount = entitled - claimed;
        distributionsClaimed[account] = entitled;
        distributionsPaid += amount;
        emit DistributionClaimed(account, amount, entitled, latestSeq);
        _pay(account, amount);
    }

    /// @notice Whether no checkpoint has been posted for `staleAfter`, so deposits are refused.
    function checkpointsStale() public view returns (bool) {
        return block.timestamp >= uint256(lastCheckpointAt) + staleAfter;
    }

    /// @notice Starts the exit against the latest posted root, once no checkpoint has been posted for
    /// `exitPeriod`; anyone may call it, and it is permanent. `terms` and `siblings` prove the root's
    /// exit terms leaf; with no checkpoint ever posted, they are ignored and every deposit is
    /// refunded. The ids below the cursor the root does not hold are as few as a reorganisation the
    /// engine was still applying leaves, at most 64.
    function startExit(ExitVerifier.ExitTerms calldata terms, bytes32[] calldata siblings) external {
        if (exitStartedAt != 0) revert ExitActive();
        uint256 dueAt = uint256(lastCheckpointAt) + exitPeriod;
        if (block.timestamp < dueAt) revert ExitNotDue(dueAt);
        uint64 cursor;
        uint256 pending;
        uint256 held;
        uint256 unheld;
        if (hasCheckpoint) {
            if (!exitVerifier.provesTerms(latestRoot, terms, siblings)) revert InvalidExitProof();
            // A root never holds more deposits than the vault received; were it to, refund none past it.
            cursor = terms.depositCursor < depositCount ? terms.depositCursor : uint64(depositCount);
            pending = _usdcCeil(terms.pending);
            held = _usdcCeil(terms.held);
            // The named gaps reserve the canonical principal of the vault's own records at their
            // ids, and `unheld` the principal of the unnamed ids below the cursor, at the last
            // amounts the engine saw: whatever no gap names stays in the vault rather than reaching
            // the pools.
            unheld = _usdcCeil(terms.unheld);
        }
        if (hasCheckpoint && terms.depositBase > cursor) revert InvalidExitProof();
        uint256 refunds = totalRefundable - (cursor == 0 ? 0 : refundableThrough[cursor - 1]);
        uint256 named;
        // The span the exit attributes runs from the finalised frontier to the cursor: below the
        // frontier every deposit is settled, and attributes settled deposits no term names.
        uint256 below = (cursor == 0 ? 0 : refundableThrough[cursor - 1])
            - (terms.depositBase == 0 ? 0 : refundableThrough[terms.depositBase - 1]);
        if (hasCheckpoint) named = _markRefundableGaps(cursor, terms.gaps);
        // Every refundable deposit below the cursor is one the root holds for a trader, a named
        // gap, or an unnamed one: attribute them and reserve the greater of the engine's estimate
        // and the canonical remainder, so no unnamed deposit's principal reaches the pools.
        uint256 attributed = _usdcCeil(terms.heldReceipts) + named;
        uint256 canonicalUnnamed = below > attributed ? below - attributed : 0;
        exitStartedAt = uint64(block.timestamp);
        exitDepositCursor = cursor;
        exitRefundReserve = refunds + pending + named + (unheld > canonicalUnnamed ? unheld : canonicalUnnamed);
        exitHeldReserve = held;
        emit ExitStarted(latestSeq, latestRoot, cursor, exitRefundReserve, held);
    }

    /// @notice Registers `account`'s equity at the exit root's marks as its claim, once. Anyone may
    /// register for any account; the claim is always the account's. See `ExitVerifier.accountClaim`.
    function registerExit(
        address account,
        ExitVerifier.AccountState calldata state,
        ExitVerifier.PositionState[] calldata positions,
        ExitVerifier.MarketState[] calldata markets
    ) external {
        _requireClaimWindow();
        if (exitAccountRegistered[account]) revert AlreadyRegistered();
        exitAccountRegistered[account] = true;
        _addExitClaim(account, exitVerifier.accountClaim(latestRoot, account, state, positions, markets));
    }

    /// @notice Registers an approval the exit root holds and the vault has not paid, whose amount the
    /// account's balance no longer holds, as part of its claim. It uses the approval's nonce.
    function registerExitWithdrawal(ExitVerifier.Approval calldata approval, bytes32[] calldata siblings) external {
        _requireClaimWindow();
        if (usedNonces[approval.account][approval.nonce]) revert WithdrawalAlreadyPaid();
        if (!exitVerifier.provesApproval(latestRoot, approval, siblings)) revert InvalidApprovalProof();
        usedNonces[approval.account][approval.nonce] = true;
        _addExitClaim(approval.account, approval.amount);
    }

    /// @notice Registers a queued request's unapproved remainder, which the account's balance no
    /// longer holds, as part of its claim, once per request.
    function registerExitQueued(ExitVerifier.QueuedWithdrawal calldata queued, bytes32[] calldata siblings) external {
        _requireClaimWindow();
        if (exitQueuedRegistered[queued.requestedSeq]) revert AlreadyRegistered();
        if (!exitVerifier.provesQueued(latestRoot, queued, siblings)) revert InvalidExitProof();
        exitQueuedRegistered[queued.requestedSeq] = true;
        // The queue holds whole base units; a fraction would round down.
        _addExitClaim(queued.account, queued.remaining > 0 ? uint256(int256(queued.remaining) / USDC_UNIT) : 0);
    }

    /// @notice Registers the part of `account`'s cumulative distribution at the exit root it was not
    /// already paid, once; distribution claims are paid only from what client claims leave.
    function registerExitDistribution(address account, uint256 entitled, bytes32[] calldata siblings) external {
        _requireClaimWindow();
        if (exitDistributionRegistered[account]) revert AlreadyRegistered();
        if (!exitVerifier.provesDistribution(latestRoot, account, entitled, siblings)) {
            revert InvalidEntitlementProof();
        }
        exitDistributionRegistered[account] = true;
        uint256 claimed = distributionsClaimed[account];
        uint256 owed = entitled > claimed ? entitled - claimed : 0;
        exitDistributionClaims[account] = owed;
        totalExitDistributionClaims += owed;
        emit ExitDistributionRegistered(account, owed);
    }

    /// @notice Pays `account` its share of each pool for the claims registered to it, once the claim
    /// window has closed. Anyone may relay; the USDC always goes to `account`.
    /// @return amount The USDC base units paid.
    function claimExit(address account) external nonReentrant returns (uint256 amount) {
        _settle();
        uint256 claim = exitClaims[account];
        uint256 distribution = exitDistributionClaims[account];
        if (claim == 0 && distribution == 0) revert NoExitClaim();
        exitClaims[account] = 0;
        exitDistributionClaims[account] = 0;
        if (claim != 0) amount = Math.mulDiv(claim, exitClientPool, totalExitClaims);
        if (distribution != 0) {
            amount += Math.mulDiv(distribution, exitDistributionPool, totalExitDistributionClaims);
        }
        emit ExitPaid(account, amount);
        if (amount != 0) _transferExact(account, amount);
    }

    /// @notice Refunds deposit `id` — every id from the exit root's deposit cursor on, and the ids
    /// below it the root does not hold — to the address that paid it, once the exit has started.
    function refundDeposit(uint256 id) external nonReentrant {
        if (exitStartedAt == 0) revert ExitNotStarted();
        if (id < exitDepositCursor && !exitRefundableGap[id]) revert NotRefundable(id);
        _refund(id);
    }

    /// @notice Refunds a deposit the exit root holds pending a declaration or screening to the address
    /// that paid it, once the exit has started. `chainRef` is its receipt's reference, which keys the
    /// leaf `siblings` prove.
    function refundPendingDeposit(
        bytes calldata chainRef,
        ExitVerifier.PendingDeposit calldata pending,
        bytes32[] calldata siblings
    ) external nonReentrant {
        if (exitStartedAt == 0) revert ExitNotStarted();
        if (!exitVerifier.provesPending(latestRoot, chainRef, pending, siblings)) revert InvalidExitProof();
        DepositRecord memory record = depositRecords[pending.depositId];
        if (record.payer != pending.payer || int256(uint256(record.amount)) * USDC_UNIT != pending.amount) {
            revert PendingDepositMismatch();
        }
        _refund(pending.depositId);
    }

    /// @notice Releases to `to` what the exit leaves once the claim window has closed: house capital
    /// and whatever claims did not register. Only the timelock may call it.
    function releaseHouseRemainder(address to) external onlyGovernance nonReentrant {
        if (to == address(0)) revert ZeroAddress();
        _settle();
        uint256 amount = houseRemainder;
        if (amount == 0) revert NoHouseRemainder();
        houseRemainder = 0;
        emit HouseRemainderReleased(to, amount);
        _transferExact(to, amount);
    }

    /// @notice Deposits `amount` USDC base units from the caller into the caller's trading account,
    /// which only a caller that signs with its own key can trade from.
    /// @return id The deposit's id, one more than the previous deposit's.
    function deposit(uint256 amount) external nonReentrant returns (uint256 id) {
        id = _deposit(msg.sender, amount);
    }

    /// @notice Deposits `amount` USDC base units from the caller into trading account `account`,
    /// which must be able to sign; the caller gets no claim on it.
    /// @return id The deposit's id, one more than the previous deposit's.
    function depositFor(address account, uint256 amount) external nonReentrant returns (uint256 id) {
        if (account == address(0)) revert ZeroAddress();
        id = _deposit(account, amount);
    }

    /// @notice Adds `amount` USDC base units from the caller to house capital. The caller gets no
    /// claim on it.
    /// @return id The deposit's id, one more than the previous deposit's.
    function depositToHouse(uint256 amount) external nonReentrant returns (uint256 id) {
        uint256 received = _pull(amount);
        id = _record(received, msg.sender);
        emit HouseDeposited(id, msg.sender, received);
    }

    /// @notice Books USDC the vault holds beyond its deposits less its withdrawals and distribution
    /// payments, such as a direct transfer, as house capital, at most `MAX_DEPOSIT` per call. A
    /// surplus does not pause withdrawals; until it is booked, the engine credits nobody with it. A
    /// surplus below `MIN_DEPOSIT` must be topped up first, as a deposit would be. A surplus carries
    /// no funding address, so access screening holds it rather than crediting it under the booker's
    /// name.
    /// @return id The deposit's id, one more than the previous deposit's.
    function bookSurplus() external nonReentrant returns (uint256 id) {
        _requireDepositsOpen();
        uint256 held = usdc.balanceOf(address(this)) + totalWithdrawn + distributionsPaid;
        uint256 surplus = held > totalDeposited ? held - totalDeposited : 0;
        if (surplus < MIN_DEPOSIT) revert NoSurplus(surplus);
        uint256 amount = surplus < MAX_DEPOSIT ? surplus : MAX_DEPOSIT;
        id = _record(amount, address(0));
        emit SurplusBooked(id, amount);
    }

    /// @dev Pulls `amount` from the caller and credits what arrived to trading account `account`.
    function _deposit(address account, uint256 amount) private returns (uint256 id) {
        uint256 received = _pull(amount);
        id = _record(received, msg.sender);
        emit Deposited(id, account, msg.sender, received);
    }

    /// @dev Whether `siblings`, from the root down, lead from the leaf at `key` with `valueHash` to
    /// `root` in the engine's sparse Merkle tree: a leaf hashes as `0x00 ‖ key ‖ valueHash`, a node as
    /// `0x01 ‖ left ‖ right`, and bit `d` of the key, most significant first, is the leaf's side at
    /// depth `d + 1`.
    function _proves(bytes32 root, bytes32 key, bytes32 valueHash, bytes32[] calldata siblings)
        private
        pure
        returns (bool)
    {
        return StateTree.proves(root, key, valueHash, siblings);
    }

    /// @dev Assigns the next deposit id to `amount` from `payer`, none for a booked surplus, and
    /// records the running totals through it. `amount` is at most `MAX_DEPOSIT`, so it fits a
    /// `uint96`.
    function _record(uint256 amount, address payer) private returns (uint256 id) {
        id = depositCount++;
        totalDeposited += amount;
        depositedThrough[id] = totalDeposited;
        if (payer != address(0)) {
            depositRecords[id] = DepositRecord(payer, uint96(amount));
            totalRefundable += amount;
        }
        refundableThrough[id] = totalRefundable;
    }

    /// @dev Refuses deposits while paused, and once checkpoints are stale, as they are throughout
    /// the exit, which starts only after `exitPeriod >= staleAfter`.
    function _requireDepositsOpen() private view {
        if (depositsPaused) revert DepositsPaused();
        if (checkpointsStale()) revert CheckpointsStale();
    }

    /// @dev Marks each id in `gaps` below `cursor` as refundable by the exit. The list is the lowest
    /// `MAX_EXIT_GAPS` ids, strictly increasing, as the engine commits it; their principal is
    /// already reserved in `terms.unheld`.
    /// @dev Marks each id in `gaps` below `cursor` as refundable by the exit and returns their
    /// canonical payer-funded principal, as the vault's own records hold it: the terms' `unheld`
    /// covers only the unnamed gaps, whose principal only the engine has seen. The list is the
    /// lowest `MAX_EXIT_GAPS` ids, strictly increasing, as the engine commits it.
    function _markRefundableGaps(uint64 cursor, uint64[] calldata gaps) private returns (uint256 refundable) {
        if (gaps.length > MAX_EXIT_GAPS) revert InvalidExitProof();
        uint64 previous;
        for (uint256 i; i < gaps.length; ++i) {
            uint64 id = gaps[i];
            if (id >= cursor || (i > 0 && id <= previous)) revert InvalidExitProof();
            previous = id;
            exitRefundableGap[id] = true;
            refundable += refundableThrough[id] - (id == 0 ? 0 : refundableThrough[id - 1]);
        }
    }

    /// @dev Refuses a registration outside the claim window, or with no root to prove against.
    function _requireClaimWindow() private view {
        if (exitStartedAt == 0) revert ExitNotStarted();
        if (block.timestamp >= uint256(exitStartedAt) + claimWindow) revert ClaimWindowClosed();
        if (!hasCheckpoint) revert NoCheckpoint();
    }

    /// @dev Adds `amount` to `account`'s exit claim.
    function _addExitClaim(address account, uint256 amount) private {
        exitClaims[account] += amount;
        totalExitClaims += amount;
        emit ExitClaimRegistered(account, amount);
    }

    /// @dev Fixes the pools once the claim window has closed: what the vault holds beyond the refunds
    /// it owes and the held principal goes first to client claims, then to distribution claims, each
    /// at most in full, and the rest to the house.
    function _settle() private {
        if (exitStartedAt == 0) revert ExitNotStarted();
        uint256 closesAt = uint256(exitStartedAt) + claimWindow;
        if (block.timestamp < closesAt) revert ClaimWindowOpen(closesAt);
        if (exitSettled) return;
        exitSettled = true;
        uint256 balance = usdc.balanceOf(address(this));
        uint256 reserved = exitRefundReserve + exitHeldReserve;
        uint256 pool = balance > reserved ? balance - reserved : 0;
        exitClientPool = Math.min(pool, totalExitClaims);
        exitDistributionPool = Math.min(pool - exitClientPool, totalExitDistributionClaims);
        houseRemainder = pool - exitClientPool - exitDistributionPool;
        emit ExitSettled(exitClientPool, exitDistributionPool, houseRemainder);
    }

    /// @dev Pays deposit `id`'s amount back to its payer, once, from the refund reserve.
    function _refund(uint256 id) private {
        DepositRecord memory record = depositRecords[id];
        if (record.payer == address(0) || depositRefunded[id]) revert NotRefundable(id);
        depositRefunded[id] = true;
        exitRefundReserve -= record.amount;
        emit DepositRefunded(id, record.payer, record.amount);
        _transferExact(record.payer, record.amount);
    }

    /// @dev Raw engine units as whole USDC base units, rounded up, and zero for a negative amount.
    function _usdcCeil(int128 raw) private pure returns (uint256) {
        if (raw <= 0) return 0;
        return (uint256(int256(raw)) + uint256(USDC_UNIT) - 1) / uint256(USDC_UNIT);
    }

    /// @dev Pulls `amount` from the caller and returns what the vault actually received.
    function _pull(uint256 amount) private returns (uint256 received) {
        _requireDepositsOpen();
        if (amount < MIN_DEPOSIT || amount > MAX_DEPOSIT) revert InvalidAmount(amount);
        uint256 before = usdc.balanceOf(address(this));
        usdc.safeTransferFrom(msg.sender, address(this), amount);
        received = usdc.balanceOf(address(this)) - before;
    }
}
