//! The leaf writes of the entries a root takes, each read from the state as it is when they are taken.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::B256;

use crate::access::{AccessDeposit, HeldDeposit};
use crate::deposits::RecordedDeposit;
use crate::engine::Engine;
use crate::merkle::credits::{unfinalised_credit, unfinalised_credit_key};
use crate::merkle::leaf::{
    deposit_key, deposit_transaction_key, emission_ledger_hash, emission_ledger_key, legacy_deposit_key,
};
use crate::merkle::state::Touched;
use crate::merkle::tree::LeafUpdate;
use crate::merkle::tree::value::LeafValue;
use crate::merkle::{
    AccountLeaf, MarketLeaf, PendingReceipt, account_key, corporate_action_key, corporate_reference_key,
    credited_ref_hash, credited_ref_key, distribution_hash, distribution_key, eligibility_key, entitlement_hash,
    entitlement_key, held_deposit_key, json_hash, levels_hash, levels_key, market_key, markout_hash, markout_key,
    order_hash, order_key, pending_deposit_key, pending_receipt_hash, period_result_hash, period_result_key,
    position_hash, position_key, request_nonce_hash, request_nonce_key, session_key_hash, session_key_key, staked_hash,
    staked_key, withdrawal_approval_hash, withdrawal_queue_hash, withdrawal_queue_key,
};
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

impl Engine {
    /// The leaf writes of the entries `touched` names, each with its value hash, `None` for one
    /// removed, and its value where a proof reads it. Each account a changed position belongs to is
    /// written too, so its value names the markets its position leaves are in.
    pub(super) fn leaf_updates(&self, touched: &Touched) -> Vec<LeafUpdate> {
        let version = self.protocol_version;
        let Touched {
            accounts,
            markets,
            nonces,
            refs,
            staking_refs,
            period_results,
            entitlements,
            emission_ledgers,
            staked,
            distributions,
            session_keys,
            toxicity_scores,
            markout_spreads,
            volumes,
            reversed_deposits,
            awards,
            deposits,
            deposit_transactions,
            legacy_deposits,
            withdrawal_queue,
            withdrawal_approvals,
            corporate_actions,
            corporate_refs,
            eligibility,
            holds,
            pending,
            changed,
        } = touched;
        let hashed = |(key, value_hash)| LeafUpdate::hashed(key, value_hash);
        let mut updates: Vec<LeafUpdate> = Vec::new();
        updates.extend(
            corporate_actions
                .iter()
                .map(|id| (corporate_action_key(id), self.corporate_actions.get(id).map(json_hash)))
                .map(hashed),
        );
        updates.extend(
            corporate_refs
                .iter()
                .map(|id| (corporate_reference_key(id), self.corporate_refs.contains(id).then(|| json_hash(id))))
                .map(hashed),
        );
        let holders: BTreeSet<AccountId> =
            accounts.iter().copied().chain(changed.positions.iter().map(|(account, _)| *account)).collect();
        updates.extend(holders.into_iter().map(|id| {
            let value = self.accounts.get(&id).map(|acct| {
                let leaf = AccountLeaf::of(acct);
                let markets = acct.positions.keys().copied().collect();
                // The levels leaves exist from `V79` only.
                let levels =
                    if version >= ProtocolVersion::V79 { acct.levels.keys().copied().collect() } else { Vec::new() };
                (leaf.value_hash(), LeafValue::Account { markets, levels, leaf })
            });
            LeafUpdate::valued(account_key(id), value)
        }));
        updates.extend(markets.iter().map(|id| {
            let value = self.markets.get(id).map(|market| {
                let leaf = MarketLeaf::of(market);
                (leaf.value_hash(), LeafValue::Market(leaf))
            });
            LeafUpdate::valued(market_key(*id), value)
        }));
        updates.extend(
            nonces
                .iter()
                .map(|id| (request_nonce_key(*id), self.request_nonces.get(id).map(|nonce| request_nonce_hash(*nonce))))
                .map(hashed),
        );
        updates.extend(
            refs.iter()
                .map(|r| (credited_ref_key(r), self.credited_refs.contains(r).then(|| credited_ref_hash(r))))
                .map(hashed),
        );
        updates.extend(
            deposits
                .iter()
                .map(|reference| (deposit_key(reference), self.deposits.get(reference).map(json_hash)))
                .map(hashed),
        );
        if version >= ProtocolVersion::V77 {
            updates.extend(deposits.iter().map(|reference| {
                let credit = self.deposits.get(reference).and_then(unfinalised_credit).map(|receipt| {
                    (pending_receipt_hash(&receipt), LeafValue::Credit { reference: reference.clone(), receipt })
                });
                LeafUpdate::valued(unfinalised_credit_key(reference), credit)
            }));
        }
        updates.extend(
            deposit_transactions
                .iter()
                .map(|reference| {
                    (deposit_transaction_key(reference), self.deposit_transactions.get(reference).map(json_hash))
                })
                .map(hashed),
        );
        updates.extend(
            legacy_deposits
                .iter()
                .map(|reference| (legacy_deposit_key(reference), self.legacy_deposits.get(reference).map(json_hash)))
                .map(hashed),
        );
        updates.extend(
            eligibility.iter().map(|id| (eligibility_key(*id), self.eligibility.get(id).map(json_hash))).map(hashed),
        );
        updates.extend(holds.iter().map(|reference| {
            let value =
                self.held_deposits.get(reference).map(|hold| held_leaf(reference, hold, &self.deposits, version));
            LeafUpdate::valued(held_deposit_key(reference), value)
        }));
        updates.extend(pending.iter().map(|reference| {
            let value = self
                .pending_deposits
                .get(reference)
                .map(|deposit| pending_leaf(reference, deposit, &self.deposits, version));
            LeafUpdate::valued(pending_deposit_key(reference), value)
        }));
        updates.extend(
            changed
                .markouts
                .iter()
                .map(|index| (markout_key(*index), self.markout(*index).map(markout_hash)))
                .map(hashed),
        );
        updates.extend(self.open_time_leaves(&changed.positions).into_iter().map(hashed));
        updates.extend(self.levels_leaves(&changed.positions).into_iter().map(|((id, market), levels)| {
            let value = levels.map(|levels| (levels_hash(&levels), LeafValue::Levels(levels)));
            LeafUpdate::valued(levels_key(id, market), value)
        }));
        updates.extend(changed.positions.iter().map(|&(id, market)| {
            let position = self.accounts.get(&id).and_then(|acct| acct.positions.get(&market)).copied();
            let value = position.map(|position| (position_hash(&position), LeafValue::Position(position)));
            LeafUpdate::valued(position_key(id, market), value)
        }));
        updates.extend(changed.orders.iter().map(|&(id, order_id)| {
            let order = self.accounts.get(&id).and_then(|acct| acct.orders.get(&order_id));
            LeafUpdate::hashed(order_key(order_id), order.map(|order| order_hash(id, order)))
        }));
        updates.extend(
            period_results
                .iter()
                .map(|id| {
                    let result = self.period_results.get(id).map(|result| period_result_hash(result, version));
                    (period_result_key(*id), result)
                })
                .map(hashed),
        );
        updates.extend(entitlements.iter().map(|id| {
            let value = self.entitlements.get(id).map(|e| (entitlement_hash(*e), LeafValue::Entitlement(*e)));
            LeafUpdate::valued(entitlement_key(*id), value)
        }));
        updates.extend(
            emission_ledgers
                .iter()
                .map(|id| (emission_ledger_key(*id), self.emission_ledgers.get(id).map(emission_ledger_hash)))
                .map(hashed),
        );
        updates.extend(distributions.iter().map(|id| {
            let value = self.distributions.get(id).map(|d| (distribution_hash(*d), LeafValue::Distribution(*d)));
            LeafUpdate::valued(distribution_key(*id), value)
        }));
        updates.extend(withdrawal_queue.iter().map(|seq| {
            let value =
                self.withdrawal_queue.get(seq).map(|q| (withdrawal_queue_hash(q, version), LeafValue::Queued(*q)));
            LeafUpdate::valued(withdrawal_queue_key(*seq), value)
        }));
        updates.extend(withdrawal_approvals.iter().map(|key| {
            let value =
                self.withdrawal_approvals.get(key).map(|a| (withdrawal_approval_hash(a), LeafValue::Approval(*a)));
            LeafUpdate::valued(*key, value)
        }));
        updates.extend(self.staking_reference_leaves(staking_refs).into_iter().map(hashed));
        updates.extend(
            staked.iter().map(|id| (staked_key(*id), self.staked.get(id).map(|s| staked_hash(*s)))).map(hashed),
        );
        updates.extend(
            session_keys
                .iter()
                .map(|account| (session_key_key(*account), self.session_keys.get(account).map(session_key_hash)))
                .map(hashed),
        );
        updates.extend(self.toxicity_leaves(toxicity_scores, markout_spreads).into_iter().map(hashed));
        updates.extend(self.volume_leaves(volumes).into_iter().map(hashed));
        updates.extend(self.reversed_deposit_leaves(reversed_deposits).into_iter().map(hashed));
        updates.extend(self.award_leaves(awards).into_iter().map(hashed));
        updates
    }
}

/// A pending deposit's leaf: from `V38` a receipt-backed one's ABI leaf, and otherwise the keccak-256
/// of its JSON, with its principal and receipt for the exit's totals and proofs.
fn pending_leaf(
    reference: &str,
    deposit: &AccessDeposit,
    receipts: &BTreeMap<String, RecordedDeposit>,
    version: ProtocolVersion,
) -> (B256, LeafValue) {
    let receipt = receipts.get(reference).map(|recorded| PendingReceipt {
        deposit_id: recorded.receipt.deposit_id,
        account: deposit.account,
        payer: deposit.payer,
        amount: deposit.amount,
    });
    let hash = match &receipt {
        Some(receipt) if version >= ProtocolVersion::V38 => pending_receipt_hash(receipt),
        _ => json_hash(deposit),
    };
    (hash, LeafValue::Pending { reference: reference.to_owned(), amount: deposit.amount, receipt })
}

/// A held deposit's leaf: from `V77` the ABI leaf of a receipt-backed one whose receipt binds its
/// payer, or its absence, as a pending one's, and otherwise the keccak-256 of its JSON, with its
/// principal and receipt for the exit's totals and proofs.
fn held_leaf(
    reference: &str,
    hold: &HeldDeposit,
    receipts: &BTreeMap<String, RecordedDeposit>,
    version: ProtocolVersion,
) -> (B256, LeafValue) {
    let deposit = &hold.deposit;
    // Bound to the receipt's own authenticated vault caller, which a replacement records apart from
    // the terminal hold it keeps, screened for the orphan's caller; a booked surplus has none, by the
    // receipt and by its screening alike. A receipt that records none while its hold names one, from
    // before `V26` or a payerless replacement under a terminal hold, has no caller to bind: it keeps
    // its JSON leaf, which the exit cannot prove unmatched, and stays held.
    let receipt = receipts.get(reference).filter(|recorded| recorded.payer.is_some() || deposit.payer.is_none()).map(
        |recorded| PendingReceipt {
            deposit_id: recorded.receipt.deposit_id,
            account: deposit.account,
            payer: recorded.payer,
            amount: deposit.amount,
        },
    );
    let hash = match &receipt {
        Some(receipt) if version >= ProtocolVersion::V77 => pending_receipt_hash(receipt),
        _ => json_hash(hold),
    };
    (hash, LeafValue::Held { reference: reference.to_owned(), amount: deposit.amount, receipt })
}
