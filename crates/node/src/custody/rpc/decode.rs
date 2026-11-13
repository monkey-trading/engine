//! Decoding the vault's deposit logs, the staking and registry events, and the deposit count.

use alloy::primitives::Bytes;
use alloy::rpc::types::Log;
use alloy::sol_types::{SolCall, SolEvent};
use anyhow::{Context, bail};
use engine_core::types::AccountId;

use crate::custody::evm::{Beneficiary, StakingEvent, StakingEventKind, VaultDeposit};
use crate::custody::rpc::{
    Deposited, HouseDeposited, Staked, SurplusBooked, UnstakeCompleted, VaultRpc, depositCountCall, legacy,
};

impl VaultRpc {
    /// One vault deposit from its log.
    /// Error cases:
    /// - the log is removed, from another address, lacks its block, block hash, transaction, or
    ///   index, or does not decode as either deposit event
    pub(in crate::custody) fn decode(&self, log: &Log) -> anyhow::Result<VaultDeposit> {
        if log.removed || log.address() != self.vault {
            bail!("unexpected log from {} (removed: {})", log.address(), log.removed);
        }
        let block = log.block_number.context("log without a block number")?;
        let block_hash = log.block_hash.context("log without a block hash")?;
        let tx_hash = log.transaction_hash.context("log without a transaction hash")?;
        let log_index = log.log_index.context("log without a log index")?;
        let (id, beneficiary, units, payer) = match log.topic0() {
            Some(&Deposited::SIGNATURE_HASH) => {
                let event = Deposited::decode_log(&log.inner).context("decoding Deposited")?.data;
                (
                    event.id,
                    Beneficiary::Trader(AccountId(event.account.into())),
                    event.amount,
                    Some(AccountId(event.from.into())),
                )
            }
            Some(&legacy::Deposited::SIGNATURE_HASH) => {
                let event = legacy::Deposited::decode_log(&log.inner).context("decoding legacy Deposited")?.data;
                (event.id, Beneficiary::Trader(AccountId(event.account.into())), event.amount, None)
            }
            Some(&HouseDeposited::SIGNATURE_HASH) => {
                let event = HouseDeposited::decode_log(&log.inner).context("decoding HouseDeposited")?.data;
                (event.id, Beneficiary::House, event.amount, Some(AccountId(event.from.into())))
            }
            // A surplus books existing holdings, not the caller's funds: it names no funding address,
            // so screening holds it as unattributable rather than crediting it under the booker.
            Some(&SurplusBooked::SIGNATURE_HASH) => {
                let event = SurplusBooked::decode_log(&log.inner).context("decoding SurplusBooked")?.data;
                (event.id, Beneficiary::House, event.amount, None)
            }
            other => bail!("unexpected event {other:?}"),
        };
        let id = u64::try_from(id).context("deposit id beyond u64")?;
        Ok(VaultDeposit { block, block_hash, tx_hash, log_index, id, beneficiary, units, payer })
    }

    /// A staking or registry event's fields; a staking event's `account` is its only indexed topic.
    /// Error cases:
    /// - the log is removed, comes from neither configured contract, lacks its position, or does
    ///   not decode
    pub(in crate::custody) fn decode_staking(&self, log: &Log) -> anyhow::Result<StakingEvent> {
        let staking = self.staking.context("a staking event without a configured staking contract")?;
        let from_registry = self.registry == Some(log.address());
        let from_schedule = self.schedule == Some(log.address());
        let from_vote = self.vote == Some(log.address());
        if log.removed || (log.address() != staking && !from_registry && !from_schedule && !from_vote) {
            bail!("unexpected log from {} (removed: {})", log.address(), log.removed);
        }
        let block = log.block_number.context("log without a block number")?;
        let block_hash = log.block_hash.context("log without a block hash")?;
        let tx_hash = log.transaction_hash.context("log without a transaction hash")?;
        let log_index = log.log_index.context("log without a log index")?;
        let units = |units| {
            crate::custody::staking::monkey_units(units)
                .with_context(|| format!("staking event amount {units} exceeds u128"))
        };
        let kind = match log.topic0() {
            _ if from_registry => crate::custody::registry::decode(log)?,
            _ if from_schedule => crate::custody::registry::decode_schedule(log)?,
            _ if from_vote => crate::custody::registry::decode_vote(log)?,
            Some(&Staked::SIGNATURE_HASH) => {
                let event = Staked::decode_log(&log.inner).context("decoding Staked")?.data;
                StakingEventKind::Staked { account: AccountId(event.account.into()), amount: units(event.amount)? }
            }
            Some(&UnstakeCompleted::SIGNATURE_HASH) => {
                let event = UnstakeCompleted::decode_log(&log.inner).context("decoding UnstakeCompleted")?.data;
                StakingEventKind::UnstakeCompleted {
                    account: AccountId(event.account.into()),
                    amount: units(event.amount)?,
                }
            }
            other => bail!("unexpected event {other:?}"),
        };
        Ok(StakingEvent { block, block_hash, tx_hash, log_index, kind })
    }
}

/// The deposit count from `depositCount()`'s return data. Empty data means no code at that block:
/// the vault was deployed later, since the identity check found it at the head, so it holds none.
/// Error cases:
/// - the data does not decode, or the count exceeds `u64`
pub(in crate::custody) fn decode_count(out: &Bytes) -> anyhow::Result<u64> {
    if out.is_empty() {
        return Ok(0);
    }
    let count = depositCountCall::abi_decode_returns(out).context("decoding depositCount")?;
    u64::try_from(count).context("deposit count beyond u64")
}
