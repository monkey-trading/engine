//! The exit against the last posted root, from a published log alone: the proofs an account or the
//! house exits with at one checkpoint, and the vault calls that carry them, ABI-encoded for anyone
//! to send (`engine exit-proof`).

use std::fs;
use std::path::Path;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use engine_core::market::QuoteCurrency;
use engine_core::merkle::{AccountProof, ExitProof, ExitTermsProof, PendingDepositProof};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use serde::Serialize;

use crate::log::{self, LogEntry, SegmentedLog};

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
    struct PendingDeposit { uint64 depositId; address account; address payer; int128 amount; }
    struct Approval { address account; uint256 amount; uint64 nonce; uint64 approvedSeq; }

    function startExit(ExitTerms terms, bytes32[] siblings) external;
    function registerExit(address account, AccountState state, PositionState[] positions, MarketState[] markets) external;
    function registerExitWithdrawal(Approval approval, bytes32[] siblings) external;
    function registerExitQueued(QueuedWithdrawal queued, bytes32[] siblings) external;
    function registerExitDistribution(address account, uint256 entitled, bytes32[] siblings) external;
    function claimExit(address account) external returns (uint256);
    function refundPendingDeposit(bytes chainRef, PendingDeposit pending, bytes32[] siblings) external;
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
}

/// Applies the log at `path`'s commands through its checkpoint at `seq`, which must be the vault's
/// `latestSeq`, and builds `subject`'s exit bundle against that checkpoint's root. Every approval the
/// account holds is included; one the vault's `usedNonces` already marks paid is refused there.
/// Error cases:
/// - the log cannot be read, or holds no checkpoint at `seq` whose root its commands reproduce (see
///   `engine_at_checkpoint`)
/// - the log's protocol at `seq` predates `V38`, so its root holds no exit terms
pub fn exit_bundle(path: &Path, seq: u64, subject: ExitSubject) -> anyhow::Result<ExitBundle> {
    let engine = engine_at_checkpoint(path, seq)?;
    let missing = || format!("the root at seq {seq} holds no exit terms: its protocol predates V38");
    match subject {
        ExitSubject::Account(account) => {
            let proof = engine.exit_proof(account).with_context(missing)?;
            let calls = account_calls(account, &proof);
            let (seq, root) = (proof.seq, proof.root);
            Ok(ExitBundle { seq, root, account: Some(proof), house_pending_deposits: Vec::new(), calls })
        }
        ExitSubject::House => {
            let pending = engine.house_pending_deposit_proofs().with_context(missing)?;
            let terms = engine.exit_terms_proof().with_context(missing)?;
            let mut calls = vec![start(&terms)];
            calls.extend(pending.iter().map(refund));
            Ok(ExitBundle { seq: terms.seq, root: terms.root, account: None, house_pending_deposits: pending, calls })
        }
    }
}

/// The engine as of the log at `path`'s checkpoint at `seq`: its commands applied in order through
/// that checkpoint, whose logged root it must reproduce. Signatures are not checked: the vault checks
/// every proof against the root it accepted, so a log altered anywhere yields proofs it refuses.
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
            _ if reached => {}
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
            }
            _ => {}
        }
        Ok(())
    };
    if found.is_dir() {
        SegmentedLog::open(path, &mut visit)?;
    } else {
        log::read_file(path, &mut visit)?;
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
        calls.push(call("registerExit: the account's equity at the root's marks", calldata));
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

/// `refundPendingDeposit` for one pending deposit.
fn refund(proof: &PendingDepositProof) -> ExitCall {
    let d = &proof.deposit;
    let pending = PendingDeposit {
        depositId: d.deposit_id,
        account: d.account.map_or(Address::ZERO, address),
        payer: d.payer.map_or(Address::ZERO, address),
        amount: d.amount.raw(),
    };
    let chain_ref = Bytes::copy_from_slice(proof.chain_ref.as_bytes());
    let calldata = refundPendingDepositCall { chainRef: chain_ref, pending, siblings: proof.siblings.clone() };
    call(&format!("refundPendingDeposit: deposit {} to its payer", d.deposit_id), calldata.abi_encode())
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
mod tests {
    use engine_core::merkle::{DistributionProof, ExitTerms};
    use engine_core::staking::UsdcUnits;

    use super::*;

    /// The node's reference log for the latest protocol: placeholder checkpoint signatures, and
    /// signed withdrawals, which `engine replay` refuses without a trusted signer.
    const REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v38.jsonl");

    #[test]
    fn a_staker_with_no_trading_account_is_given_its_claim_call() {
        let staker = AccountId([5; 20]);
        let terms = ExitTermsProof { seq: 9, root: B256::ZERO, terms: ExitTerms::default(), siblings: vec![] };
        let distribution =
            DistributionProof { seq: 9, root: B256::ZERO, account: staker, entitled: UsdcUnits(7), siblings: vec![] };
        let proof = ExitProof {
            seq: 9,
            root: B256::ZERO,
            terms,
            account: None,
            approvals: vec![],
            queued: vec![],
            distribution: Some(distribution),
            pending_deposits: vec![],
        };
        let calls = account_calls(staker, &proof);
        let selectors: Vec<[u8; 4]> = calls.iter().map(|call| call.calldata[..4].try_into().unwrap()).collect();
        assert_eq!(
            selectors,
            [startExitCall::SELECTOR, registerExitDistributionCall::SELECTOR, claimExitCall::SELECTOR]
        );
        let claim = claimExitCall::abi_decode(&calls[2].calldata).unwrap();
        assert_eq!(claim.account, address(staker));
        let none = ExitProof { distribution: None, ..proof };
        assert_eq!(account_calls(staker, &none).len(), 1, "nothing to register, nothing to claim");
    }

    #[test]
    fn the_builder_reproduces_a_published_logs_root_whatever_signed_it() {
        let mut checkpoints = Vec::new();
        log::read_file(Path::new(REFERENCE), |entry| {
            if let LogEntry::Checkpoint(cp) = entry {
                checkpoints.push((cp.seq, cp.root));
            }
            Ok(())
        })
        .unwrap();
        let (seq, root) = checkpoints.pop().expect("the reference log checkpoints");
        let bundle = exit_bundle(Path::new(REFERENCE), seq, ExitSubject::House).unwrap();
        assert_eq!(format!("{:#x}", bundle.root), root);
        assert!(bundle.house_pending_deposits.iter().all(|pending| pending.verify(bundle.root)));
        assert!(!bundle.house_pending_deposits.is_empty(), "the V38 session leaves a house deposit pending");
        let error = exit_bundle(Path::new(REFERENCE), seq + 1, ExitSubject::House).unwrap_err();
        assert!(error.to_string().contains("no checkpoint at seq"), "{error:#}");
    }
}
