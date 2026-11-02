//! MONKEY and USDC claim proofs at the checkpoint the vault has accepted.
//!
//! The vault checks a claim against its latest posted root, and the engine's tree is only ever at
//! the newest checkpoint, which runs ahead of the vault while posting lags. So the sequencer keeps,
//! for the accounts that asked for a proof lately, the proof each new checkpoint gives them, until
//! the vault has accepted a later one. An account is watched from its first request, so that first
//! answer is the newest proof and not yet claimable.

use std::collections::BTreeMap;

use engine_core::Engine;
use engine_core::emissions::TokenUnits;
use engine_core::merkle::{DistributionProof, EntitlementProof};
use engine_core::staking::UsdcUnits;
use engine_core::types::AccountId;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Accounts whose proofs each checkpoint retains; the least recently asked leaves first.
const MAX_WATCHED: usize = 128;
/// Retained proofs per kind, a ceiling: the posting queue's 64 checkpoints, the accepted one, and the
/// newest, though each checkpoint waits for the last to be final, so few are held at once.
const MAX_HELD: usize = MAX_WATCHED * 66;

/// A cumulative claim proof, as the vault's claim takes it.
pub trait Proved: Clone {
    /// The cumulative amount the proof names.
    type Amount: Copy + Serialize + DeserializeOwned;
    fn seq(&self) -> u64;
    fn entitled(&self) -> Self::Amount;
}

impl Proved for EntitlementProof {
    type Amount = TokenUnits;
    fn seq(&self) -> u64 {
        self.seq
    }
    fn entitled(&self) -> TokenUnits {
        self.entitled
    }
}

impl Proved for DistributionProof {
    type Amount = UsdcUnits;
    fn seq(&self) -> u64 {
        self.seq
    }
    fn entitled(&self) -> UsdcUnits {
        self.entitled
    }
}

/// A claim proof and what it is proven against. The proof's fields come first and unchanged; the
/// rest says whether the vault can check it now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "P: Serialize", deserialize = "P: DeserializeOwned"))]
pub struct ClaimAnswer<P: Proved> {
    #[serde(flatten)]
    pub proof: P,
    /// The latest checkpoint the node has seen the vault accept, `null` before it has seen one.
    pub accepted_seq: Option<u64>,
    /// The newest checkpoint the node has created.
    pub newest_seq: u64,
    /// The account's cumulative amount at the newest checkpoint: what the proof will name once the
    /// vault accepts that checkpoint. Never below the proof's own `entitled`.
    pub newest_entitled: P::Amount,
    /// Whether the proof is against the checkpoint the vault last accepted, so a claim can land
    /// now. Otherwise the claim waits for the vault to accept `newest_seq`.
    pub claimable: bool,
}

type Held<P> = BTreeMap<(u64, AccountId), P>;

/// The watched accounts and the proofs retained for them.
#[derive(Default)]
pub(super) struct Claims {
    watched: BTreeMap<AccountId, u64>,
    asked: u64,
    accepted: Option<u64>,
    entitlements: Held<EntitlementProof>,
    distributions: Held<DistributionProof>,
}

impl Claims {
    /// Notes the checkpoint the vault accepted and drops the proofs of the ones before it.
    pub(super) fn accept(&mut self, posted: u64) {
        self.accepted = Some(self.accepted.map_or(posted, |known| known.max(posted)));
        let accepted = self.accepted.unwrap_or(posted);
        self.entitlements = self.entitlements.split_off(&(accepted, AccountId([0; 20])));
        self.distributions = self.distributions.split_off(&(accepted, AccountId([0; 20])));
    }

    /// Keeps each watched account's proofs at the checkpoint just created.
    pub(super) fn capture(&mut self, engine: &Engine) {
        for &account in self.watched.keys() {
            Self::keep(&mut self.entitlements, engine.entitlement_proof(account), account);
            Self::keep(&mut self.distributions, engine.distribution_proof(account), account);
        }
    }

    /// The account's MONKEY proof, at the accepted checkpoint when one is retained for it.
    pub(super) fn entitlement(&mut self, engine: &Engine, account: AccountId) -> Option<ClaimAnswer<EntitlementProof>> {
        self.watch(account);
        let newest = engine.entitlement_proof(account)?;
        Self::keep(&mut self.entitlements, Some(newest.clone()), account);
        Some(Self::answer(&self.entitlements, self.accepted, account, newest))
    }

    /// The account's USDC distribution proof, at the accepted checkpoint when one is retained for it.
    pub(super) fn distribution(
        &mut self,
        engine: &Engine,
        account: AccountId,
    ) -> Option<ClaimAnswer<DistributionProof>> {
        self.watch(account);
        let newest = engine.distribution_proof(account)?;
        Self::keep(&mut self.distributions, Some(newest.clone()), account);
        Some(Self::answer(&self.distributions, self.accepted, account, newest))
    }

    fn watch(&mut self, account: AccountId) {
        self.asked += 1;
        self.watched.insert(account, self.asked);
        if self.watched.len() > MAX_WATCHED
            && let Some(stalest) = self.watched.iter().min_by_key(|(_, asked)| **asked).map(|(account, _)| *account)
        {
            self.watched.remove(&stalest);
        }
    }

    fn keep<P: Proved>(held: &mut Held<P>, proof: Option<P>, account: AccountId) {
        let Some(proof) = proof else { return };
        held.insert((proof.seq(), account), proof);
        while held.len() > MAX_HELD {
            held.pop_first();
        }
    }

    fn answer<P: Proved>(held: &Held<P>, accepted: Option<u64>, account: AccountId, newest: P) -> ClaimAnswer<P> {
        let at_accepted = accepted.and_then(|seq| held.get(&(seq, account)));
        let (newest_seq, newest_entitled) = (newest.seq(), newest.entitled());
        ClaimAnswer {
            claimable: at_accepted.is_some(),
            proof: at_accepted.cloned().unwrap_or(newest),
            accepted_seq: accepted,
            newest_seq,
            newest_entitled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAKER: AccountId = AccountId([7; 20]);

    /// A distribution proof at `seq` naming `entitled`; its path is not checked here.
    fn distribution(seq: u64, entitled: u128) -> DistributionProof {
        DistributionProof {
            seq,
            root: [seq as u8; 32].into(),
            account: STAKER,
            entitled: UsdcUnits(entitled),
            siblings: Vec::new(),
        }
    }

    /// The distribution answer the watched `claims` give for `newest`.
    fn ask(claims: &mut Claims, newest: DistributionProof) -> ClaimAnswer<DistributionProof> {
        Claims::keep(&mut claims.distributions, Some(newest.clone()), STAKER);
        Claims::answer(&claims.distributions, claims.accepted, STAKER, newest)
    }

    #[test]
    fn a_distribution_is_proven_at_the_accepted_checkpoint_while_the_vault_lags() {
        let mut claims = Claims::default();
        Claims::keep(&mut claims.distributions, Some(distribution(10, 5_000)), STAKER);
        Claims::keep(&mut claims.distributions, Some(distribution(11, 5_000)), STAKER);
        claims.accept(10);
        let answer = ask(&mut claims, distribution(12, 5_000));
        assert_eq!((answer.proof.seq, answer.claimable), (10, true));
        assert_eq!((answer.accepted_seq, answer.newest_seq), (Some(10), 12));
    }

    #[test]
    fn a_caught_up_vault_gets_the_newest_distribution_proof() {
        let mut claims = Claims::default();
        claims.accept(12);
        let answer = ask(&mut claims, distribution(12, 5_000));
        assert_eq!((answer.proof.seq, answer.claimable), (12, true));
    }

    #[test]
    fn a_distribution_that_grew_after_the_accepted_root_is_proven_at_what_it_was_then() {
        let mut claims = Claims::default();
        Claims::keep(&mut claims.distributions, Some(distribution(10, 5_000)), STAKER);
        claims.accept(10);
        let answer = ask(&mut claims, distribution(12, 9_000));
        assert_eq!((answer.proof.entitled, answer.newest_entitled), (UsdcUnits(5_000), UsdcUnits(9_000)));
        assert!(answer.claimable);
    }

    #[test]
    fn with_no_accepted_checkpoint_or_none_retained_a_distribution_waits() {
        let mut claims = Claims::default();
        Claims::keep(&mut claims.distributions, Some(distribution(10, 5_000)), STAKER);
        let answer = ask(&mut claims, distribution(12, 5_000));
        assert_eq!((answer.proof.seq, answer.accepted_seq, answer.claimable), (12, None, false));
        // The vault's checkpoint is one the account was not retained at.
        claims.accept(11);
        let answer = ask(&mut claims, distribution(12, 5_000));
        assert_eq!((answer.proof.seq, answer.claimable), (12, false));
    }

    #[test]
    fn accepting_a_checkpoint_drops_the_proofs_before_it_and_never_goes_back() {
        let mut claims = Claims::default();
        for seq in [10, 11, 12] {
            Claims::keep(&mut claims.distributions, Some(distribution(seq, 5_000)), STAKER);
        }
        claims.accept(11);
        assert_eq!(claims.distributions.keys().map(|(seq, _)| *seq).collect::<Vec<_>>(), [11, 12]);
        claims.accept(10);
        assert_eq!(claims.accepted, Some(11), "a lower report does not move the accepted checkpoint back");
    }

    #[test]
    fn the_watched_accounts_and_the_retained_proofs_are_bounded() {
        let mut claims = Claims::default();
        for n in 0..=MAX_WATCHED as u8 {
            claims.watch(AccountId([n; 20]));
        }
        assert_eq!(claims.watched.len(), MAX_WATCHED);
        assert!(!claims.watched.contains_key(&AccountId([0; 20])), "the least recently asked leaves first");
        for seq in 0..(MAX_HELD as u64 + 5) {
            Claims::keep(&mut claims.distributions, Some(distribution(seq, 1)), STAKER);
        }
        assert_eq!(claims.distributions.len(), MAX_HELD);
        assert_eq!(claims.distributions.keys().next().map(|(seq, _)| *seq), Some(5));
    }
}
