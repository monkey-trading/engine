//! The seed relayer: it proves on Ethereum the RANDAO mix that seeds each daily operator turn and
//! relays it to the operator registry's seed receiver on Arbitrum (`contracts/src/SeedRelay.sol`,
//! `SeedReceiver.sol`).

mod header;
pub use header::BeaconBlockHeader;
mod ssz;
pub mod state;
