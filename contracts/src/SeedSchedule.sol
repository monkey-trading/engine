// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

/// @title SeedSchedule
/// @notice Which beacon epoch's RANDAO mix seeds each daily operator turn, and until when it may be
/// relayed. Turn `n` starts at its boundary, 22:30 UTC on day `n` of the Unix epoch. Its draw
/// deadline is a day before the boundary, and from then its seed is final. Its seed is the RANDAO
/// mix at the end of the beacon epoch that contains the time two days before the draw deadline, so a
/// relay has two days to reach Arbitrum, more than Arbitrum One's 24-hour delay before anyone can
/// force-include a message its sequencer withholds. The schedule depends only on the turn number and
/// the beacon chain's genesis time, so every turn's epoch is fixed before that epoch's mix exists.
/// @dev `SeedRelay` on Ethereum and `SeedReceiver` on Arbitrum each check it, so a relay for any other
/// epoch fails on both chains. It counts epochs from the beacon chain's 12-second slots and 32-slot
/// epochs; a fork that changes either leaves every turn's epoch fixed by its number, but no longer
/// the epoch at that time.
abstract contract SeedSchedule {
    uint64 internal constant SLOTS_PER_EPOCH = 32;
    uint64 internal constant SECONDS_PER_EPOCH = 12 * SLOTS_PER_EPOCH;

    /// @notice A turn's boundary, in seconds after midnight UTC.
    uint64 public constant BOUNDARY_OFFSET = 22 hours + 30 minutes;
    /// @notice How long before its boundary a turn's seed is final and its draw may run.
    uint64 public constant DRAW_LEAD = 1 days;
    /// @notice How long before the draw deadline the seed's epoch ends.
    uint64 public constant RELAY_WINDOW = 2 days;

    /// @notice The beacon chain's genesis time on the Ethereum network the relay reads.
    uint64 public immutable beaconGenesisTime;

    error ZeroGenesisTime();
    error TurnBeforeGenesis(uint64 turn);

    constructor(uint64 genesisTime) {
        if (genesisTime == 0) revert ZeroGenesisTime();
        beaconGenesisTime = genesisTime;
    }

    /// @notice When turn `turn` starts.
    function boundary(uint64 turn) public pure returns (uint64) {
        return turn * 1 days + BOUNDARY_OFFSET;
    }

    /// @notice From this time turn `turn`'s seed is final: a relay for it is refused, and
    /// `SeedReceiver.seedOf` answers it.
    function drawDeadline(uint64 turn) public pure returns (uint64) {
        return boundary(turn) - DRAW_LEAD;
    }

    /// @notice The beacon epoch whose RANDAO mix seeds turn `turn`.
    function seedEpoch(uint64 turn) public view returns (uint64) {
        uint64 time = drawDeadline(turn) - RELAY_WINDOW;
        if (time < beaconGenesisTime) revert TurnBeforeGenesis(turn);
        return (time - beaconGenesisTime) / SECONDS_PER_EPOCH;
    }
}
