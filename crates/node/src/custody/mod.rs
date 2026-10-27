//! Collateral custody. USDC sits in a vault contract on an EVM L2. The chain handles custody only
//! and never touches execution:
//! - deposits: from V20, sequence exact quorum-proven receipts on confirmation, lock principal
//!   against withdrawals until Ethereum finality, and reconcile canonical reversals; from V24 each
//!   new receipt carries its funding payer's screening and is credited, left pending, or held
//!   uncredited; older logs retain their legacy credit commands and cursor semantics;
//! - withdrawals: the engine approves, the enclave signs, and the vault pays out against the
//!   enclave signature;
//! - checkpoints: post the signed state roots so anyone can check the enclave against the public
//!   log.
//!
//! The checkpoint service gates withdrawal authorizations; accounts or relayers claim them at the vault.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use anyhow::Context;
use engine_core::Engine;
use engine_core::Fixed;
use engine_core::deposits::LegacyDeposit;
use engine_core::fixed::DECIMALS;
use std::collections::BTreeMap;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::access::AccessClient;
use crate::config::{CustodyProvider, CustodySection};
mod chain;
mod credit;
mod cursor;
mod evm;
mod pending;
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

/// Proves the finality needed to redeliver an old approval before writable recovery.
/// Error cases:
/// - invalid custody configuration, missing quorum or canonical finality evidence
pub(crate) async fn prove_legacy_withdrawals(
    cfg: &CustodySection,
    engine: &Engine,
    ledger: &BTreeMap<String, LegacyDeposit>,
    approvals: &crate::sequencer::legacy::Approvals,
) -> anyhow::Result<crate::sequencer::legacy::Authorizations> {
    let evm = cfg.evm.as_ref().context("legacy EVM finality proof needs custody.evm")?;
    let vault = cfg.vault.parse().context("custody.vault")?;
    DepositWatcher::prove_legacy_withdrawals(
        Quorum::connect(evm, vault)?,
        evm,
        cfg.chain_id,
        vault,
        engine,
        ledger,
        approvals,
    )
    .await
}

/// A connected custody provider, ready to run.
pub struct Custody {
    /// Credits quorum-proven vault deposits and reconciles their principal finality; `None` is the
    /// mock, whose deposits come from the dev endpoint and whose withdrawals are only logged.
    evm: Option<Box<DepositWatcher<Quorum<VaultRpc>>>>,
    access: Option<Arc<AccessClient>>,
}

impl Custody {
    /// Gives the watcher independent screening capacity.
    pub(crate) fn with_access(mut self, access: Option<Arc<AccessClient>>) -> Self {
        if let Some(watcher) = &mut self.evm {
            watcher.access = access.clone();
        }
        self.access = access;
        self
    }
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
            return Ok(Custody { evm: None, access: None });
        }
        let evm = cfg.evm.as_ref().context("custody.provider = \"evm\" needs [custody.evm]")?;
        let vault: Address = cfg.vault.parse().context("custody.vault")?;
        let rpc = Quorum::connect(evm, vault)?;
        let watcher = DepositWatcher::start(rpc, evm, cfg.chain_id, vault, log_seq)?;
        warn!("EVM custody requires finalized checkpoint inclusion before withdrawal authorization");
        Ok(Custody { evm: Some(Box::new(watcher)), access: None })
    }

    /// Runs until the sequencer stops sending withdrawals or the deposit watcher fails.
    /// Error cases:
    /// - the deposit watcher fails (see `DepositWatcher::run`)
    pub async fn run(
        self,
        withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
        tx: mpsc::Sender<Request>,
    ) -> anyhow::Result<()> {
        let retry = pending::run(self.access.as_deref(), tx.clone());
        match self.evm {
            None => tokio::select! {
                () = log_withdrawals(withdrawals) => Ok(()),
                result = retry => result,
            },
            Some(watcher) => tokio::select! {
                () = log_withdrawals(withdrawals) => Ok(()),
                result = watcher.run(tx) => result,
                result = retry => result,
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
