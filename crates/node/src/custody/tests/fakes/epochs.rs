//! The fake chain's signer history and earlier epochs' log heads, answered as `contract_call`s.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, Bytes};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::bail;

use super::VAULT;
use crate::custody::evm::BlockRef;

/// The LogHeads contract the fake chain answers `heads` for.
pub(in crate::custody::tests) const LOG_HEADS: Address = Address::repeat_byte(0x11);

sol! {
    function heads(uint64 epoch) external view returns (uint64 seq, bytes32 link);
    function signerAtEpoch(uint64 epoch) external view returns (address signer);
}

/// Each epoch's signer and, for the epochs before the current one, the head it holds; and every
/// `(block, epoch)` a call read, so a test pins the walk at the agreed block.
#[derive(Default)]
pub(in crate::custody::tests) struct Epochs {
    pub(in crate::custody::tests) by_epoch: BTreeMap<u64, (Address, Option<(u64, B256)>)>,
    pub(in crate::custody::tests) reads: Vec<(u64, u64)>,
}

impl Epochs {
    /// The return data of the vault's `signerAtEpoch` or the LogHeads contract's `heads` called on
    /// `to` at `at`.
    /// Error cases:
    /// - any other call, or one on another contract
    pub(super) fn answer(&mut self, at: BlockRef, to: Address, data: &[u8]) -> anyhow::Result<Bytes> {
        if let Ok(call) = signerAtEpochCall::abi_decode(data)
            && to == VAULT
        {
            self.reads.push((at.number, call.epoch));
            let signer = self.by_epoch.get(&call.epoch).map_or(Address::ZERO, |(signer, _)| *signer);
            return Ok(signerAtEpochCall::abi_encode_returns(&signer).into());
        }
        if let Ok(call) = headsCall::abi_decode(data)
            && to == LOG_HEADS
        {
            self.reads.push((at.number, call.epoch));
            let (seq, link) = self.by_epoch.get(&call.epoch).and_then(|(_, head)| *head).unwrap_or_default();
            return Ok(headsCall::abi_encode_returns(&headsReturn { seq, link }).into());
        }
        bail!("the fake chain answers no such call")
    }
}
