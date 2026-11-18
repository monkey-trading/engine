//! Proofs against one version of the state tree, from its leaves and their values alone.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::merkle::credits::unfinalised_credit_key;
use crate::merkle::tree::value::LeafValue;
use crate::merkle::tree::{StateTree, Version};
use crate::merkle::{
    AccountProof, DistributionProof, EntitlementProof, ExitProof, ExitTermsProof, LevelsProof, MarketProof,
    NoLevelsProof, PendingDepositProof, PendingReceipt, PositionProof, QueuedWithdrawalProof, ReceiptKind,
    ReceiptProof, UnfinalisedCreditProof, WithdrawalApprovalProof, account_key, distribution_key, entitlement_key,
    exit_terms_key, held_deposit_key, levels_key, market_key, pending_deposit_key, position_key,
    withdrawal_approval_key, withdrawal_queue_key,
};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

/// The ids of the exit leaves an account may hold, in proof order: its queued requests, and its
/// receipt-backed pending deposits and unfinalised credits by reference.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExitItems {
    pub queued: Vec<u64>,
    pub pending: Vec<String>,
    pub credits: Vec<String>,
}

impl StateTree {
    /// The value of the leaf at `key` in `version`, with its siblings, or `None` when the version
    /// holds no leaf there.
    fn valued(&self, version: &Version, key: &B256) -> Option<(&LeafValue, Vec<B256>)> {
        let (leaf, siblings) = self.arena.proven(version.root, key)?;
        // Every kind a proof reads is written with its value.
        Some((leaf.value.as_deref().expect("a proven leaf carries its value"), siblings))
    }

    /// The proof of `account`'s leaf, of each of its position leaves, of the leaf of each market it
    /// holds a position in, and of each position's levels or that it holds none, against `version`.
    /// `None` for an account the version does not hold.
    pub fn account_proof(&self, version: &Version, account: AccountId) -> Option<AccountProof> {
        let (value, siblings) = self.valued(version, &account_key(account))?;
        let LeafValue::Account { leaf, markets, levels } = value else {
            unreachable!("an account leaf holds an account")
        };
        let (positions, markets): (Vec<PositionProof>, Vec<MarketProof>) = markets
            .iter()
            .map(|&market| {
                // The account's markets are those its position leaves are in, at each version.
                let (position, siblings) =
                    self.valued(version, &position_key(account, market)).expect("an account's position has a leaf");
                let LeafValue::Position(position) = position else { unreachable!("a position leaf holds a position") };
                // Positions are only ever opened in listed markets, and listings are never removed.
                let (leaf, market_siblings) =
                    self.valued(version, &market_key(market)).expect("a position's market has a leaf");
                let LeafValue::Market(leaf) = leaf else { unreachable!("a market leaf holds a market") };
                (
                    PositionProof { market, position: *position, siblings },
                    MarketProof { market, leaf: leaf.clone(), siblings: market_siblings },
                )
            })
            .unzip();
        let (mut held, mut no_levels) = (Vec::new(), Vec::new());
        for &PositionProof { market, .. } in &positions {
            let key = levels_key(account, market);
            // The markets with levels are those the account's levels leaves are in, at each version.
            if levels.contains(&market) {
                let (levels, siblings) = self.valued(version, &key).expect("a position's levels have a leaf");
                let LeafValue::Levels(levels) = levels else { unreachable!("a levels leaf holds levels") };
                held.push(LevelsProof { market, levels: *levels, siblings });
            } else {
                let (siblings, other) =
                    self.arena.absence(version.root, &key).expect("a position without levels has no leaf");
                no_levels.push(NoLevelsProof { market, other, siblings });
            }
        }
        let (seq, root, leaf) = (version.seq, version.hash, leaf.clone());
        Some(AccountProof { seq, root, account, leaf, siblings, positions, markets, levels: held, no_levels })
    }

    /// The proof of `account`'s cumulative MONKEY entitlement against `version`, or `None` when the
    /// version holds none for it.
    pub fn entitlement_proof(&self, version: &Version, account: AccountId) -> Option<EntitlementProof> {
        let (value, siblings) = self.valued(version, &entitlement_key(account))?;
        let LeafValue::Entitlement(entitled) = value else { unreachable!("an entitlement leaf holds one") };
        Some(EntitlementProof { seq: version.seq, root: version.hash, account, entitled: *entitled, siblings })
    }

    /// The proof of `account`'s cumulative USDC distribution against `version`, or `None` when the
    /// version holds none for it.
    pub fn distribution_proof(&self, version: &Version, account: AccountId) -> Option<DistributionProof> {
        let (value, siblings) = self.valued(version, &distribution_key(account))?;
        let LeafValue::Distribution(entitled) = value else { unreachable!("a distribution leaf holds one") };
        Some(DistributionProof { seq: version.seq, root: version.hash, account, entitled: *entitled, siblings })
    }

    /// The proof of `account`'s withdrawal approval `nonce` against `version`, or `None` until a
    /// version includes it.
    pub fn approval_proof(&self, version: &Version, account: AccountId, nonce: u64) -> Option<WithdrawalApprovalProof> {
        let (value, siblings) = self.valued(version, &withdrawal_approval_key(account, nonce))?;
        let LeafValue::Approval(approval) = value else { unreachable!("an approval leaf holds an approval") };
        Some(WithdrawalApprovalProof { seq: version.seq, root: version.hash, approval: *approval, siblings })
    }

    /// The proof of the exit terms against `version`, or `None` for a version before `V38`.
    pub fn exit_terms_proof(&self, version: &Version) -> Option<ExitTermsProof> {
        let (value, siblings) = self.valued(version, &exit_terms_key())?;
        let LeafValue::Terms(terms) = value else { unreachable!("the exit terms leaf holds them") };
        Some(ExitTermsProof { seq: version.seq, root: version.hash, terms: terms.clone(), siblings })
    }

    /// Every proof `account` exits with against `version` but its approvals, which the caller adds
    /// from `approval_proof` for each nonce up to the account leaf's `withdrawal_nonce`, or `None` for
    /// a version before `V38`. The work is bounded by what the account holds.
    pub fn exit_proof_without_approvals(&self, version: &Version, account: AccountId) -> Option<ExitProof> {
        let mut proof = self.exit_head(version, account)?;
        let ExitItems { queued, pending, credits } = self.exit_items(account);
        proof.queued = queued.into_iter().filter_map(|seq| self.queued_proof(version, account, seq)).collect();
        proof.pending_deposits = pending
            .iter()
            .filter_map(|reference| self.pending_deposit_proof(version, Some(account), reference))
            .collect();
        proof.unfinalised_credits =
            credits.iter().filter_map(|reference| self.credit_proof(version, account, reference)).collect();
        proof.unfinalised_credits.sort_by_key(|credit| credit.deposit.deposit_id);
        Some(proof)
    }

    /// `account`'s exit proof against `version` with its terms, account and distribution proofs only,
    /// or `None` for a version before `V38`: the part whose work the markets bound. The rest is proved
    /// item by item from `exit_items`.
    pub fn exit_head(&self, version: &Version, account: AccountId) -> Option<ExitProof> {
        let terms = self.exit_terms_proof(version)?;
        Some(ExitProof {
            seq: version.seq,
            root: version.hash,
            terms,
            account: self.account_proof(version, account),
            approvals: Vec::new(),
            queued: Vec::new(),
            distribution: self.distribution_proof(version, account),
            pending_deposits: Vec::new(),
            unfinalised_credits: Vec::new(),
        })
    }

    /// The exit leaves `account` may hold in any version kept, in proof order, for `queued_proof`,
    /// `pending_deposit_proof` and `credit_proof` to prove one at a time.
    pub fn exit_items(&self, account: AccountId) -> ExitItems {
        let listed = |index: Option<&BTreeSet<String>>| index.into_iter().flatten().cloned().collect();
        ExitItems {
            queued: self.index.queued.get(&account).into_iter().flatten().copied().collect(),
            pending: listed(self.index.pending.get(&Some(account))),
            credits: listed(self.index.credits.get(&Some(account))),
        }
    }

    /// The proof of `account`'s queued request at `seq` against `version`, if the version holds it.
    pub fn queued_proof(&self, version: &Version, account: AccountId, seq: u64) -> Option<QueuedWithdrawalProof> {
        let (value, siblings) = self.valued(version, &withdrawal_queue_key(seq))?;
        let LeafValue::Queued(queued) = value else { return None };
        (queued.account == account).then_some(QueuedWithdrawalProof {
            seq: version.seq,
            root: version.hash,
            queued: *queued,
            siblings,
        })
    }

    /// The proof of the receipt-backed pending deposit at `reference` for `beneficiary`, `None` for the
    /// house, against `version`, if the version holds one.
    pub fn pending_deposit_proof(
        &self,
        version: &Version,
        beneficiary: Option<AccountId>,
        reference: &str,
    ) -> Option<PendingDepositProof> {
        let (value, siblings) = self.valued(version, &pending_deposit_key(reference))?;
        let LeafValue::Pending { receipt: Some(deposit), .. } = value else { return None };
        (deposit.account == beneficiary).then(|| PendingDepositProof {
            seq: version.seq,
            root: version.hash,
            chain_ref: reference.to_owned(),
            deposit: *deposit,
            siblings,
        })
    }

    /// The proof of `account`'s unfinalised credit at `reference` against `version`, if the version holds
    /// one.
    pub fn credit_proof(
        &self,
        version: &Version,
        account: AccountId,
        reference: &str,
    ) -> Option<UnfinalisedCreditProof> {
        let (value, siblings) = self.valued(version, &unfinalised_credit_key(reference))?;
        let LeafValue::Credit { receipt, .. } = value else { return None };
        (receipt.account == Some(account)).then(|| UnfinalisedCreditProof {
            seq: version.seq,
            root: version.hash,
            chain_ref: reference.to_owned(),
            deposit: *receipt,
            siblings,
        })
    }

    /// The proofs of the receipt-backed pending deposits for `beneficiary`, `None` for the house,
    /// against `version`, in reference order, or empty before `V38`.
    pub fn pending_deposit_proofs(
        &self,
        version: &Version,
        beneficiary: Option<AccountId>,
    ) -> Vec<PendingDepositProof> {
        let references = self.index.pending.get(&beneficiary).into_iter().flatten();
        references.filter_map(|reference| self.pending_deposit_proof(version, beneficiary, reference)).collect()
    }

    /// The proof of the receipt `version` holds at vault deposit id `deposit_id`: a credited and
    /// unfinalised one, a pending one or, from a version computed under `V77`, a held one, in that
    /// order and each in reference order. `None` before `V38`, or when the version holds no such
    /// receipt there. The work is a scan of the exit's indexed leaves.
    pub fn receipt_proof(&self, version: &Version, deposit_id: u64) -> Option<ReceiptProof> {
        self.exit_terms_proof(version)?;
        let all = |by_account: &std::collections::BTreeMap<Option<AccountId>, BTreeSet<String>>| {
            by_account.values().flatten().cloned().collect::<BTreeSet<String>>()
        };
        let held = if version.protocol >= ProtocolVersion::V77 { self.index.held.clone() } else { BTreeSet::new() };
        let kinds = [
            (ReceiptKind::Credited, all(&self.index.credits)),
            (ReceiptKind::Pending, all(&self.index.pending)),
            (ReceiptKind::Held, held),
        ];
        kinds.into_iter().find_map(|(kind, references)| {
            references.into_iter().find_map(|reference| {
                let key = match kind {
                    ReceiptKind::Credited => unfinalised_credit_key(&reference),
                    ReceiptKind::Pending => pending_deposit_key(&reference),
                    ReceiptKind::Held => held_deposit_key(&reference),
                };
                let (value, siblings) = self.valued(version, &key)?;
                let deposit = receipt_of(value).filter(|receipt| receipt.deposit_id == deposit_id)?;
                let (seq, root) = (version.seq, version.hash);
                Some(ReceiptProof { seq, root, kind, chain_ref: reference, deposit, siblings })
            })
        })
    }
}

/// The receipt a credit, pending or held leaf binds, if it is receipt-backed.
fn receipt_of(value: &LeafValue) -> Option<PendingReceipt> {
    match value {
        LeafValue::Credit { receipt, .. } => Some(*receipt),
        LeafValue::Pending { receipt, .. } | LeafValue::Held { receipt, .. } => *receipt,
        _ => None,
    }
}
