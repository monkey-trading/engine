// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {SeedReceiver} from "../../src/SeedReceiver.sol";

/// @notice A seed receiver whose `seedOf` answers a chosen seed, or fails in a chosen way.
contract MockSeedReceiver {
    enum Failure {
        NoRelayedSeed,
        OtherError,
        Panic,
        OutOfGas
    }

    Failure public failure;
    bytes32 public answer;

    function fail(Failure failure_) external {
        failure = failure_;
    }

    /// @dev Answers `seed` for every turn; zero fails as `failure` says.
    function answerWith(bytes32 seed) external {
        answer = seed;
    }

    function seedOf(uint64 turn) external view returns (bytes32) {
        if (answer != 0) return answer;
        if (failure == Failure.NoRelayedSeed) revert SeedReceiver.NoRelayedSeed(turn);
        if (failure == Failure.OtherError) revert SeedReceiver.SeedNotFinal(turn, 0);
        if (failure == Failure.Panic) assert(false);
        // Spends every unit of gas it was given.
        while (true) {}
        return 0;
    }
}
