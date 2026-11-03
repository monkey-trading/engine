//! Warns before the checkpoint relayer runs out of gas, which would stop posts and, once
//! checkpoints go stale, refuse deposits.

use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, U256};
use tracing::{debug, info, warn};

use crate::checkpoints::{CheckpointChain, Poster};
use crate::metrics::FloatGauge;

/// What the poster needs to judge the relayer's balance.
pub(crate) struct RelayerGas {
    pub relayer: Address,
    /// Below this many wei, `checkpoint_relayer_low` fires.
    pub min_balance: U256,
    /// Exports the balance in ether.
    pub balance_eth: FloatGauge,
    /// Whether the `checkpoint_relayer_low` episode is open, so only the read that ends it logs
    /// the resolve.
    pub low: AtomicBool,
}

impl<R: CheckpointChain> Poster<R> {
    /// Reads the relayer's balance, exports it, and warns while it is below the configured
    /// minimum. A read that fails changes nothing: the custody providers alert their own loss.
    pub(super) async fn check_relayer_gas(&self) {
        let Some(gas) = &self.relayer_gas else { return };
        let balance = match self.chain.relayer_balance().await {
            Ok(Some(balance)) => balance,
            Ok(None) => return,
            Err(error) => {
                debug!("relayer balance unreadable: {error:#}");
                return;
            }
        };
        let ether = format_ether(balance);
        gas.balance_eth.set(ether.parse().unwrap_or(f64::NAN));
        if balance < gas.min_balance {
            gas.low.store(true, Ordering::Relaxed);
            warn!(
                alert = "checkpoint_relayer_low",
                relayer = %gas.relayer,
                "the relayer holds {ether} ETH, below {}: checkpoint posts stop when it cannot pay for gas",
                format_ether(gas.min_balance)
            );
        } else if gas.low.swap(false, Ordering::Relaxed) {
            info!(alert_resolved = "checkpoint_relayer_low", relayer = %gas.relayer, "the relayer holds {ether} ETH");
        }
    }
}
