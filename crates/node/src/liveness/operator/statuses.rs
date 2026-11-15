//! How the operator asks the members for their statuses and matches them against its log.

use std::collections::{BTreeMap, BTreeSet};

use alloy::primitives::{Address, B256};
use engine_core::types::AccountId;
use futures_util::future::{BoxFuture, join_all};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::liveness::Status;
use crate::liveness::chain::{Member, standby_holder};
use crate::liveness::link::{Asking, Request, verified_status};
use crate::liveness::operator::{Assessment, Holders, Operator, STATUS_TIMEOUT, assess};
use crate::sequencer::Request as SequencerRequest;
use crate::standby_key::chain::StandbyChain;

impl<C: StandbyChain, A: Asking> Operator<C, A> {
    /// Asks each of `others` for its status under signer epoch `epoch`, and weighs the verified
    /// answers against this log. A member that held an assignment of the turn reports, as its own
    /// cut-off tail, only a durable head sealed by its own standby key, or by a key no candidate
    /// registered (the turn's keys), by `schedule`'s holders, cached in `holders`; a tail it followed
    /// from another member's promoted key counts, as does any, while its holder cannot be read.
    pub(super) fn ask<'a>(
        &'a self,
        (epoch, schedule): (u64, Address),
        others: Vec<Member>,
        holders: &'a mut Holders,
    ) -> BoxFuture<'a, Assessment> {
        Box::pin(async move {
            let mut nonce = [0u8; 32];
            if SystemRandom::new().fill(&mut nonce).is_err() {
                warn!("the RNG failed: no status questions this round");
                return Assessment::default();
            }
            let request = Request::Status { epoch, nonce: B256::from(nonce) };
            let mut asked = Vec::with_capacity(others.len());
            for member in &others {
                asked.push(self.status(member.clone(), request.clone(), nonce, epoch));
            }
            let statuses: Vec<(AccountId, Status)> = join_all(asked).await.into_iter().flatten().collect();
            let tip = self.released.borrow().head;
            let mut seqs: Vec<u64> = Vec::new();
            for (_, status) in &statuses {
                seqs.extend([Some(status.durable), status.seen].into_iter().flatten().map(|head| head.seq));
            }
            seqs.retain(|seq| *seq <= tip.seq);
            seqs.sort_unstable();
            seqs.dedup();
            let mut prefixes = BTreeMap::new();
            for seq in seqs {
                match self.prefix_at(seq).await {
                    Ok(prefix) => _ = prefixes.insert(seq, prefix),
                    Err(e) => debug!(seq, "no prefix to match a status against: {e:#}"),
                }
            }
            let mut held = BTreeSet::new();
            for (member, status) in &statuses {
                let foreign = status.sealer != AccountId([0; 20]) && status.sealer != self.key;
                if !foreign || !others.iter().any(|listed| listed.candidate == *member && listed.held) {
                    continue;
                }
                match self.holder(schedule, status.sealer, holders).await {
                    Ok(holder) if holder.is_none_or(|holder| holder == *member) => _ = held.insert(*member),
                    Ok(_) => {}
                    Err(e) => debug!(member = %member, "reading whose key sealed the member's tail failed: {e:#}"),
                }
            }
            assess(&statuses, |seq| prefixes.get(&seq).copied().flatten(), tip, (self.key, &held))
        })
    }

    /// The candidate that registered `key` as a standby key with `schedule`, `None` for none, from
    /// `holders` or read at the latest agreed block and kept there.
    /// Error cases:
    /// - the read fails
    async fn holder(
        &self,
        schedule: Address,
        key: AccountId,
        holders: &mut Holders,
    ) -> anyhow::Result<Option<AccountId>> {
        if let Some(holder) = holders.get(&key) {
            return Ok(*holder);
        }
        let block = self.chain.latest().await?;
        let holder = standby_holder(&block, schedule, key).await?;
        holders.insert(key, holder);
        Ok(holder)
    }

    /// `member`'s status for `request`, once it recovers to the member's promotable standby key over
    /// `nonce` and names `epoch`; `None`, logged, otherwise.
    fn status(
        &self,
        member: Member,
        request: Request,
        nonce: [u8; 32],
        epoch: u64,
    ) -> BoxFuture<'_, Option<(AccountId, Status)>> {
        let deployment = (self.chain_id, self.vault);
        Box::pin(async move {
            let answered = async {
                let key = member.standby.ok_or_else(|| anyhow::anyhow!("no promotable standby key"))?;
                anyhow::ensure!(!member.endpoint.is_empty(), "no registered handover endpoint");
                let reply = tokio::time::timeout(STATUS_TIMEOUT, self.asking.ask(&member.endpoint, &request))
                    .await
                    .map_err(|_| anyhow::anyhow!("no answer within {STATUS_TIMEOUT:?}"))??;
                let status = verified_status(reply, deployment, member.candidate, key, &nonce)?;
                anyhow::ensure!(status.epoch == epoch, "the status names epoch {}", status.epoch);
                anyhow::Ok((member.candidate, status))
            };
            answered.await.inspect_err(|e| debug!(member = %member.candidate, "no status: {e:#}")).ok()
        })
    }

    /// This log's cumulative prefix after the command at `seq`, while the sequencer retains it.
    /// Error cases:
    /// - the sequencer has stopped
    async fn prefix_at(&self, seq: u64) -> anyhow::Result<Option<B256>> {
        let sequencer = self.sequencer.upgrade().ok_or_else(|| anyhow::anyhow!("the sequencer has stopped"))?;
        let (reply, answer) = oneshot::channel();
        sequencer
            .send(SequencerRequest::PrefixAt { seq, reply })
            .await
            .map_err(|_| anyhow::anyhow!("the sequencer has stopped"))?;
        answer.await.map_err(|_| anyhow::anyhow!("the sequencer dropped the question"))
    }
}
