//! A candidate's registration as the registry records it: its registered release, and whether the
//! timelock approved that release for it, each its own read, so a check sees the release before it reads
//! the approval.

use alloy::primitives::{Address, B256, keccak256};
use alloy::sol_types::SolValue;
use engine_core::types::AccountId;

use crate::standby_key::chain::{AgreedBlock, StandbyDomain, call, candidateOfCall, releaseApprovedCall};

/// `candidate`'s registered release at `block` (`None` while unregistered), and whether the timelock
/// approved it for the candidate. Error cases: a read fails.
pub(crate) async fn registration_at(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    candidate: AccountId,
) -> anyhow::Result<(Option<B256>, bool)> {
    let release = registered_release(block, domain, candidate).await?;
    let approved = match release {
        Some(release) => release_approved(block, domain, candidate, release).await?,
        None => false,
    };
    Ok((release, approved))
}

/// `candidate`'s registered release at `block`, `None` while unregistered. Error cases: the read fails.
pub(crate) async fn registered_release(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    candidate: AccountId,
) -> anyhow::Result<Option<B256>> {
    let candidate = Address::from(candidate.0);
    let registered = call(block, "standby_candidate", domain.registry, candidateOfCall { candidate }).await?;
    Ok((!registered.adminKey.is_zero()).then_some(registered.release))
}

/// Whether the timelock approved `release` for `candidate` at `block`. Error cases: the read fails.
pub(crate) async fn release_approved(
    block: &impl AgreedBlock,
    domain: &StandbyDomain,
    candidate: AccountId,
    release: B256,
) -> anyhow::Result<bool> {
    let approval = keccak256((Address::from(candidate.0), release).abi_encode());
    call(block, "standby_release_approved", domain.registry, releaseApprovedCall { approval }).await
}
