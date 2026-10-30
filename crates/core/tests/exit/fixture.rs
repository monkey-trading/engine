//! The exit vectors the vault's tests register and refund with (`contracts/test/ExitVector.t.sol`):
//! `fixtures/exit.json` must be exactly what the engine proves for these sessions, with every amount
//! as the raw integer the vault takes, and each claim the engine's own valuation in USDC base units.

use engine_core::Fixed;
use engine_core::merkle::{AccountProof, ExitProof, ExitTermsProof, PendingDepositProof};
use engine_core::withdrawal::usdc_amount;
use serde_json::{Value, json};

use crate::custody::custody;
use crate::support::trading;

const FIXTURE: &str = include_str!("../fixtures/exit.json");

#[test]
fn the_vault_test_vectors_are_the_engines_exit_proofs() {
    let (mut h, trader, _) = trading();
    h.engine.state_root();
    let exit = h.engine.exit_proof(trader).unwrap();
    let equity = h.equity_and_margin(trader).0;
    assert_eq!(exit.account.as_ref().unwrap().equity(), Some(equity));
    let trading = session(&exit, equity);

    let (mut h, [client, newcomer, _], _) = custody();
    h.engine.state_root();
    let credited = h.engine.exit_proof(client).unwrap();
    let pending = h.engine.exit_proof(newcomer).unwrap();
    let house = h.engine.house_pending_deposit_proofs().unwrap();
    let mut custody = session(&credited, h.equity_and_margin(client).0);
    custody["pending"] = pending_deposit(&pending.pending_deposits[0]);
    custody["house"] = pending_deposit(&house[0]);

    let computed = json!({ "trading": trading, "custody": custody });
    let pretty = serde_json::to_string_pretty(&computed).unwrap();
    assert_eq!(serde_json::from_str::<Value>(FIXTURE).unwrap(), computed, "expected:\n{pretty}");
}

/// The vectors of one exit proof: its root and terms, the account with its positions and markets,
/// its first approval and queued request, and the claim each pays in USDC base units.
fn session(exit: &ExitProof, equity: Fixed) -> Value {
    let account = exit.account.as_ref().unwrap();
    let mut session = json!({
        "seq": exit.seq.to_string(),
        "root": exit.root,
        "terms": terms(&exit.terms),
        "account": account_leaf(account),
        "positions": account.positions.iter().map(|p| json!({
            "market": p.market.to_string(),
            "qty": raw(p.position.qty),
            "entry": raw(p.position.entry),
            "siblings": p.siblings,
        })).collect::<Vec<_>>(),
        "markets": account.markets.iter().map(|m| {
            let leaf = &m.leaf;
            let (currency, fx_market) = match leaf.quote_currency {
                engine_core::market::QuoteCurrency::Usd => (0, 0),
                engine_core::market::QuoteCurrency::Eur { fx_market } => (1, fx_market),
                engine_core::market::QuoteCurrency::Gbp { fx_market } => (2, fx_market),
                engine_core::market::QuoteCurrency::Jpy { fx_market } => (3, fx_market),
            };
            json!({
                "market": m.market.to_string(),
                "marked": leaf.mark.is_some(),
                "mark": raw(leaf.mark.unwrap_or(Fixed::ZERO)),
                "quoteCurrency": currency.to_string(),
                "fxMarket": fx_market.to_string(),
                "fxMarked": leaf.fx_mark.is_some(),
                "fxMark": raw(leaf.fx_mark.unwrap_or(Fixed::ZERO)),
                "clientQty": raw(leaf.client_qty),
                "clientCost": raw(leaf.client_cost),
                "stateHash": leaf.state_hash,
                "siblings": m.siblings,
            })
        }).collect::<Vec<_>>(),
        // The vault floors a negative equity at zero and pays whole base units, rounded down.
        "claimUsdc": (equity.max(Fixed::ZERO).raw() / 1_000).to_string(),
    });
    if let Some(approval) = exit.approvals.first() {
        let a = &approval.approval;
        session["approval"] = json!({
            "account": a.account,
            "amountUsdc": a.amount_usdc().to_string(),
            "nonce": a.nonce.to_string(),
            "approvedSeq": a.approved_seq.to_string(),
            "siblings": approval.siblings,
        });
    }
    if let Some(queued) = exit.queued.first() {
        let q = &queued.queued;
        session["queued"] = json!({
            "account": q.account,
            "amount": raw(q.amount),
            "remaining": raw(q.remaining),
            "requestedSeq": q.requested_seq.to_string(),
            "ticket": q.ticket.to_string(),
            "remainingUsdc": usdc_amount(q.remaining).unwrap().to_string(),
            "siblings": queued.siblings,
        });
    }
    session
}

/// The exit terms leaf's vector.
fn terms(proof: &ExitTermsProof) -> Value {
    json!({
        "depositCursor": proof.terms.deposit_cursor.to_string(),
        "depositBase": proof.terms.deposit_base.to_string(),
        "pending": raw(proof.terms.pending),
        "held": raw(proof.terms.held),
        "unheld": raw(proof.terms.unheld),
        "heldReceipts": raw(proof.terms.held_receipts),
        "gaps": proof.terms.gaps.iter().map(u64::to_string).collect::<Vec<_>>(),
        "siblings": proof.siblings,
    })
}

/// An account leaf's vector, every field the vault rebuilds its value hash from.
fn account_leaf(proof: &AccountProof) -> Value {
    let leaf = &proof.leaf;
    json!({
        "account": proof.account,
        "balance": raw(leaf.balance),
        "unfinalisedDeposits": raw(leaf.unfinalised_deposits),
        "withdrawalNonce": leaf.withdrawal_nonce.to_string(),
        "toxicityBps": leaf.toxicity_bps.to_string(),
        "openPositions": leaf.open_positions.to_string(),
        "toxicityFraction": raw(leaf.toxicity_fraction),
        "toxicityHistory": raw(leaf.toxicity_history),
        "siblings": proof.siblings,
    })
}

/// A pending deposit's vector.
fn pending_deposit(proof: &PendingDepositProof) -> Value {
    let d = &proof.deposit;
    let zero = engine_core::types::AccountId([0; 20]);
    json!({
        "chainRef": proof.chain_ref,
        "depositId": d.deposit_id.to_string(),
        "account": d.account.unwrap_or(zero),
        "payer": d.payer.unwrap_or(zero),
        "amount": raw(d.amount),
        "siblings": proof.siblings,
    })
}

/// `Fixed`'s raw integer, as the vault takes it.
fn raw(value: Fixed) -> String {
    value.raw().to_string()
}
