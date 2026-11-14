//! The fake chain's operator schedule binding: the schedule its registry names, the registry that
//! schedule records, the schedule a vote contract records, and the turn the registry names the
//! operator by.

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::SolCall;

use crate::custody::evm::BlockRef;
use crate::custody::tests::fakes::{ChainState, REGISTRY};

alloy::sol! {
    function schedule() external view returns (address);
    function rotationState() external view returns (bool enabled, bytes32 release, uint64 generation, uint64 until);
}

/// What the fake chain answers for the schedule binding reads.
#[derive(Clone, Default)]
pub(in crate::custody::tests) struct Schedule {
    /// The schedule `registry_schedule` answers: the zero address, the default, while the registry
    /// names none.
    pub(in crate::custody::tests) named: Address,
    /// The registry `schedule_registry` answers: `None`, the default, where the schedule has no code.
    pub(in crate::custody::tests) registry: Option<Address>,
    /// A vote contract and the schedule its `schedule()` answers, `None` where it has no code.
    pub(in crate::custody::tests) vote: Option<(Address, Option<Address>)>,
    /// When the turn the registry's `rotationState()` names the operator by ends, in seconds; `None`,
    /// the default, while no turn names it.
    pub(in crate::custody::tests) named_turn_until: Option<u64>,
    /// The block of each `rotationState()` read, in order.
    pub(in crate::custody::tests) turn_reads: Vec<BlockRef>,
}

impl ChainState {
    /// A contract call's answer: a vote contract's schedule, the registry's rotation state, or the
    /// vault's and the log heads' epochs.
    /// Error cases:
    /// - any call `Epochs::answer` refuses
    pub(super) fn answer_call(&mut self, at: BlockRef, to: Address, data: &[u8]) -> anyhow::Result<Bytes> {
        match self.schedule.vote {
            Some((vote, recorded)) if to == vote && data == scheduleCall {}.abi_encode() => {
                Ok(recorded.map(|schedule| scheduleCall::abi_encode_returns(&schedule).into()).unwrap_or_default())
            }
            _ if to == REGISTRY && data == rotationStateCall {}.abi_encode() => {
                anyhow::ensure!(self.hash_at(at.number) == at.hash, "block {} is not canonical", at.hash);
                self.schedule.turn_reads.push(at);
                let until = self.schedule.named_turn_until;
                let state = rotationStateReturn {
                    enabled: until.is_some(),
                    release: Default::default(),
                    generation: u64::from(until.is_some()),
                    until: until.unwrap_or_default(),
                };
                Ok(rotationStateCall::abi_encode_returns(&state).into())
            }
            _ => self.epochs.answer(at, to, data),
        }
    }
}
