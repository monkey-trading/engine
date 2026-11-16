//! The operator's read of the chain at the latest agreed block: the vault's registry and schedule, the
//! view under them, and, while a turn names the operator, its set's members.

use alloy::primitives::Address;
use engine_core::types::AccountId;
use futures_util::future::BoxFuture;

use crate::liveness::chain::{
    LivenessDomain, Member, View, first_turnable, members, set_effective, vault_signer, view,
};
use crate::liveness::link::Asking;
use crate::liveness::operator::{Operator, SetId};
use crate::seed_relay::schedule::next_boundary;
use crate::standby_key::chain::{AgreedBlock, StandbyChain, StandbyDomain};

/// One read of the chain at the latest agreed block.
pub(super) enum Read {
    /// The vault names no registry, or the registry no schedule, at a block of time `time`, where it
    /// names `signer`.
    Unscheduled { time: u64, signer: AccountId },
    /// The view under `schedule`, and, while a turn names the operator, its set's members; otherwise
    /// the first boundary a drawn turn could start at (`turnable`).
    Scheduled { schedule: Address, view: Box<View>, listed: Option<Vec<Member>>, turnable: u64 },
}

impl<C: StandbyChain, A: Asking> Operator<C, A> {
    /// The vault's registry and schedule at the latest agreed block, the view under them, and, while
    /// a turn names the operator, its set's members: `cached` when it lists that set of that schedule,
    /// read at the block otherwise, each with no promotable standby key given the key it attests
    /// (`attested`).
    /// Error cases:
    /// - a read fails
    pub(super) fn read(&self, cached: Option<(SetId, Vec<Member>)>) -> BoxFuture<'_, anyhow::Result<Read>> {
        Box::pin(async move {
            let block = self.chain.latest().await?;
            let Some(standby) = StandbyDomain::at_if_scheduled(&block, self.chain_id, self.vault).await? else {
                let signer = vault_signer(&block, self.vault).await?;
                return Ok(Read::Unscheduled { time: block.time(), signer });
            };
            let domain = LivenessDomain { standby, log_heads: self.log_heads };
            let view = view(&block, &domain).await?;
            let listed = match (view.named.filter(|_| view.turn_names_operator()), cached) {
                (Some(named), Some((id, listed))) if id == (standby.schedule, named.set, named.turn) => Some(listed),
                (Some(named), _) => {
                    let listed = members(&block, &standby, (named.set, named.turn, named.prior, named.source)).await?;
                    Some(self.attested(&block, &standby, listed).await)
                }
                (None, _) => None,
            };
            let turnable = match &listed {
                Some(_) => next_boundary(view.time),
                None => first_turnable(view.time, set_effective(&block, standby.schedule).await?),
            };
            Ok(Read::Scheduled { schedule: standby.schedule, view: Box::new(view), listed, turnable })
        })
    }
}
