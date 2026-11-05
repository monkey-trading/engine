// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {SeedReceiver} from "../../src/SeedReceiver.sol";

/// @notice A seed receiver whose `seedOf` fails in a chosen way.
contract MockSeedReceiver {
    enum Failure {
        NoRelayedSeed,
        OtherError,
        Panic,
        OutOfGas
    }

    Failure public failure;

    function fail(Failure failure_) external {
        failure = failure_;
    }

    function seedOf(uint64 turn) external view returns (bytes32) {
        if (failure == Failure.NoRelayedSeed) revert SeedReceiver.NoRelayedSeed(turn);
        if (failure == Failure.OtherError) revert SeedReceiver.SeedNotFinal(turn, 0);
        if (failure == Failure.Panic) assert(false);
        // Spends every unit of gas it was given.
        while (true) {}
        return 0;
    }
}
