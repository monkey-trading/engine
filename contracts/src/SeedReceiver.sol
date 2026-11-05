// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {SeedSchedule} from "./SeedSchedule.sol";

/// @title SeedReceiver
/// @notice Records on Arbitrum the seeds `SeedRelay` proves on Ethereum, one per daily operator turn,
/// and answers each turn's final seed from its draw deadline on.
/// @dev A seed is accepted only in a retryable ticket from the relay, which arrives from the relay's
/// address with Arbitrum's L1-to-L2 alias applied; only for the epoch `SeedSchedule` fixes for its
/// turn; and only before that turn's draw deadline. A turn with no relayed seed by then draws from
/// the last seed relayed for an earlier turn, hashed with its own turn number: every turn before it
/// has passed its own deadline, so that answer cannot change either. Who could stop every relay
/// until a deadline can choose between a turn's mix and that fallback, which is why the schedule
/// leaves a relay two days. No key administers this contract.
contract SeedReceiver is SeedSchedule {
    /// @notice What Arbitrum adds to an Ethereum contract's address as the sender of its L1-to-L2
    /// messages (Arbitrum's `AddressAliasHelper`).
    uint160 internal constant L1_TO_L2_ALIAS_OFFSET = uint160(0x1111000000000000000000000000000000001111);

    /// @notice The `SeedRelay` on Ethereum.
    address public immutable relay;
    /// @notice The sender of the relay's messages here.
    address public immutable relayAlias;

    /// @notice Each turn's relayed seed; zero while it has none.
    mapping(uint64 turn => bytes32) public relayedSeed;
    /// @notice The earliest turn with a relayed seed; zero before the first relay, since turn 0 has no
    /// draw deadline.
    uint64 public firstRelayedTurn;

    event SeedRecorded(uint64 indexed turn, uint64 epoch, bytes32 seed);

    error ZeroAddress();
    error NotRelay(address sender);
    error WrongEpoch(uint64 turn, uint64 epoch, uint64 expected);
    error DrawDeadlinePassed(uint64 turn, uint64 deadline);
    error ZeroSeed();
    error AlreadyRelayed(uint64 turn);
    error SeedNotFinal(uint64 turn, uint64 deadline);
    error NoRelayedSeed(uint64 turn);

    constructor(address relay_, uint64 genesisTime) SeedSchedule(genesisTime) {
        if (relay_ == address(0)) revert ZeroAddress();
        relay = relay_;
        unchecked {
            relayAlias = address(uint160(relay_) + L1_TO_L2_ALIAS_OFFSET);
        }
    }

    /// @notice Records `seed` as turn `turn`'s, as the relay proved it for `epoch`.
    function receiveSeed(uint64 turn, uint64 epoch, bytes32 seed) external {
        if (msg.sender != relayAlias) revert NotRelay(msg.sender);
        uint64 expected = seedEpoch(turn);
        if (epoch != expected) revert WrongEpoch(turn, epoch, expected);
        uint64 deadline = drawDeadline(turn);
        if (block.timestamp >= deadline) revert DrawDeadlinePassed(turn, deadline);
        // A RANDAO mix is a hash; zero marks a turn with none.
        if (seed == 0) revert ZeroSeed();
        if (relayedSeed[turn] != 0) revert AlreadyRelayed(turn);
        relayedSeed[turn] = seed;
        uint64 first = firstRelayedTurn;
        if (first == 0 || turn < first) firstRelayedTurn = turn;
        emit SeedRecorded(turn, epoch, seed);
    }

    /// @notice Turn `turn`'s final seed: its relayed seed, or from its draw deadline
    /// `keccak256(abi.encode(s, turn))`, where `s` is the seed of the latest earlier turn that has one.
    /// @dev The fallback reads back through the turns with no seed, one storage read each.
    function seedOf(uint64 turn) external view returns (bytes32 seed) {
        seed = relayedSeed[turn];
        if (seed != 0) return seed;
        uint64 deadline = drawDeadline(turn);
        if (block.timestamp < deadline) revert SeedNotFinal(turn, deadline);
        uint64 first = firstRelayedTurn;
        if (first == 0 || turn < first) revert NoRelayedSeed(turn);
        // `first` has a seed and lies below `turn`, so the walk ends there at the latest.
        for (uint64 earlier = turn - 1;; --earlier) {
            bytes32 last = relayedSeed[earlier];
            if (last != 0) return keccak256(abi.encode(last, turn));
        }
    }
}
