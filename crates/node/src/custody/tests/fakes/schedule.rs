//! The fake chain's operator schedule binding: the schedule its registry names, the registry that
//! schedule records, and the schedule a vote contract records.

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;

use crate::custody::evm::BlockRef;
use crate::custody::tests::fakes::ChainState;

alloy::sol! {
    function schedule() external view returns (address);
}

/// What the fake chain answers for the schedule binding reads.
#[derive(Clone, Copy, Default)]
pub(in crate::custody::tests) struct Schedule {
    /// The schedule `registry_schedule` answers: the zero address, the default, while the registry
    /// names none.
    pub(in crate::custody::tests) named: Address,
    /// The registry `schedule_registry` answers: `None`, the default, where the schedule has no code.
    pub(in crate::custody::tests) registry: Option<Address>,
    /// A vote contract and the schedule its `schedule()` answers, `None` where it has no code.
    pub(in crate::custody::tests) vote: Option<(Address, Option<Address>)>,
}

impl ChainState {
    /// A contract call's answer: a vote contract's schedule, or the vault's and the log heads' epochs.
    /// Error cases:
    /// - any call `Epochs::answer` refuses
    pub(super) fn answer_call(&mut self, at: BlockRef, to: Address, data: &[u8]) -> anyhow::Result<Bytes> {
        match self.schedule.vote {
            Some((vote, recorded)) if to == vote && data == scheduleCall {}.abi_encode() => {
                Ok(recorded.map(|schedule| scheduleCall::abi_encode_returns(&schedule).into()).unwrap_or_default())
            }
            _ => self.epochs.answer(at, to, data),
        }
    }
}
