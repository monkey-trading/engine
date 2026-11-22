//! A member with no promotable standby key, as after a promotion consumed its registration or a release
//! move voided it: until its candidate registers the new key, the member's statuses count when they
//! recover to the key whose `engine.standby.v1` attestation this enclave verified, as a vouch checks it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use alloy::primitives::B256;
use anyhow::{Context, anyhow, bail, ensure};
use engine_core::types::AccountId;
use futures_util::future::join_all;
use tracing::{debug, warn};

use crate::liveness::chain::{Member, standby_holder};
use crate::liveness::link::{Asking, Reply, Request, describe};
use crate::liveness::operator::{Operator, STATUS_TIMEOUT};
use crate::standby_key::chain::{AgreedBlock, StandbyChain, StandbyDomain, registered_release, release_approved};
use crate::standby_key::vouch::admit;

/// What a failed check read of the candidate's registration before it failed: its registered release,
/// and whether the timelock approved that release once that was read. Every failure after the
/// registration read carries it, so a kept key whose release it shows moved is refuted whatever read
/// fails after it.
#[derive(Debug)]
struct Registration {
    registered: Option<B256>,
    approved: Option<bool>,
}

impl std::fmt::Display for Registration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.registered {
            Some(release) => write!(f, "the candidate's registered release is {release}")?,
            None => write!(f, "the candidate is not registered")?,
        }
        match self.approved {
            Some(true) => write!(f, ", approved"),
            Some(false) => write!(f, ", not approved"),
            None => Ok(()),
        }
    }
}

impl Registration {
    /// Whether what the check read refutes a kept key that proved `release`.
    fn moved(&self, release: B256) -> bool {
        self.registered != Some(release) || self.approved == Some(false)
    }
}

/// The attested keys this enclave verified, by candidate, with the release each quote proved, and the
/// candidates whose latest re-check failed: a failed re-check keeps the key until a check refutes it.
#[derive(Default)]
pub(crate) struct Kept(Mutex<Held>);

#[derive(Default)]
struct Held {
    keys: BTreeMap<AccountId, (AccountId, B256)>,
    failing: BTreeSet<AccountId>,
}

impl Kept {
    /// Whether this is the first failed re-check of `candidate`'s key since one last passed.
    pub(super) fn first_failure(&self, candidate: AccountId) -> bool {
        self.with(|held| held.failing.insert(candidate))
    }

    fn with<T>(&self, f: impl FnOnce(&mut Held) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl<C: StandbyChain, A: Asking> Operator<C, A> {
    /// `listed` at `block`, each other member with no promotable standby key given the key its member
    /// link attests, once this enclave verifies it (`attested_key`); one it cannot verify keeps the key
    /// an earlier read verified, until a check refutes it (`refuted`), and otherwise none, and its
    /// statuses count for nothing. A re-check that fails logs at `warn` once until one passes again.
    pub(super) async fn attested(
        &self,
        block: &impl AgreedBlock,
        domain: &StandbyDomain,
        listed: Vec<Member>,
    ) -> Vec<Member> {
        let attest = |mut member: Member| async move {
            if member.standby.is_none() && !member.endpoint.is_empty() && member.admin_key != self.admin {
                match self.attested_key(block, domain, &member).await {
                    Ok((key, release)) => {
                        debug!(member = %member.candidate, %key, "taking the member's statuses on its attested key");
                        self.kept.with(|held| {
                            held.keys.insert(member.candidate, (key, release));
                            held.failing.remove(&member.candidate);
                        });
                        member.standby = Some(key);
                    }
                    Err(e) => {
                        let held = self.kept.with(|held| held.keys.get(&member.candidate).copied());
                        match held {
                            None => {
                                debug!(member = %member.candidate, "no attested standby key to take statuses on: {e:#}")
                            }
                            Some((key, release))
                                if !e.downcast_ref::<Registration>().is_some_and(|r| r.moved(release))
                                    && !self.refuted(block, domain, &member, (key, release)).await =>
                            {
                                if self.kept.first_failure(member.candidate) {
                                    warn!(member = %member.candidate, %key, "re-checking the member's attested standby key failed, so the key stays until a check refutes it: {e:#}");
                                }
                                member.standby = Some(key);
                            }
                            Some(_) => {
                                debug!(member = %member.candidate, "the attested standby key is refuted: {e:#}");
                                self.kept.with(|held| {
                                    held.keys.remove(&member.candidate);
                                    held.failing.remove(&member.candidate);
                                });
                            }
                        }
                    }
                }
            }
            member
        };
        join_all(listed.into_iter().map(attest)).await
    }

    /// Whether `block` refutes the attested `key` of `member`, which proved `release`: the schedule has
    /// taken it, or its candidate's registered release is another, or no longer approved. The registered
    /// release is compared before the approval is read, so one that moved refutes the key whatever read
    /// fails after it; a read that fails refutes nothing.
    async fn refuted(
        &self,
        block: &impl AgreedBlock,
        domain: &StandbyDomain,
        member: &Member,
        (key, release): (AccountId, B256),
    ) -> bool {
        let moved = match registered_release(block, domain, member.candidate).await {
            Ok(registered) if registered != Some(release) => true,
            Ok(_) => release_approved(block, domain, member.candidate, release).await.is_ok_and(|approved| !approved),
            Err(_) => false,
        };
        moved || standby_holder(block, domain.schedule, key).await.is_ok_and(|holder| holder.is_some())
    }

    /// The standby key `member`'s link attests, once its `engine.standby.v1` quote verifies at trusted
    /// time as a vouch's does (`vouch::admit`), names the member, and proves the release its candidacy
    /// registered, approved for it; and the schedule never took the key, so a key a promotion consumed
    /// or a release move voided never counts again. Every failure after the registered release is read
    /// carries what the check read of the registration (`Registration`).
    /// Error cases:
    /// - this enclave has no sealed platform or trusted time sample to verify against
    /// - the member does not answer within `STATUS_TIMEOUT`, answers no attestation, or a check fails
    /// - a read fails
    async fn attested_key(
        &self,
        block: &impl AgreedBlock,
        domain: &StandbyDomain,
        member: &Member,
    ) -> anyhow::Result<(AccountId, B256)> {
        let (provider, cpu_models, platform) = self.platform.as_ref().context("this enclave has no sealed platform")?;
        let at = self.trusted.since_sample_ms().context("no trusted time sample to verify the quote at")?;
        let asked = tokio::time::timeout(STATUS_TIMEOUT, self.asking.ask(&member.endpoint, &Request::Attestation));
        let reply = asked.await.map_err(|_| anyhow::anyhow!("no answer within {STATUS_TIMEOUT:?}"))??;
        let Reply::Attestation { request } = reply else { bail!("no attestation: {}", describe(&reply)) };
        ensure!(request.candidate == member.candidate, "the attestation is candidate {}'s", request.candidate);
        let quoted = B256::from(admit(&request, domain, (*provider, cpu_models, platform), at / 1000)?.id());
        let registered = registered_release(block, domain, member.candidate).await?;
        let read = |approved| Registration { registered, approved };
        if registered != Some(quoted) {
            return Err(anyhow!("the quote proves release {quoted}, not the registered one").context(read(None)));
        }
        let approved = release_approved(block, domain, member.candidate, quoted).await.context(read(None))?;
        if !approved {
            return Err(anyhow!("the quote's release {quoted} is not approved").context(read(Some(false))));
        }
        let holder = standby_holder(block, domain.schedule, request.key).await.context(read(Some(true)))?;
        if holder.is_some() {
            return Err(anyhow!("the schedule has taken standby key {}", request.key).context(read(Some(true))));
        }
        Ok((request.key, quoted))
    }
}
