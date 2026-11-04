//! The ABIs the relayer calls, as `contracts/src/SeedRelay.sol`, `SeedReceiver.sol` and Arbitrum's
//! Inbox declare them.

alloy::sol! {
    /// `SeedRelay` on Ethereum, with its errors so a refused relay names its reason.
    #[derive(Debug)]
    interface ISeedRelay {
        struct BeaconBlockHeader {
            uint64 slot;
            uint64 proposerIndex;
            bytes32 parentRoot;
            bytes32 stateRoot;
            bytes32 bodyRoot;
        }
        struct Witness {
            uint64 timestamp;
            BeaconBlockHeader header;
            bytes4 forkVersion;
            bytes32[] forkProof;
            bytes32 seed;
            bytes32[] seedProof;
        }
        struct Fees {
            uint256 maxSubmissionCost;
            uint256 gasLimit;
            uint256 maxFeePerGas;
        }
        function relay(uint64 turn, Witness witness, Fees fees) external payable returns (uint256 ticket);
        function receiver() external view returns (address);
        function inbox() external view returns (address);
        function beaconGenesisTime() external view returns (uint64);
        function acceptedFork(bytes4 version) external view returns (bool);

        error DrawDeadlinePassed(uint64 turn, uint64 deadline);
        error BeaconRootUnavailable(uint64 timestamp);
        error HeaderMismatch(bytes32 beaconRoot, bytes32 headerRoot);
        error EpochNotEnded(uint64 epoch, uint64 slot);
        error MixOverwritten(uint64 epoch, uint64 slot);
        error ForkNotAccepted(bytes4 version);
        error InvalidForkProof();
        error InvalidSeedProof();
        error TurnBeforeGenesis(uint64 turn);
    }

    /// `SeedReceiver` on Arbitrum.
    interface ISeedReceiver {
        function relay() external view returns (address);
        function beaconGenesisTime() external view returns (uint64);
        function relayedSeed(uint64 turn) external view returns (bytes32);
        function firstRelayedTurn() external view returns (uint64);
        function receiveSeed(uint64 turn, uint64 epoch, bytes32 seed) external;
    }

    /// Arbitrum's delayed inbox on Ethereum.
    interface IInbox {
        function calculateRetryableSubmissionFee(uint256 dataLength, uint256 baseFee) external view returns (uint256);
    }
}
