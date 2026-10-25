// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";
import {EIP712} from "@openzeppelin-contracts/utils/cryptography/EIP712.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {MonkeyToken} from "./MonkeyToken.sol";

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
/// @dev The attested signer authorizes checkpoints, withdrawals, and distribution claims; the
/// deployment's multisig controls signer rotation and pauses through its timelock. This contract
/// is not upgradeable.
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

    /// @notice Smallest single deposit in USDC base units: 10 USDC. Every deposit becomes a
    /// permanent entry in the engine's state, so a floor makes each one cost locked capital, not
    /// only gas.
    uint256 public constant MIN_DEPOSIT = 1e7;

    /// @notice Largest single deposit in USDC base units: 1e9 USDC, the engine's input bound.
    uint256 public constant MAX_DEPOSIT = 1e15;

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
    TimelockController public immutable governance;
    bool public depositsPaused;
    bool public withdrawalsPaused;
    bool public hasCheckpoint;
    uint64 public latestSeq;
    bytes32 public latestRoot;
    mapping(uint64 seq => bytes32 root) public checkpointRoots;
    mapping(uint64 seq => bool accepted) public hasCheckpointAt;
    mapping(uint64 seq => address signer) public checkpointSignerAt;
    mapping(uint64 seq => uint64 epoch) public checkpointEpochAt;
    mapping(address account => mapping(uint64 nonce => bool paid)) public usedNonces;

    event CheckpointPosted(uint64 indexed seq, bytes32 root, bytes signature);
    event WithdrawalPaid(address indexed account, uint256 amount, uint64 indexed nonce, uint64 approvedSeq);
    event SignerChanged(address indexed previousSigner, address indexed signer, uint64 signerEpoch);
    event PausesChanged(bool depositsPaused, bool withdrawalsPaused);

    /// @notice `amount` USDC base units deposited to trader account `account`.
    event Deposited(uint256 indexed id, address indexed account, uint256 amount);

    /// @notice `amount` USDC base units added to house capital by `from`.
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);

    /// @notice `amount` MONKEY base units minted to `account`, bringing its total to `entitled`, the
    /// entitlement checkpoint `seq` proves.
    event EmissionsClaimed(address indexed account, uint256 amount, uint256 entitled, uint64 seq);

    /// @notice `amount` USDC base units paid to `account`, bringing its total to `entitled`, the
    /// distribution checkpoint `seq` proves.
    event DistributionClaimed(address indexed account, uint256 amount, uint256 entitled, uint64 seq);

    error ZeroAddress();
    error InvalidAmount(uint256 amount);
    error CheckpointNotExtended();
    error InvalidCheckpointSigner();
    error CheckpointParentMismatch();
    error UnsupportedChain();
    error InvalidGovernance();
    error UnauthorizedGovernance();
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

    /// @param governance_ The deployment's timelock, which alone rotates the signer and pauses.
    /// @param periodBudget_ The engine's initial emission period budget in MONKEY base units, which
    /// sets the token's cap at `2 * PERIODS_PER_HALVING * periodBudget_`.
    constructor(IERC20 usdc_, address checkpointSigner_, TimelockController governance_, uint256 periodBudget_)
        EIP712("EngineWithdrawal", "1")
    {
        if (address(usdc_) == address(0) || checkpointSigner_ == address(0)) revert ZeroAddress();
        if (block.chainid > type(uint64).max) revert UnsupportedChain();
        if (address(governance_).code.length == 0 || governance_.getMinDelay() == 0) revert InvalidGovernance();
        if (periodBudget_ == 0 || periodBudget_ > MAX_PERIOD_BUDGET) revert InvalidPeriodBudget(periodBudget_);
        usdc = usdc_;
        checkpointSigner = checkpointSigner_;
        signerAtEpoch[signerEpoch] = checkpointSigner_;
        governance = governance_;
        monkey = new MonkeyToken(2 * PERIODS_PER_HALVING * periodBudget_);
    }

    modifier onlyGovernance() {
        if (msg.sender != address(governance)) revert UnauthorizedGovernance();
        _;
    }

    /// @notice Retires existing withdrawal authorizations, including when reusing the signer address.
    function setSigner(address signer) external onlyGovernance {
        if (signer == address(0)) revert ZeroAddress();
        address previous = checkpointSigner;
        checkpointSigner = signer;
        signerEpoch++;
        signerAtEpoch[signerEpoch] = signer;
        emit SignerChanged(previous, signer, signerEpoch);
    }

    /// @notice Independently pauses deposits (including house capital) and withdrawal claims.
    function setPauses(bool depositsPaused_, bool withdrawalsPaused_) external onlyGovernance {
        depositsPaused = depositsPaused_;
        withdrawalsPaused = withdrawalsPaused_;
        emit PausesChanged(depositsPaused_, withdrawalsPaused_);
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
        if (withdrawalsPaused) revert WithdrawalsPaused();
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
        id = _record(received);
        emit HouseDeposited(id, msg.sender, received);
    }

    /// @notice Books USDC the vault holds beyond its deposits less its payments, such as a direct
    /// transfer, as house capital, at most `MAX_DEPOSIT` per call. A surplus does not pause
    /// withdrawals; until it is booked, the engine credits nobody with it. A surplus below
    /// `MIN_DEPOSIT` must be topped up first, as a deposit would be.
    /// @return id The deposit's id, one more than the previous deposit's.
    function bookSurplus() external nonReentrant returns (uint256 id) {
        if (depositsPaused) revert DepositsPaused();
        uint256 held = usdc.balanceOf(address(this)) + totalWithdrawn;
        uint256 surplus = held > totalDeposited ? held - totalDeposited : 0;
        if (surplus < MIN_DEPOSIT) revert NoSurplus(surplus);
        uint256 amount = surplus < MAX_DEPOSIT ? surplus : MAX_DEPOSIT;
        id = _record(amount);
        emit HouseDeposited(id, msg.sender, amount);
    }

    /// @dev Pulls `amount` from the caller and credits what arrived to trading account `account`.
    function _deposit(address account, uint256 amount) private returns (uint256 id) {
        uint256 received = _pull(amount);
        id = _record(received);
        emit Deposited(id, account, received);
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
        if (siblings.length > 256) return false;
        bytes32 hash = keccak256(abi.encodePacked(bytes1(0x00), key, valueHash));
        for (uint256 depth = siblings.length; depth > 0;) {
            unchecked {
                --depth;
            }
            bool right = (uint256(key) >> (255 - depth)) & 1 == 1;
            hash = right
                ? keccak256(abi.encodePacked(bytes1(0x01), siblings[depth], hash))
                : keccak256(abi.encodePacked(bytes1(0x01), hash, siblings[depth]));
        }
        return hash == root;
    }

    /// @dev Assigns the next deposit id to `amount` and records the running total through it.
    function _record(uint256 amount) private returns (uint256 id) {
        id = depositCount++;
        totalDeposited += amount;
        depositedThrough[id] = totalDeposited;
    }

    /// @dev Pulls `amount` from the caller and returns what the vault actually received.
    function _pull(uint256 amount) private returns (uint256 received) {
        if (depositsPaused) revert DepositsPaused();
        if (amount < MIN_DEPOSIT || amount > MAX_DEPOSIT) revert InvalidAmount(amount);
        uint256 before = usdc.balanceOf(address(this));
        usdc.safeTransferFrom(msg.sender, address(this), amount);
        received = usdc.balanceOf(address(this)) - before;
    }
}
