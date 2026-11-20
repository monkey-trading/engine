//! What a cache carries of the withdrawal approvals its engine holds, which only the log before its
//! checkpoint shows: each owed approval's place in payout order and its latest authorization, and the
//! checkpoints an authorization after it can name.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, ensure};
use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};

use crate::log::Checkpoint;
use crate::sequencer::SignedWithdrawal;

/// The approvals a node still owed a delivery for at a checkpoint, in payout order. An approval the
/// engine holds that none of them names was delivered: paid at a finalized block, or, under mock
/// custody, signed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Payouts {
    pub owed: Vec<Owed>,
    /// The roots of the checkpoints before this one at which an owed approval's proof was taken or
    /// is being taken: an authorization signed after the checkpoint names one of them.
    pub roots: BTreeMap<u64, B256>,
}

/// An owed approval, with its event's index in the command that approved it and the latest
/// authorization the log holds for it, if one was signed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Owed {
    pub account: AccountId,
    pub nonce: u64,
    pub event_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<SignedWithdrawal>,
}

/// The payout state a replay of the log up to a cache's checkpoint would hold, as replay continues
/// from it.
#[derive(Default)]
pub(crate) struct Restored {
    /// Each owed approval with its event index, in payout order.
    pub payout_order: Vec<((AccountId, u64), usize)>,
    pub authorized: BTreeMap<(AccountId, u64), SignedWithdrawal>,
    /// Every approval a withdrawal entry signed: all the engine holds but the owed ones never signed.
    pub signed: BTreeSet<(AccountId, u64)>,
    /// Approvals delivered before the checkpoint, which `payout_order` leaves out.
    pub delivered: usize,
    /// The roots an authorization after the checkpoint can name, the checkpoint's own included.
    pub roots: BTreeMap<u64, B256>,
}

impl Payouts {
    /// Checks the owed approvals against `engine`, the state at `checkpoint`, and gives the payout
    /// state replay continues from.
    /// Error cases:
    /// - an owed approval is not one the engine holds, is listed twice, or is out of payout order
    /// - a latest authorization is not a typed authorization of its approval
    /// - a root is not before the checkpoint, or the checkpoint's root does not parse
    pub(crate) fn restore(&self, engine: &Engine, checkpoint: &Checkpoint) -> anyhow::Result<Restored> {
        let mut restored = Restored::default();
        let (mut owed_keys, mut unsigned, mut previous) = (BTreeSet::new(), BTreeSet::new(), None);
        for owed in &self.owed {
            let key = (owed.account, owed.nonce);
            let approval = engine
                .withdrawal_approval(owed.account, owed.nonce)
                .context("cached payout is not an engine approval")?;
            ensure!(owed_keys.insert(key), "cached payout is listed twice");
            let order = (approval.approved_seq, owed.event_index);
            ensure!(previous < Some(order), "cached payouts are out of payout order");
            previous = Some(order);
            match &owed.latest {
                Some(latest) => {
                    ensure!(
                        (latest.account, latest.nonce, latest.amount, latest.approved_seq)
                            == (approval.account, approval.nonce, approval.amount, Some(approval.approved_seq))
                            && latest.authorization.is_some(),
                        "cached authorization is not its approval's"
                    );
                    restored.authorized.insert(key, latest.clone());
                }
                None => _ = unsigned.insert(key),
            }
            restored.payout_order.push((key, owed.event_index));
        }
        let approvals = engine.withdrawal_approvals().map(|approval| (approval.account, approval.nonce));
        restored.signed = approvals.filter(|key| !unsigned.contains(key)).collect();
        restored.delivered = engine.withdrawal_approvals().count() - self.owed.len();
        ensure!(self.roots.keys().all(|seq| *seq < checkpoint.seq), "cached root is not before its checkpoint");
        restored.roots = self.roots.clone();
        restored.roots.insert(checkpoint.seq, checkpoint.root.parse().context("cache checkpoint root")?);
        Ok(restored)
    }
}
