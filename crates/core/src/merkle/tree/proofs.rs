//! Proofs against one version of the state tree, from its leaves and their values alone.

use std::collections::BTreeSet;

use alloy_primitives::B256;

use crate::merkle::credits::unfinalised_credit_key;
use crate::merkle::tree::value::LeafValue;
use crate::merkle::tree::{StateTree, Version};
use crate::merkle::{
    AccountProof, DistributionProof, EntitlementProof, ExitProof, ExitTermsProof, LevelsProof, MarketProof,
    PendingDepositProof, PendingReceipt, PositionProof, QueuedWithdrawalProof, ReceiptKind, ReceiptProof,
    UnfinalisedCreditProof, WithdrawalApprovalProof, account_key, distribution_key, entitlement_key, exit_terms_key,
    held_deposit_key, levels_key, market_key, pending_deposit_key, position_key, withdrawal_approval_key,
    withdrawal_queue_key,
};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

impl StateTree {
    /// The value of the leaf at `key` in `version`, with its siblings, or `None` when the version
    /// holds no leaf there.
    fn valued(&self, version: &Version, key: &B256) -> Option<(&LeafValue, Vec<B256>)> {
        let (leaf, siblings) = self.arena.proven(version.root, key)?;
        // Every kind a proof reads is written with its value.
        Some((leaf.value.as_deref().expect("a proven leaf carries its value"), siblings))
    }

    /// The proof of `account`'s leaf, of each of its position leaves, and of the leaf of each market
    /// it holds a position in, against `version`. `None` for an account the version does not hold.
    pub fn account_proof(&self, version: &Version, account: AccountId) -> Option<AccountProof> {
        let (value, siblings) = self.valued(version, &account_key(account))?;
        let LeafValue::Account { leaf, markets, levels } = value else {
            unreachable!("an account leaf holds an account")
        };
        let (positions, markets) = markets
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
        let levels = levels
            .iter()
            .map(|&market| {
                // The markets with levels are those the account's levels leaves are in, at each version.
                let (levels, siblings) =
                    self.valued(version, &levels_key(account, market)).expect("a position's levels have a leaf");
                let LeafValue::Levels(levels) = levels else { unreachable!("a levels leaf holds levels") };
                LevelsProof { market, levels: *levels, siblings }
            })
            .collect();
        let (seq, root) = (version.seq, version.hash);
        Some(AccountProof { seq, root, account, leaf: leaf.clone(), siblings, positions, markets, levels })
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
        let terms = self.exit_terms_proof(version)?;
        let (seq, root) = (version.seq, version.hash);
        let queued = self
            .named(version, self.index.queued.get(&account).into_iter().flatten().map(|seq| withdrawal_queue_key(*seq)))
            .filter_map(|(value, siblings)| match value {
                LeafValue::Queued(queued) if queued.account == account => {
                    Some(QueuedWithdrawalProof { seq, root, queued: *queued, siblings })
                }
                _ => None,
            })
            .collect();
        Some(ExitProof {
            seq,
            root,
            terms,
            account: self.account_proof(version, account),
            approvals: Vec::new(),
            queued,
            distribution: self.distribution_proof(version, account),
            pending_deposits: self.pending_deposit_proofs(version, Some(account)),
            unfinalised_credits: self.unfinalised_credit_proofs(version, account),
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
        references
            .filter_map(|reference| {
                let (value, siblings) = self.valued(version, &pending_deposit_key(reference))?;
                match value {
                    LeafValue::Pending { receipt: Some(deposit), .. } if deposit.account == beneficiary => {
                        let (seq, root, chain_ref) = (version.seq, version.hash, reference.clone());
                        Some(PendingDepositProof { seq, root, chain_ref, deposit: *deposit, siblings })
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// The proofs of `account`'s unfinalised credits against `version`, in increasing deposit id.
    pub fn unfinalised_credit_proofs(&self, version: &Version, account: AccountId) -> Vec<UnfinalisedCreditProof> {
        let references = self.index.credits.get(&Some(account)).into_iter().flatten();
        let mut proofs: Vec<UnfinalisedCreditProof> = references
            .filter_map(|reference| {
                let (value, siblings) = self.valued(version, &unfinalised_credit_key(reference))?;
                match value {
                    LeafValue::Credit { receipt, .. } if receipt.account == Some(account) => {
                        let (seq, root, chain_ref) = (version.seq, version.hash, reference.clone());
                        Some(UnfinalisedCreditProof { seq, root, chain_ref, deposit: *receipt, siblings })
                    }
                    _ => None,
                }
            })
            .collect();
        proofs.sort_by_key(|proof| proof.deposit.deposit_id);
        proofs
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

    /// The values and siblings of the leaves at `keys` that `version` holds.
    fn named<'a>(
        &'a self,
        version: &'a Version,
        keys: impl Iterator<Item = B256> + 'a,
    ) -> impl Iterator<Item = (&'a LeafValue, Vec<B256>)> + 'a {
        keys.filter_map(move |key| self.valued(version, &key))
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
