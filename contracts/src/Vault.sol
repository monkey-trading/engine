// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";
import {EIP712} from "@openzeppelin-contracts/utils/cryptography/EIP712.sol";
import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

/// @title Vault
/// @notice Holds the engine's USDC collateral. The chain handles custody only: the engine credits
/// each deposit event once it is final, keyed by transaction hash and log index.
/// @notice A trading account must be able to sign the engine's EIP-712 requests with its own key:
/// the engine accepts no contract signatures. A contract wallet funds a key that signs for it
/// through `depositFor`; a balance credited to a contract account can be recovered only through
/// the exit (#48).
/// @dev The attested signer authorizes checkpoints and withdrawals; the deployment's multisig
/// controls signer rotation and pauses through its timelock. This contract is not upgradeable.
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

    /// @notice The collateral token: USDC, 6 decimals.
    IERC20 public immutable usdc;

    /// @notice Deposits accepted so far, trader and house alike; the next deposit's id.
    uint256 public depositCount;
    uint256 public totalWithdrawn;

    /// @notice Authorized enclave address, provisioned after verifying its attestation.
    address public checkpointSigner;
    uint64 public signerEpoch = 1;
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

    constructor(IERC20 usdc_, address checkpointSigner_, TimelockController governance_)
        EIP712("EngineWithdrawal", "1")
    {
        if (address(usdc_) == address(0) || checkpointSigner_ == address(0)) {
            revert ZeroAddress();
        }
        if (block.chainid > type(uint64).max) revert UnsupportedChain();
        if (address(governance_).code.length == 0 || governance_.getMinDelay() == 0) revert InvalidGovernance();
        usdc = usdc_;
        checkpointSigner = checkpointSigner_;
        governance = governance_;
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

    /// @dev Verifies the engine's compressed SMT, with root-to-leaf siblings and MSB-first key bits.
    function _verifyApproval(Withdrawal calldata withdrawal, bytes32 root, bytes32[] calldata siblings)
        private
        pure
        returns (bool)
    {
        if (siblings.length > 256) return false;
        bytes32 key = keccak256(abi.encodePacked(uint8(32), withdrawal.account, withdrawal.nonce));
        bytes32 valueHash =
            keccak256(abi.encode(withdrawal.account, withdrawal.amount, withdrawal.nonce, withdrawal.approvedSeq));
        bytes32 hash = keccak256(abi.encodePacked(bytes1(0), key, valueHash));
        for (uint256 depth = siblings.length; depth > 0;) {
            --depth;
            if (((uint256(key) >> (255 - depth)) & 1) == 0) {
                hash = keccak256(abi.encodePacked(bytes1(0x01), hash, siblings[depth]));
            } else {
                hash = keccak256(abi.encodePacked(bytes1(0x01), siblings[depth], hash));
            }
        }
        return hash == root;
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
        id = depositCount++;
        emit HouseDeposited(id, msg.sender, received);
    }

    /// @dev Pulls `amount` from the caller and credits what arrived to trading account `account`.
    function _deposit(address account, uint256 amount) private returns (uint256 id) {
        uint256 received = _pull(amount);
        id = depositCount++;
        emit Deposited(id, account, received);
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
