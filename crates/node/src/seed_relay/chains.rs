//! The node's chains behind `SeedChains`: Ethereum and the beacon node through the relayer's own
//! endpoints, and Arbitrum through the custody providers.

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, anyhow, bail};

use crate::custody::ContractQuorum;
use crate::seed_relay::abi::ISeedReceiver;
use crate::seed_relay::beacon::BeaconApi;
use crate::seed_relay::ethereum::{Ethereum, RelayIdentity};
use crate::seed_relay::schedule::last_final_turn;
use crate::seed_relay::{BeaconBlockHeader, MESSAGE_LENGTH, ReceiverView, SeedChains};

/// The node's chains: Ethereum and the beacon node through the relayer's endpoints, and Arbitrum
/// through the custody providers.
pub(crate) struct Chains {
    pub(super) ethereum: Ethereum,
    pub(super) beacon: BeaconApi,
    pub(super) arbitrum: ContractQuorum,
    pub(super) receiver: Address,
}

impl SeedChains for Chains {
    async fn receiver(&self) -> anyhow::Result<ReceiverView> {
        let seed = |turn: u64| ISeedReceiver::relayedSeedCall { turn }.abi_encode().into();
        let (time, answers) = self
            .arbitrum
            .read(self.receiver, |time| {
                let turn = last_final_turn(time);
                vec![
                    ("seed_receiver_relay", ISeedReceiver::relayCall {}.abi_encode().into()),
                    ("seed_receiver_genesis", ISeedReceiver::beaconGenesisTimeCall {}.abi_encode().into()),
                    ("seed_receiver_first", ISeedReceiver::firstRelayedTurnCall {}.abi_encode().into()),
                    ("seed_final_turn", seed(turn)),
                    ("seed_next_turn", seed(turn + 1)),
                    ("seed_turn_after", seed(turn + 2)),
                ]
            })
            .await?;
        let [relay, genesis, first, seeds @ ..] = answers.as_slice() else { bail!("the receiver read lost a call") };
        let seeds = seeds
            .iter()
            .map(|answer| ISeedReceiver::relayedSeedCall::abi_decode_returns(answer).context("decoding a seed"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(ReceiverView {
            time,
            relay: ISeedReceiver::relayCall::abi_decode_returns(relay).context("decoding the receiver's relay")?,
            genesis: ISeedReceiver::beaconGenesisTimeCall::abi_decode_returns(genesis).context("decoding genesis")?,
            first: ISeedReceiver::firstRelayedTurnCall::abi_decode_returns(first).context("decoding the first turn")?,
            final_turn: last_final_turn(time),
            seeds: seeds.try_into().map_err(|_| anyhow!("the receiver read lost a seed"))?,
        })
    }

    async fn identity(&self) -> anyhow::Result<RelayIdentity> {
        self.ethereum.identity().await
    }

    async fn finalized_header(&self) -> anyhow::Result<BeaconBlockHeader> {
        self.beacon.finalized_header().await
    }

    async fn beacon_root(&self, timestamp: u64) -> anyhow::Result<Option<B256>> {
        self.ethereum.beacon_root(timestamp).await
    }

    async fn state(&self, slot: u64) -> anyhow::Result<Vec<u8>> {
        self.beacon.state(slot).await
    }

    async fn accepts_fork(&self, version: [u8; 4]) -> anyhow::Result<bool> {
        self.ethereum.accepts_fork(version).await
    }

    async fn submission_cost(&self, inbox: Address) -> anyhow::Result<U256> {
        // The Inbox charges at the base fee of the block that includes the relay, which may rise
        // while it waits; what the ticket does not spend is refunded on Arbitrum.
        let base_fee = self.ethereum.base_fee().await?.saturating_mul(U256::from(2));
        self.ethereum.submission_fee(inbox, MESSAGE_LENGTH, base_fee).await
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        self.ethereum.balance().await
    }

    async fn send(&self, input: Vec<u8>, value: U256, nonce: u64, replacing: Option<B256>) -> anyhow::Result<B256> {
        self.ethereum.send(input, value, nonce, replacing).await
    }

    async fn receipt(&self, hash: B256) -> anyhow::Result<Option<bool>> {
        self.ethereum.receipt(hash).await
    }

    async fn nonces(&self) -> anyhow::Result<(u64, u64)> {
        self.ethereum.nonces().await
    }
}
