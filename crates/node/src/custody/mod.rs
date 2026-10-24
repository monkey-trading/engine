//! Collateral custody. USDC sits in a vault contract on an EVM L2. The chain handles custody only
//! and never touches execution:
//! - deposits: watch the vault's deposit events, wait for finality, then sequence
//!   `Command::Deposit` (or `Command::HouseCapital` for a deposit to the house) with the
//!   transaction hash and log index as its reference;
//! - withdrawals: the engine approves, the enclave signs, and the vault pays out against the
//!   enclave signature;
//! - checkpoints: post the signed state roots so anyone can check the enclave against the public
//!   log.
//!
//! The checkpoint service gates withdrawal authorizations; accounts or relayers claim them at the vault.

use alloy::primitives::{Address, U256};
use anyhow::Context;
use engine_core::Fixed;
use engine_core::fixed::DECIMALS;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::{CustodyProvider, CustodySection};
mod chain;
mod credit;
mod cursor;
mod evm;
use crate::custody::evm::DepositWatcher;
mod quorum;
use crate::custody::quorum::Quorum;
mod rpc;
use crate::custody::rpc::VaultRpc;
mod sizing;
mod stall;
mod watchdog;
use crate::sequencer::{Request, SignedWithdrawal};

/// USDC's decimals on every chain Circle issues it on.
const USDC_DECIMALS: u32 = 6;

/// A connected custody provider, ready to run.
pub enum Custody {
    /// No chain: deposits come from the dev endpoint and withdrawals are only logged.
    Mock,
    /// Credits final vault deposits from an EVM chain.
    Evm(Box<DepositWatcher<Quorum<VaultRpc>>>),
}

impl Custody {
    /// Sets up the configured provider without touching the network: the deposit watcher checks
    /// the providers' chain and the vault's token on every poll. `log_seq` is the last seq of
    /// the recovered log, which the deposit cursor must not have read past.
    /// Error cases:
    /// - EVM custody without its `[custody.evm]` section, which config validation rules out
    /// - `custody.vault` or the RPC URL does not parse
    /// - the deposit cursor is unreadable, belongs to another chain or vault, or is ahead of the
    ///   log (see `Cursor::load`)
    pub fn connect(cfg: &CustodySection, log_seq: Option<u64>) -> anyhow::Result<Self> {
        if cfg.provider == CustodyProvider::Mock {
            return Ok(Custody::Mock);
        }
        let evm = cfg.evm.as_ref().context("custody.provider = \"evm\" needs [custody.evm]")?;
        let vault: Address = cfg.vault.parse().context("custody.vault")?;
        let rpc = Quorum::connect(evm, vault)?;
        let watcher = DepositWatcher::start(rpc, evm, cfg.chain_id, vault, log_seq)?;
        warn!("EVM custody requires finalized checkpoint inclusion before withdrawal authorization");
        Ok(Custody::Evm(Box::new(watcher)))
    }

    /// Runs until the sequencer stops sending withdrawals or the deposit watcher fails.
    /// Error cases:
    /// - the deposit watcher fails (see `DepositWatcher::run`)
    pub async fn run(
        self,
        withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
        tx: mpsc::Sender<Request>,
    ) -> anyhow::Result<()> {
        match self {
            Custody::Mock => {
                log_withdrawals(withdrawals).await;
                Ok(())
            }
            Custody::Evm(watcher) => tokio::select! {
                () = log_withdrawals(withdrawals) => Ok(()),
                result = watcher.run(tx) => result,
            },
        }
    }
}

/// Logs each signed withdrawal until the sequencer goes away.
async fn log_withdrawals(mut withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>) {
    while let Some(w) = withdrawals.recv().await {
        info!(account = %w.account, amount = %w.amount, nonce = w.nonce, "withdrawal authorization available for vault claim");
    }
}

/// USDC base units as `Fixed`, exactly: `Fixed` carries more decimals than USDC, so no precision
/// is lost. `None` only when the value is beyond what `Fixed` can hold; the engine bounds the rest.
pub(crate) fn usdc_to_fixed(units: U256) -> Option<Fixed> {
    let scale = 10i128.pow(DECIMALS - USDC_DECIMALS);
    i128::try_from(units).ok()?.checked_mul(scale).map(Fixed::from_raw)
}

#[cfg(test)]
mod tests;
