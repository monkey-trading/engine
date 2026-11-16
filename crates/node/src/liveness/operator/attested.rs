//! A member with no promotable standby key, as after a promotion consumed its registration or a release
//! move voided it: until its candidate registers the new key, the member's statuses count when they
//! recover to the key whose `engine.standby.v1` attestation this enclave verified, as a vouch checks it.

use anyhow::{Context, bail, ensure};
use engine_core::types::AccountId;
use futures_util::future::join_all;
use tracing::debug;

use crate::liveness::chain::{Member, standby_holder};
use crate::liveness::link::{Asking, Reply, Request, describe};
use crate::liveness::operator::{Operator, STATUS_TIMEOUT};
use crate::standby_key::chain::{AgreedBlock, StandbyChain, StandbyDomain, vouch_view_at};
use crate::standby_key::vouch::admit;

impl<C: StandbyChain, A: Asking> Operator<C, A> {
    /// `listed` at `block`, each other member with no promotable standby key given the key its member
    /// link attests, once this enclave verifies it (`attested_key`); one it cannot verify keeps none, and
    /// its statuses count for nothing.
    pub(super) async fn attested(
        &self,
        block: &impl AgreedBlock,
        domain: &StandbyDomain,
        listed: Vec<Member>,
    ) -> Vec<Member> {
        let attest = |mut member: Member| async move {
            if member.standby.is_none() && !member.endpoint.is_empty() && member.admin_key != self.admin {
                match self.attested_key(block, domain, &member).await {
                    Ok(key) => {
                        debug!(member = %member.candidate, %key, "taking the member's statuses on its attested key");
                        member.standby = Some(key);
                    }
                    Err(e) => debug!(member = %member.candidate, "no attested standby key to take statuses on: {e:#}"),
                }
            }
            member
        };
        join_all(listed.into_iter().map(attest)).await
    }

    /// The standby key `member`'s link attests, once its `engine.standby.v1` quote verifies at trusted
    /// time as a vouch's does (`vouch::admit`), names the member, and proves the release its candidacy
    /// registered, approved for it; and the schedule never took the key, so a key a promotion consumed
    /// or a release move voided never counts again.
    /// Error cases:
    /// - this enclave has no sealed platform or trusted time sample to verify against
    /// - the member does not answer within `STATUS_TIMEOUT`, answers no attestation, or a check fails
    /// - a read fails
    async fn attested_key(
        &self,
        block: &impl AgreedBlock,
        domain: &StandbyDomain,
        member: &Member,
    ) -> anyhow::Result<AccountId> {
        let (provider, platform) = self.platform.as_ref().context("this enclave has no sealed platform")?;
        let at = self.trusted.since_sample_ms().context("no trusted time sample to verify the quote at")?;
        let asked = tokio::time::timeout(STATUS_TIMEOUT, self.asking.ask(&member.endpoint, &Request::Attestation));
        let reply = asked.await.map_err(|_| anyhow::anyhow!("no answer within {STATUS_TIMEOUT:?}"))??;
        let Reply::Attestation { request } = reply else { bail!("no attestation: {}", describe(&reply)) };
        ensure!(request.candidate == member.candidate, "the attestation is candidate {}'s", request.candidate);
        let release = admit(&request, domain, (*provider, platform), at / 1000)?;
        let view = vouch_view_at(block, domain, member.candidate, None).await?;
        ensure!(
            view.release.is_some_and(|registered| registered.0 == release.id()) && view.approved,
            "the quote proves release 0x{}, not the candidate's registered, approved one",
            hex::encode(release.id())
        );
        let holder = standby_holder(block, domain.schedule, request.key).await?;
        ensure!(holder.is_none(), "the schedule has taken standby key {}", request.key);
        Ok(request.key)
    }
}
