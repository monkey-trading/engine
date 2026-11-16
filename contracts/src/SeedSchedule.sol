// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

/// @title SeedSchedule
/// @notice Which beacon epoch's RANDAO mix seeds each operator turn, and until when it may be relayed.
/// Turns last `turnLength` seconds, a day in production, and turn `n` starts at its boundary,
/// `n * turnLength + boundaryOffset`: with day-long turns, 22:30 UTC on day `n` of the Unix epoch. Its
/// draw deadline is one turn before the boundary, and from then its seed is final. Its seed is the
/// RANDAO mix at the end of the beacon epoch that contains the time two turns before the draw deadline,
/// so a relay has two turns to reach Arbitrum: with day-long turns, more than Arbitrum One's 24-hour
/// delay before anyone can force-include a message its sequencer withholds. The schedule depends only on
/// the turn number, the turn length and the beacon chain's genesis time, so every turn's epoch is fixed
/// before that epoch's mix exists.
/// @notice A turn shorter than a day is for development stacks only: a production engine build has
/// day-long turns and refuses a schedule of any other length, and two short turns are less than
/// mainnet's beacon finality, so such a stack's seeds come from a stand-in beacon chain.
/// @dev `SeedRelay` on Ethereum and `SeedReceiver` on Arbitrum each check it, so a relay for any other
/// epoch fails on both chains. It counts epochs from the beacon chain's 12-second slots and 32-slot
/// epochs; a fork that changes either leaves every turn's epoch fixed by its number, but no longer
/// the epoch at that time.
abstract contract SeedSchedule {
    uint64 internal constant SLOTS_PER_EPOCH = 32;
    uint64 internal constant SECONDS_PER_EPOCH = 12 * SLOTS_PER_EPOCH;

    /// @notice The shortest turn a deployment may take.
    uint64 internal constant MIN_TURN_LENGTH = 5 minutes;
    /// @dev 22:30 UTC, in seconds after midnight: a boundary at every turn length.
    uint64 internal constant DAILY_BOUNDARY = 22 hours + 30 minutes;

    /// @notice The beacon chain's genesis time on the Ethereum network the relay reads.
    uint64 public immutable beaconGenesisTime;
    /// @notice A turn's length, in seconds: a day in production, and otherwise a divisor of a day of at
    /// least `MIN_TURN_LENGTH`. It is also the draw lead, and the relay window is two of it.
    uint64 public immutable turnLength;
    /// @dev A turn's boundary, in seconds past a multiple of the turn length: 22:30 UTC modulo it, which
    /// `boundary(0)` answers.
    uint64 internal immutable boundaryOffset;

    error ZeroGenesisTime();
    error InvalidTurnLength(uint64 turnLength);
    error TurnBeforeGenesis(uint64 turn);

    constructor(uint64 genesisTime, uint64 turnLength_) {
        if (genesisTime == 0) revert ZeroGenesisTime();
        if (turnLength_ < MIN_TURN_LENGTH || 1 days % turnLength_ != 0) revert InvalidTurnLength(turnLength_);
        beaconGenesisTime = genesisTime;
        turnLength = turnLength_;
        boundaryOffset = DAILY_BOUNDARY % turnLength_;
    }

    /// @notice When turn `turn` starts; turn `turn` ends at `boundary(turn + 1)`.
    function boundary(uint64 turn) public view returns (uint64) {
        return turn * turnLength + boundaryOffset;
    }

    /// @notice From this time turn `turn`'s seed is final: a relay for it is refused, and
    /// `SeedReceiver.seedOf` answers it.
    function drawDeadline(uint64 turn) public view returns (uint64) {
        return boundary(turn) - turnLength;
    }

    /// @notice The beacon epoch whose RANDAO mix seeds turn `turn`.
    function seedEpoch(uint64 turn) public view returns (uint64) {
        // In 256 bits, so a turn too early for its seed time is refused by name rather than underflowing.
        uint256 time = uint256(turn) * turnLength + boundaryOffset;
        // The draw lead, one turn, and the relay window, two.
        uint256 lead = 3 * uint256(turnLength) + beaconGenesisTime;
        if (time < lead) revert TurnBeforeGenesis(turn);
        return uint64((time - lead) / SECONDS_PER_EPOCH);
    }
}
