// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {SeedReceiver} from "./SeedReceiver.sol";
import {SeedSchedule} from "./SeedSchedule.sol";

/// @notice Arbitrum's delayed inbox on Ethereum, as far as a retryable ticket needs it.
interface IInbox {
    function createRetryableTicket(
        address to,
        uint256 l2CallValue,
        uint256 maxSubmissionCost,
        address excessFeeRefundAddress,
        address callValueRefundAddress,
        uint256 gasLimit,
        uint256 maxFeePerGas,
        bytes calldata data
    ) external payable returns (uint256);
}

/// @title SeedRelay
/// @notice Proves on Ethereum the RANDAO mix at the end of the beacon epoch `SeedSchedule` fixes for
/// a turn, and sends it to the `SeedReceiver` on Arbitrum as a retryable ticket. Anyone may relay: the
/// proof alone decides what is sent, so every relay of a turn sends the same seed.
/// @dev The proof, against a beacon block root the EIP-4788 contract holds:
/// - the header hashes to that root (`hash_tree_root` of a `BeaconBlockHeader`);
/// - its state's `fork.current_version` is one this relay was deployed to accept, each a fork whose
///   `BeaconState` is the 64-leaf tree below; any other fork is refused, since one that changed the
///   layout would make the same generalized index name another field, so a fork whose version was not
///   listed at deployment stops relays until a new relay and receiver are deployed;
/// - its slot is past the epoch's last slot, and before the slot at which `randao_mixes` reuses the
///   epoch's index, so the state holds that epoch's final mix;
/// - `randao_mixes[epoch % 65536]` is the seed.
/// Each branch is checked with the consensus specification's `is_valid_merkle_branch` over SHA-256.
///
/// What it trusts:
/// - Ethereum's consensus, for the beacon roots the EIP-4788 contract records, which hold about 27
///   hours of blocks; the RANDAO mix itself is biasable by the proposers of the epoch's last slots,
///   each of whom can withhold its block.
/// - Arbitrum's delayed inbox: the sequencer includes the message, or anyone force-includes it after
///   Arbitrum's delay, before the turn's draw deadline. A message that misses the deadline is
///   refused, and the turn draws from `SeedReceiver`'s fallback.
/// - At least one relayer: nothing here runs by itself.
/// The ticket's beneficiary is the receiver, which cannot cancel a ticket, so a ticket whose
/// automatic redemption fails stays redeemable by anyone until it expires.
contract SeedRelay is SeedSchedule {
    /// @notice The EIP-4788 beacon block root contract, at the same address on every network that has it.
    address public constant BEACON_ROOTS = 0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02;

    /// @notice The `BeaconState`'s fields fill a 64-leaf tree from Electra on; `fork` is field 3 and
    /// `randao_mixes` field 13.
    uint256 internal constant STATE_DEPTH = 6;
    uint256 internal constant FORK_FIELD = 3;
    uint256 internal constant RANDAO_MIXES_FIELD = 13;
    /// @notice `Fork` is `(previous_version, current_version, epoch)`, a 4-leaf tree.
    uint256 internal constant FORK_DEPTH = 2;
    uint256 internal constant CURRENT_VERSION_FIELD = 1;
    /// @notice `randao_mixes` holds `EPOCHS_PER_HISTORICAL_VECTOR` mixes, a 2^16-leaf tree.
    uint256 internal constant EPOCHS_PER_HISTORICAL_VECTOR = 65536;
    uint256 internal constant MIXES_DEPTH = 16;

    /// @notice Siblings from the state root to `fork.current_version`.
    uint256 public constant FORK_PROOF_LENGTH = STATE_DEPTH + FORK_DEPTH;
    /// @notice Siblings from the state root to a mix.
    uint256 public constant SEED_PROOF_LENGTH = STATE_DEPTH + MIXES_DEPTH;

    /// @notice A beacon block header, whose `hash_tree_root` is its block's root.
    struct BeaconBlockHeader {
        uint64 slot;
        uint64 proposerIndex;
        bytes32 parentRoot;
        bytes32 stateRoot;
        bytes32 bodyRoot;
    }

    /// @notice What proves a seed: the time of an execution block whose EIP-4788 entry is `header`'s
    /// root, and two branches up to `header.stateRoot`, each from the leaf to the root.
    struct Witness {
        uint64 timestamp;
        BeaconBlockHeader header;
        bytes4 forkVersion;
        bytes32[] forkProof;
        bytes32 seed;
        bytes32[] seedProof;
    }

    /// @notice The retryable ticket's fees, which the Inbox checks against `msg.value`; what the
    /// ticket does not spend is refunded on Arbitrum to the caller.
    struct Fees {
        uint256 maxSubmissionCost;
        uint256 gasLimit;
        uint256 maxFeePerGas;
    }

    /// @notice Arbitrum's delayed inbox.
    IInbox public immutable inbox;
    /// @notice The `SeedReceiver` on Arbitrum.
    address public immutable receiver;
    /// @notice The fork versions whose states this relay reads.
    mapping(bytes4 version => bool) public acceptedFork;

    event SeedRelayed(uint64 indexed turn, uint64 epoch, bytes32 seed, uint256 ticket);

    error ZeroAddress();
    error NoForks();
    error DrawDeadlinePassed(uint64 turn, uint64 deadline);
    error BeaconRootUnavailable(uint64 timestamp);
    error HeaderMismatch(bytes32 beaconRoot, bytes32 headerRoot);
    error EpochNotEnded(uint64 epoch, uint64 slot);
    error MixOverwritten(uint64 epoch, uint64 slot);
    error ForkNotAccepted(bytes4 version);
    error InvalidForkProof();
    error InvalidSeedProof();

    constructor(IInbox inbox_, address receiver_, uint64 genesisTime, bytes4[] memory forks) SeedSchedule(genesisTime) {
        if (address(inbox_) == address(0) || receiver_ == address(0)) revert ZeroAddress();
        if (forks.length == 0) revert NoForks();
        inbox = inbox_;
        receiver = receiver_;
        for (uint256 i = 0; i < forks.length; ++i) {
            if (forks[i] == bytes4(0)) revert ForkNotAccepted(forks[i]);
            acceptedFork[forks[i]] = true;
        }
    }

    /// @notice Proves turn `turn`'s seed and sends it to the receiver; `msg.value` pays the ticket.
    /// @return ticket The retryable ticket's number in the Inbox.
    function relay(uint64 turn, Witness calldata witness, Fees calldata fees)
        external
        payable
        returns (uint256 ticket)
    {
        uint64 deadline = drawDeadline(turn);
        if (block.timestamp >= deadline) revert DrawDeadlinePassed(turn, deadline);
        uint64 epoch = seedEpoch(turn);
        verify(epoch, witness);
        ticket = send(abi.encodeCall(SeedReceiver.receiveSeed, (turn, epoch, witness.seed)), fees);
        emit SeedRelayed(turn, epoch, witness.seed, ticket);
    }

    /// @dev Sends `data` to the receiver as a retryable ticket paid with `msg.value`. The caller gets
    /// the unspent fees on Arbitrum; the receiver is the beneficiary, so no one can cancel the ticket.
    function send(bytes memory data, Fees calldata fees) internal returns (uint256) {
        return inbox.createRetryableTicket{value: msg.value}(
            receiver, 0, fees.maxSubmissionCost, msg.sender, receiver, fees.gasLimit, fees.maxFeePerGas, data
        );
    }

    /// @dev Reverts unless `witness` proves `witness.seed` is the RANDAO mix at the end of `epoch`.
    function verify(uint64 epoch, Witness calldata witness) internal view {
        BeaconBlockHeader calldata header = witness.header;
        bytes32 beaconRoot = beaconRootAt(witness.timestamp);
        bytes32 headerRoot = hashTreeRoot(header);
        if (headerRoot != beaconRoot) revert HeaderMismatch(beaconRoot, headerRoot);
        // The state is the one after the block at `header.slot`. Once past the epoch's last slot,
        // `randao_mixes[epoch % 65536]` holds the epoch's final mix until epoch `epoch + 65536` reuses it.
        if (header.slot < (uint256(epoch) + 1) * SLOTS_PER_EPOCH) revert EpochNotEnded(epoch, header.slot);
        if (header.slot >= (uint256(epoch) + EPOCHS_PER_HISTORICAL_VECTOR) * SLOTS_PER_EPOCH) {
            revert MixOverwritten(epoch, header.slot);
        }
        if (!acceptedFork[witness.forkVersion]) revert ForkNotAccepted(witness.forkVersion);
        // An SSZ `Version` is four bytes, left-aligned in its chunk.
        uint256 forkIndex = (FORK_FIELD << FORK_DEPTH) | CURRENT_VERSION_FIELD;
        if (witness.forkProof.length != FORK_PROOF_LENGTH) revert InvalidForkProof();
        if (!isValidMerkleBranch(bytes32(witness.forkVersion), witness.forkProof, forkIndex, header.stateRoot)) {
            revert InvalidForkProof();
        }
        uint256 seedIndex = (RANDAO_MIXES_FIELD << MIXES_DEPTH) | (epoch % EPOCHS_PER_HISTORICAL_VECTOR);
        if (witness.seedProof.length != SEED_PROOF_LENGTH) revert InvalidSeedProof();
        if (!isValidMerkleBranch(witness.seed, witness.seedProof, seedIndex, header.stateRoot)) {
            revert InvalidSeedProof();
        }
    }

    /// @dev The parent beacon block root of the execution block at `timestamp`, from EIP-4788's ring
    /// buffer, which refuses a time it holds no entry for.
    function beaconRootAt(uint64 timestamp) internal view returns (bytes32) {
        (bool ok, bytes memory root) = BEACON_ROOTS.staticcall(abi.encode(timestamp));
        if (!ok || root.length != 32) revert BeaconRootUnavailable(timestamp);
        return abi.decode(root, (bytes32));
    }

    /// @dev SSZ `hash_tree_root` of a `BeaconBlockHeader`: its five fields as the leaves of an
    /// 8-leaf tree, integers little-endian.
    function hashTreeRoot(BeaconBlockHeader calldata header) internal pure returns (bytes32) {
        bytes32 zero = sha256(abi.encodePacked(bytes32(0), bytes32(0)));
        return sha256(
            abi.encodePacked(
                sha256(
                    abi.encodePacked(
                        sha256(abi.encodePacked(littleEndian(header.slot), littleEndian(header.proposerIndex))),
                        sha256(abi.encodePacked(header.parentRoot, header.stateRoot))
                    )
                ),
                sha256(abi.encodePacked(sha256(abi.encodePacked(header.bodyRoot, bytes32(0))), zero))
            )
        );
    }

    /// @dev An SSZ `uint64` chunk: the value's eight bytes, least significant first, then zeros.
    function littleEndian(uint64 value) internal pure returns (bytes32) {
        uint256 reversed;
        for (uint256 i = 0; i < 8; ++i) {
            reversed = (reversed << 8) | ((value >> (8 * i)) & 0xff);
        }
        return bytes32(reversed << 192);
    }

    /// @dev The consensus specification's `is_valid_merkle_branch`: `branch` runs from the leaf up,
    /// and bit `i` of `index`, the leaf's position at depth `branch.length`, puts the running hash on
    /// the right of `branch[i]`. The callers fix the depth and index.
    function isValidMerkleBranch(bytes32 leaf, bytes32[] calldata branch, uint256 index, bytes32 root)
        internal
        pure
        returns (bool)
    {
        bytes32 value = leaf;
        for (uint256 i = 0; i < branch.length; ++i) {
            value = (index >> i) & 1 == 1
                ? sha256(abi.encodePacked(branch[i], value))
                : sha256(abi.encodePacked(value, branch[i]));
        }
        return value == root;
    }
}
