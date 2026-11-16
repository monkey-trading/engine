//! The record a promoted member keeps of its promotion beside its keys, and what the chain records of
//! the promotion it names.

use std::path::Path;

use alloy::primitives::B256;
use anyhow::Context;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};

use crate::standby_key::chain::{
    AgreedBlock, Promotion, StandbyChain, StandbyDomain, in_effect, names_release, promotion_of,
};

/// The record a promoted member keeps of its promotion, beside its keys.
pub(crate) const RECORD: &str = "promotion.json";

/// The promotion as the record carries it: the key it made the vault's signer, what it fixed, and the
/// turn assignment and signer epoch it made, which every later start and the promotion's watch compare
/// with what the chain records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub candidate: AccountId,
    /// The standby key the promotion made the vault's signer.
    pub key: AccountId,
    pub turn: u64,
    pub position: u16,
    pub signer_epoch: u64,
    pub generation: u64,
    pub head_seq: u64,
    pub head_link: B256,
    pub checkpoint_seq: u64,
    pub checkpoint_root: B256,
    /// The block time the promotion was made at, in seconds.
    pub at: u64,
    /// The agreed block the start read the promotion at: a later start or the watch reads no block
    /// below it as lacking the promotion, and finality passes the promotion only at or past it.
    pub block: u64,
    /// The first seq this member sequences under the promotion.
    pub from_seq: u64,
    /// The promotion's watch saw a latest block lacking it: only a final block recording it again
    /// resumes it.
    #[serde(default)]
    pub reorged: bool,
    /// What the promotion's history is being set aside: a start finishes that first.
    #[serde(default)]
    pub quarantining: bool,
}

/// What the chain records of a promotion a record names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// The same promotion, the vault names its key in its epoch, and the registry names this release in
    /// the promotion's generation.
    Stands,
    /// The same promotion, but the vault has left its epoch or retired its key, or the registry names
    /// another release or generation, and a final block at or past the one the start read it at records
    /// it: this release's term ended.
    Ended,
    /// As `Ended` at the latest block, but no final block at or past the start's records the promotion
    /// yet: the record stays, so a reorg that takes it, and what ended it, still finds its commitments.
    Passed,
    /// The latest block lacks it, or records other commitments, and no final block has passed it.
    Unsettled,
    /// The latest agreed block is older than the one the start read the promotion at, as a lagging
    /// provider majority serves: it says nothing of the promotion.
    Behind,
    /// A final block at or past the one the start read it at lacks it: a reorg removed it for good.
    Gone,
}

impl Record {
    /// The record for `promotion` of `candidate`'s `key`, from `from_seq`.
    pub(crate) fn new(candidate: AccountId, key: AccountId, promotion: Promotion, from_seq: u64) -> Self {
        let Promotion {
            turn,
            position,
            signer_epoch,
            generation,
            head_seq,
            head_link,
            checkpoint_seq,
            checkpoint_root,
            at,
            block,
            carried: _,
        } = promotion;
        Self {
            candidate,
            key,
            turn,
            position,
            signer_epoch,
            generation,
            head_seq,
            head_link,
            checkpoint_seq,
            checkpoint_root,
            at,
            block,
            from_seq,
            reorged: false,
            quarantining: false,
        }
    }

    /// The promotion the record names.
    pub(crate) fn promotion(&self) -> Promotion {
        Promotion {
            turn: self.turn,
            position: self.position,
            signer_epoch: self.signer_epoch,
            generation: self.generation,
            head_seq: self.head_seq,
            head_link: self.head_link,
            checkpoint_seq: self.checkpoint_seq,
            checkpoint_root: self.checkpoint_root,
            at: self.at,
            block: self.block,
            carried: self.turn,
        }
    }

    /// The record in `dir`, if one is there.
    /// Error cases:
    /// - the file cannot be read or does not parse
    pub(crate) fn load(dir: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read(dir.join(RECORD)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).context("the promotion record does not parse"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading the promotion record"),
        }
    }

    /// Writes the record durably into `dir`.
    /// Error cases:
    /// - the file cannot be written
    pub(super) fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).context("encoding the promotion record")?;
        crate::tee::sealing::write_replacing(&dir.join(RECORD), &bytes)
    }

    /// Records in `dir`'s record that the promotion's watch saw a latest block lacking it, so no start
    /// resumes it before a final block records it again.
    /// Error cases:
    /// - the record cannot be read or written, or there is none
    pub(crate) fn mark_reorged(dir: &Path) -> anyhow::Result<()> {
        let record = Self::load(dir)?.context("no promotion record")?;
        Self { reorged: true, ..record }.write(dir)
    }

    /// `standing`, as a start reads it: an unsettled promotion is marked reorged in the record in `dir`
    /// first, so a later start that finds it again at the latest block still waits for a final one.
    /// Error cases:
    /// - see `standing`, or the record cannot be written
    pub(crate) async fn standing_at_start(
        &self,
        chain: &impl StandbyChain,
        domain: &StandbyDomain,
        release: B256,
        dir: &Path,
    ) -> anyhow::Result<Standing> {
        let standing = self.standing(chain, domain, release).await?;
        if standing == Standing::Unsettled && !self.reorged {
            Self::mark_reorged(dir)?;
        }
        Ok(standing)
    }

    /// What `chain` records of this promotion in `domain`, for the enclave running `release`: at the
    /// latest agreed block, then, when that lacks it, at the final block.
    /// Error cases:
    /// - a read fails
    pub(crate) async fn standing(
        &self,
        chain: &impl StandbyChain,
        domain: &StandbyDomain,
        release: B256,
    ) -> anyhow::Result<Standing> {
        let promotion = self.promotion();
        let latest = chain.latest().await?;
        let found = promotion_of(&latest, domain, self.key, self.turn, self.position).await?;
        if found.is_some_and(|found| found.same(&promotion)) {
            let held = in_effect(&latest, domain, self.key, &promotion).await?
                && names_release(&latest, domain, release, self.generation).await?;
            if !held {
                drop(latest);
                let finalized = chain.finalized().await?;
                let settled = finalized.number() >= self.block
                    && promotion_of(&finalized, domain, self.key, self.turn, self.position)
                        .await?
                        .is_some_and(|found| found.same(&promotion));
                return Ok(if settled { Standing::Ended } else { Standing::Passed });
            }
            if !self.reorged {
                return Ok(Standing::Stands);
            }
            // A reorg once took it: only a final block's record settles it again.
            drop(latest);
            let finalized = chain.finalized().await?;
            let settled = promotion_of(&finalized, domain, self.key, self.turn, self.position).await?;
            return Ok(match settled.is_some_and(|found| found.same(&promotion)) {
                true => Standing::Stands,
                false => Standing::Unsettled,
            });
        }
        if latest.number() < self.block {
            return Ok(Standing::Behind);
        }
        drop(latest);
        let finalized = chain.finalized().await?;
        let found = promotion_of(&finalized, domain, self.key, self.turn, self.position).await?;
        Ok(match found.is_some_and(|found| found.same(&promotion)) || finalized.number() < self.block {
            true => Standing::Unsettled,
            false => Standing::Gone,
        })
    }
}

#[cfg(test)]
impl Record {
    /// Writes the record into `dir`, for tests outside this module.
    pub(crate) fn write_for_test(&self, dir: &Path) {
        self.write(dir).expect("the promotion record is written");
    }
}
