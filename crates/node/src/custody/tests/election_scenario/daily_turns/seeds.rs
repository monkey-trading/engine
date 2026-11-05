//! The daily turns' seed source, the seed relay scenario's two-chain fixture: a second Anvil as
//! Ethereum, whose EIP-4788 contract records a stand-in beacon node's block roots, the relay and an
//! Inbox stand-in on it, the receiver on the scenario's chain, a stand-in bridge that delivers each
//! ticket from the relay's alias, and the node's own relayer task.

use std::time::Duration;

use alloy::primitives::{Address, B256};
use alloy::sol;
use tokio::task::JoinHandle;

use super::super::T0_SECS;
use super::super::chain::Chain;
use crate::metrics::Metrics;
use crate::seed_relay::tests::scenario::{
    Beacon, Chain as Ethereum, bridge, deploy_relay, finalize_beacon, relayer_config,
};

sol! {
    function relayedSeed(uint64 turn) external view returns (bytes32);
}

/// The stand-in Ethereum's chain id, apart from the custody chain's 31337.
const ETHEREUM_CHAIN_ID: u64 = 31_338;

/// The running seed source; dropping it stops the beacon node, the bridge and the relayer.
pub(super) struct Seeds {
    ethereum: Ethereum,
    beacon: Beacon,
    tasks: Vec<JoinHandle<()>>,
    /// The `SeedReceiver` on the scenario's chain.
    pub(super) receiver: Address,
}

impl Seeds {
    /// Starts Ethereum an hour before the scenario's chain, deploys the relay there and the receiver
    /// on `chain`, and starts the beacon node, the bridge and the relayer, which reads the receiver
    /// through `chain`'s custody proxies.
    pub(super) async fn start(chain: &Chain) -> Self {
        let ethereum = Ethereum::start(ETHEREUM_CHAIN_ID, T0_SECS - 3_600).await;
        let (relay, receiver, inbox) = deploy_relay(&ethereum, &chain.provider, chain.accounts[0]).await;
        let beacon = Beacon::default();
        let (url, server) = beacon.serve().await;
        let delivery = tokio::spawn(bridge(ethereum.client.clone(), chain.provider.clone(), inbox, receiver));
        let cfg = relayer_config(&ethereum, ETHEREUM_CHAIN_ID, &chain.proxies, relay, receiver, &url);
        let relayer = crate::seed_relay::task(&cfg, &Metrics::new()).unwrap().expect("the relayer is configured");
        let relayer = tokio::spawn(async move {
            if let Err(e) = relayer.await {
                panic!("the seed relayer stopped: {e:#}");
            }
        });
        Self { ethereum, beacon, tasks: vec![server, delivery, relayer], receiver }
    }

    /// Moves the beacon node and Ethereum to `at`, so the beacon node has finalized the epochs that
    /// ended about 16 minutes before, mines a block of `chain` stamped `at`, and waits until the
    /// relayer has relayed each of `turns`, returning their seeds. Once the last seed is recorded
    /// no ticket waits to be delivered, so the scenario's own timed blocks are not interleaved.
    pub(super) async fn relay(&self, chain: &Chain, at: u64, turns: [u64; 2]) -> [B256; 2] {
        finalize_beacon(&self.ethereum, &self.beacon, at).await;
        chain.mine_at(at).await;
        let mut seeds = [B256::ZERO; 2];
        for (seed, turn) in seeds.iter_mut().zip(turns) {
            *seed = self.wait(chain, turn).await;
        }
        seeds
    }

    /// Turn `turn`'s relayed seed, once the receiver records it.
    pub(super) async fn wait(&self, chain: &Chain, turn: u64) -> B256 {
        crate::test_support::wait_for(&format!("turn {turn}'s relayed seed"), async {
            loop {
                let seed = chain.view(self.receiver, relayedSeedCall { turn }).await;
                if !seed.is_zero() {
                    return seed;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
    }
}

impl Drop for Seeds {
    fn drop(&mut self) {
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}
