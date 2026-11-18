//! The exit against the last posted root, from a published log alone: the proofs an account or the
//! house exits with at one checkpoint, or the receipt the root holds at one vault deposit id, and the
//! vault calls that carry them, ABI-encoded for anyone to send (`engine exit-proof`).

use std::fs;
use std::path::Path;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use engine_core::market::QuoteCurrency;
use engine_core::merkle::{
    AccountProof, ExitProof, ExitTermsProof, PendingDepositProof, PendingReceipt, ReceiptKind, ReceiptProof,
};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use serde::Serialize;

use crate::log::{self, LogEntry, SegmentedLog};

/// How many unfinalised credits one `registerExitCredits` call carries, so each call stays well
/// within a transaction's size however many credits an account holds.
const CREDITS_PER_CALL: usize = 32;

// The vault's exit calls and the verifier's structs, as `contracts/src/Vault.sol` and
// `contracts/src/ExitVerifier.sol` declare them.
sol! {
    struct AccountState {
        int128 balance;
        int128 unfinalisedDeposits;
        uint64 withdrawalNonce;
        int64 toxicityBps;
        uint32 openPositions;
        int128 toxicityFraction;
        int128 toxicityHistory;
        bytes32[] siblings;
    }
    struct PositionState { uint32 market; int128 qty; int128 entry; bytes32[] siblings; }
    struct MarketState {
        uint32 market;
        bool marked;
        int128 mark;
        uint8 quoteCurrency;
        uint32 fxMarket;
        bool fxMarked;
        int128 fxMark;
        int128 clientQty;
        int128 clientCost;
        bytes32 stateHash;
        bytes32[] siblings;
    }
    struct ExitTerms { uint64 depositCursor; uint64 depositBase; int128 pending; int128 held; int128 unheld; int128 heldReceipts; uint64[] gaps; }
    struct QueuedWithdrawal { address account; int128 amount; int128 remaining; uint64 requestedSeq; uint64 ticket; }
    struct ReceiptLeaf { bytes chainRef; uint64 depositId; address account; address payer; int128 amount; bytes32[] siblings; }
    struct Approval { address account; uint256 amount; uint64 nonce; uint64 approvedSeq; }

    function startExit(ExitTerms terms, bytes32[] siblings) external;
    function registerExit(address account, AccountState state, PositionState[] positions, MarketState[] markets) external;
    function registerExitCredits(address account, ReceiptLeaf[] credits) external;
    function registerExitWithdrawal(Approval approval, bytes32[] siblings) external;
    function registerExitQueued(QueuedWithdrawal queued, bytes32[] siblings) external;
    function registerExitDistribution(address account, uint256 entitled, bytes32[] siblings) external;
    function claimExit(address account) external returns (uint256);
    function refundReceipt(ReceiptLeaf leaf, uint8 kind) external;
}

/// What `engine exit-proof` prints: the proofs as the engine holds them, and each vault call that
/// carries them, in the order the exit takes them.
#[derive(Debug, Serialize)]
pub struct ExitBundle {
    pub seq: u64,
    pub root: B256,
    /// The account's proofs, or none for the house.
    pub account: Option<ExitProof>,
    /// The house's pending deposits, refunded to their payers; empty for an account.
    pub house_pending_deposits: Vec<PendingDepositProof>,
    /// The receipt the root holds at the deposit id asked for, if it holds one there.
    pub receipt: Option<ReceiptProof>,
    pub calls: Vec<ExitCall>,
}

/// One vault call, ABI-encoded.
#[derive(Debug, Serialize)]
pub struct ExitCall {
    /// What the call does, with the function it calls.
    pub what: String,
    pub calldata: Bytes,
}

/// Whose proofs to build.
#[derive(Clone, Copy, Debug)]
pub enum ExitSubject {
    Account(AccountId),
    House,
    /// The receipt the root holds at a vault deposit id, through which the vault refunds its own
    /// deposit there when its record does not match.
    Deposit(u64),
}

/// Applies the log at `path`'s commands through its checkpoint at `seq`, which must be the vault's
/// `latestSeq`, and builds `subject`'s exit bundle against that checkpoint's root. Every approval the
/// account holds is included; one the vault's `usedNonces` already marks paid is refused there.
/// Error cases:
/// - the log cannot be read, or holds no checkpoint at `seq` whose root its commands reproduce (see
///   `engine_at_checkpoint`)
/// - the log's protocol at `seq` predates `V38`, so its root holds no exit terms
/// - for a deposit id, the root holds no credited, pending or held receipt there
pub fn exit_bundle(path: &Path, seq: u64, subject: ExitSubject) -> anyhow::Result<ExitBundle> {
    let engine = engine_at_checkpoint(path, seq)?;
    let missing = || format!("the root at seq {seq} holds no exit terms: its protocol predates V38");
    match subject {
        ExitSubject::Account(account) => {
            let proof = engine.exit_proof(account).with_context(missing)?;
            let calls = account_calls(account, &proof);
            let (seq, root) = (proof.seq, proof.root);
            Ok(ExitBundle { seq, root, account: Some(proof), house_pending_deposits: Vec::new(), receipt: None, calls })
        }
        ExitSubject::House => {
            let pending = engine.house_pending_deposit_proofs().with_context(missing)?;
            let terms = engine.exit_terms_proof().with_context(missing)?;
            let mut calls = vec![start(&terms)];
            calls.extend(pending.iter().map(refund));
            let (seq, root) = (terms.seq, terms.root);
            Ok(ExitBundle { seq, root, account: None, house_pending_deposits: pending, receipt: None, calls })
        }
        ExitSubject::Deposit(deposit_id) => {
            let terms = engine.exit_terms_proof().with_context(missing)?;
            let receipt = engine.receipt_proof(deposit_id).with_context(|| {
                format!("the root at seq {seq} holds no credited, pending or held receipt at deposit id {deposit_id}")
            })?;
            let calls = vec![start(&terms), refund_at_id(&receipt)];
            let (seq, root) = (terms.seq, terms.root);
            Ok(ExitBundle {
                seq,
                root,
                account: None,
                house_pending_deposits: Vec::new(),
                receipt: Some(receipt),
                calls,
            })
        }
    }
}

/// The engine as of the log at `path`'s checkpoint at `seq`: its commands applied in order through
/// that checkpoint, whose logged root it must reproduce. Signatures are not checked: the vault checks
/// every proof against the root it accepted, so a log altered anywhere yields proofs it refuses.
/// Reading stops once the checkpoint's root is reproduced, so a suffix the node died writing does not
/// block proofs the complete prefix supports.
/// Error cases:
/// - nothing is at `path`, or it cannot be read or fails its manifest check
/// - a command fails to apply, or the commands do not reproduce the checkpoint's root
/// - the log holds no checkpoint at `seq`
fn engine_at_checkpoint(path: &Path, seq: u64) -> anyhow::Result<Engine> {
    let found = fs::metadata(path).with_context(|| format!("no readable log at {}", path.display()))?;
    let mut engine = Engine::new();
    let mut reached = false;
    let mut visit = |entry: LogEntry| {
        match entry {
            LogEntry::Command(command) => {
                let at = command.sequenced.seq;
                engine.apply(&command.sequenced).with_context(|| format!("command at seq {at} does not apply"))?;
            }
            LogEntry::Checkpoint(cp) if cp.seq == seq => {
                ensure!(engine.last_seq() == Some(seq), "checkpoint for seq {seq} found after {:?}", engine.last_seq());
                let root = format!("0x{}", hex::encode(engine.state_root()));
                if root != cp.root {
                    bail!("the log's root at seq {seq} is {}, its commands give {root}", cp.root);
                }
                reached = true;
                // The suffix past the requested checkpoint cannot change its root; stop before a
                // torn write there blocks proofs the complete prefix supports.
                return Err(log::Stop.into());
            }
            _ => {}
        }
        Ok(())
    };
    let stopped = |error: &anyhow::Error| error.chain().any(|cause| cause.is::<log::Stop>());
    match if found.is_dir() {
        SegmentedLog::open(path, &mut visit).map(|_| ())
    } else {
        log::read_file(path, &mut visit)
    } {
        Ok(()) => {}
        Err(error) if stopped(&error) => {}
        Err(error) => return Err(error),
    }
    ensure!(reached, "{} holds no checkpoint at seq {seq}", path.display());
    Ok(engine)
}

/// The calls that exit `account`: start the exit, register its equity, approvals, queued requests
/// and distribution, refund the deposits pending for it, and claim once the window closes. A call
/// the vault refuses for the reason its description names can be skipped.
pub fn account_calls(account: AccountId, proof: &ExitProof) -> Vec<ExitCall> {
    let mut calls = vec![start(&proof.terms)];
    if let Some(held) = &proof.account {
        let (state, positions, markets) = account_state(held);
        let calldata = registerExitCall { account: address(held.account), state, positions, markets }.abi_encode();
        calls.push(call(
            "registerExit: the account's equity at the root's marks, less its unfinalised principal",
            calldata,
        ));
        for batch in proof.unfinalised_credits.chunks(CREDITS_PER_CALL) {
            let credits = batch.iter().map(|c| receipt_leaf(&c.chain_ref, &c.deposit, &c.siblings)).collect();
            let calldata = registerExitCreditsCall { account: address(held.account), credits }.abi_encode();
            let ids = batch.iter().map(|c| c.deposit.deposit_id.to_string()).collect::<Vec<_>>().join(", ");
            let what = format!(
                "registerExitCredits: unfinalised credits at deposit ids {ids}, each adding its principal once where \
                 the vault's deposit records match it; one anyone has already registered adds nothing again"
            );
            calls.push(call(&what, calldata));
        }
    }
    for held in &proof.approvals {
        let a = &held.approval;
        let approval = Approval {
            account: address(a.account),
            amount: U256::from(a.amount_usdc()),
            nonce: a.nonce,
            approvedSeq: a.approved_seq,
        };
        let calldata = registerExitWithdrawalCall { approval, siblings: held.siblings.clone() }.abi_encode();
        let what = format!(
            "registerExitWithdrawal: approval nonce {}; refused with WithdrawalAlreadyPaid when \
             usedNonces(account, {}) shows the vault paid it",
            a.nonce, a.nonce
        );
        calls.push(call(&what, calldata));
    }
    for queued in &proof.queued {
        let q = &queued.queued;
        let queued_withdrawal = QueuedWithdrawal {
            account: address(q.account),
            amount: q.amount.raw(),
            remaining: q.remaining.raw(),
            requestedSeq: q.requested_seq,
            ticket: q.ticket,
        };
        let siblings = queued.siblings.clone();
        let calldata = registerExitQueuedCall { queued: queued_withdrawal, siblings }.abi_encode();
        calls.push(call(&format!("registerExitQueued: request seq {}", q.requested_seq), calldata));
    }
    if let Some(distribution) = &proof.distribution {
        let (account, entitled) = (address(distribution.account), U256::from(distribution.entitled.0));
        let calldata =
            registerExitDistributionCall { account, entitled, siblings: distribution.siblings.clone() }.abi_encode();
        calls.push(call("registerExitDistribution: the account's unpaid distribution", calldata));
    }
    calls.extend(proof.pending_deposits.iter().map(refund));
    let registers = proof.account.is_some()
        || !proof.approvals.is_empty()
        || !proof.queued.is_empty()
        || proof.distribution.is_some();
    if registers {
        let calldata = claimExitCall { account: address(account) }.abi_encode();
        calls.push(call("claimExit: once the claim window has closed", calldata));
    }
    calls
}

/// `startExit` with the root's exit terms.
fn start(terms: &ExitTermsProof) -> ExitCall {
    let exit_terms = ExitTerms {
        depositCursor: terms.terms.deposit_cursor,
        depositBase: terms.terms.deposit_base,
        pending: terms.terms.pending.raw(),
        held: terms.terms.held.raw(),
        unheld: terms.terms.unheld.raw(),
        heldReceipts: terms.terms.held_receipts.raw(),
        gaps: terms.terms.gaps.clone(),
    };
    let calldata = startExitCall { terms: exit_terms, siblings: terms.siblings.clone() }.abi_encode();
    call(
        "startExit: once no checkpoint has been posted for the exit period; refused with ExitActive once \
         anyone has started it",
        calldata,
    )
}

/// `refundReceipt` for one pending deposit.
fn refund(proof: &PendingDepositProof) -> ExitCall {
    let leaf = receipt_leaf(&proof.chain_ref, &proof.deposit, &proof.siblings);
    let calldata = refundReceiptCall { leaf, kind: ReceiptKind::Pending.leaf_kind() }.abi_encode();
    call(&format!("refundReceipt: pending deposit {} to its payer", proof.deposit.deposit_id), calldata)
}

/// `refundReceipt` for the receipt the root holds at a deposit id: the vault refunds its own deposit
/// there to its payer when its records do not match the receipt, and a matched pending deposit from
/// the pending reserve.
fn refund_at_id(receipt: &ReceiptProof) -> ExitCall {
    let leaf = receipt_leaf(&receipt.chain_ref, &receipt.deposit, &receipt.siblings);
    let id = leaf.depositId;
    let what = format!(
        "refundReceipt: the vault's deposit {id} to its payer, while claims register when its records do not \
         match the {:?} receipt the root holds there, and at any time after the start for a pending receipt they \
         match; refused with NotRefundable for a credited or held receipt they match, or an id already \
         refunded",
        receipt.kind
    );
    call(&what, refundReceiptCall { leaf, kind: receipt.kind.leaf_kind() }.abi_encode())
}

/// The verifier's receipt leaf for the receipt at `chain_ref`.
fn receipt_leaf(chain_ref: &str, deposit: &PendingReceipt, siblings: &[B256]) -> ReceiptLeaf {
    ReceiptLeaf {
        chainRef: Bytes::copy_from_slice(chain_ref.as_bytes()),
        depositId: deposit.deposit_id,
        account: deposit.account.map_or(Address::ZERO, address),
        payer: deposit.payer.map_or(Address::ZERO, address),
        amount: deposit.amount.raw(),
        siblings: siblings.to_vec(),
    }
}

/// The verifier's account, position and market structs from an account proof.
fn account_state(proof: &AccountProof) -> (AccountState, Vec<PositionState>, Vec<MarketState>) {
    let leaf = &proof.leaf;
    let state = AccountState {
        balance: leaf.balance.raw(),
        unfinalisedDeposits: leaf.unfinalised_deposits.raw(),
        withdrawalNonce: leaf.withdrawal_nonce,
        toxicityBps: leaf.toxicity_bps,
        openPositions: leaf.open_positions,
        toxicityFraction: leaf.toxicity_fraction.raw(),
        toxicityHistory: leaf.toxicity_history.raw(),
        siblings: proof.siblings.clone(),
    };
    let positions = proof
        .positions
        .iter()
        .map(|p| PositionState {
            market: p.market,
            qty: p.position.qty.raw(),
            entry: p.position.entry.raw(),
            siblings: p.siblings.clone(),
        })
        .collect();
    let markets = proof
        .markets
        .iter()
        .map(|m| {
            let (quote_currency, fx_market) = match m.leaf.quote_currency {
                QuoteCurrency::Usd => (0, 0),
                QuoteCurrency::Eur { fx_market } => (1, fx_market),
                QuoteCurrency::Gbp { fx_market } => (2, fx_market),
                QuoteCurrency::Jpy { fx_market } => (3, fx_market),
            };
            MarketState {
                market: m.market,
                marked: m.leaf.mark.is_some(),
                mark: m.leaf.mark.unwrap_or(Fixed::ZERO).raw(),
                quoteCurrency: quote_currency,
                fxMarket: fx_market,
                fxMarked: m.leaf.fx_mark.is_some(),
                fxMark: m.leaf.fx_mark.unwrap_or(Fixed::ZERO).raw(),
                clientQty: m.leaf.client_qty.raw(),
                clientCost: m.leaf.client_cost.raw(),
                stateHash: m.leaf.state_hash,
                siblings: m.siblings.clone(),
            }
        })
        .collect();
    (state, positions, markets)
}

fn call(what: &str, calldata: Vec<u8>) -> ExitCall {
    ExitCall { what: what.to_owned(), calldata: calldata.into() }
}

fn address(account: AccountId) -> Address {
    Address::from(account.0)
}

#[cfg(test)]
mod tests;
