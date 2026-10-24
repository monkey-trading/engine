//! Whole-USDC withdrawal reservations and immutable payouts, processed in public FIFO order.

use std::ops::Bound::{Excluded, Unbounded};

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::merkle::withdrawal_approval_key;
use crate::types::AccountId;

/// A protocol work bound: one command creates at most this many approval leaves and payouts.
pub const MAX_WITHDRAWAL_APPROVALS_PER_COMMAND: usize = 16;
/// A query work bound: each page examines at most this many requests plus one continuation probe.
pub const MAX_WITHDRAWAL_QUEUE_PAGE_SIZE: usize = 200;
/// One USDC base unit (six decimals) in the engine's nine-decimal representation.
const USDC_BASE_UNIT_RAW: i128 = 1_000;

/// A fully reserved request, ordered by its sequence, with any approved parts removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuedWithdrawal {
    pub account: AccountId,
    pub amount: Fixed,
    pub remaining: Fixed,
    pub requested_seq: u64,
    /// Contiguous enqueue order, including completed requests; only FIFO heads can leave the queue.
    #[serde(default, skip_serializing_if = "crate::merkle::state::is_zero")]
    pub ticket: u64,
}

/// A bounded global scan, optionally filtered by account, with exact global one-based positions.
#[derive(Debug)]
pub struct WithdrawalQueuePage<'a> {
    pub queue: Vec<(u64, &'a QueuedWithdrawal)>,
    /// Resume after this scanned request, even when the account filter returned an empty page.
    pub next: Option<u64>,
}

/// An immutable payout. Its leaf binds the recipient, exact USDC amount, nonce and approval seq.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithdrawalApproval {
    pub account: AccountId,
    pub amount: Fixed,
    pub nonce: u64,
    pub approved_seq: u64,
}

impl WithdrawalApproval {
    /// The exact amount to sign and transfer, in whole USDC base units.
    pub fn amount_usdc(&self) -> u128 {
        usdc_amount(self.amount).expect("an approval contains whole nonnegative USDC base units")
    }
}

impl Engine {
    /// Reserved requests in public FIFO order, oldest first. Approved parts are no longer queued.
    pub fn withdrawal_queue(&self) -> impl DoubleEndedIterator<Item = &QueuedWithdrawal> {
        self.withdrawal_queue.values()
    }

    /// Seeks past `after`, scans `limit` requests, then filters by account. Limits clamp to 1..=200;
    /// continue with `next` even after an empty filtered page. Reads at most `limit + 1` entries.
    pub fn withdrawal_queue_page(
        &self,
        account: Option<AccountId>,
        after: Option<u64>,
        limit: usize,
    ) -> WithdrawalQueuePage<'_> {
        let limit = limit.clamp(1, MAX_WITHDRAWAL_QUEUE_PAGE_SIZE);
        let head_ticket = self.withdrawal_head_ticket();
        let mut requests = self.withdrawal_queue.range((after.map_or(Unbounded, Excluded), Unbounded));
        let mut queue = Vec::new();
        let mut last_scanned = None;
        for (&seq, request) in requests.by_ref().take(limit) {
            last_scanned = Some(seq);
            if account.is_none_or(|account| account == request.account) {
                queue.push((request.ticket - head_ticket + 1, request));
            }
        }
        let next = if requests.next().is_some() { last_scanned } else { None };
        WithdrawalQueuePage { queue, next }
    }

    /// Looks up a queued request's current global one-based position without scanning predecessors.
    pub fn withdrawal_queue_position(&self, requested_seq: u64) -> Option<u64> {
        let request = self.withdrawal_queue.get(&requested_seq)?;
        Some(request.ticket - self.withdrawal_head_ticket() + 1)
    }

    /// Derives the head ticket in constant time: only heads leave a contiguous FIFO ticket range.
    fn withdrawal_head_ticket(&self) -> u64 {
        let queued = u64::try_from(self.withdrawal_queue.len()).expect("queue length fits its u64 ticket space");
        self.withdrawal_next_ticket - queued
    }

    /// All immutable approvals, in leaf-key order. Custody evidence determines which remain unpaid.
    pub fn withdrawal_approvals(&self) -> impl Iterator<Item = &WithdrawalApproval> {
        self.withdrawal_approvals.values()
    }

    /// The immutable payout for an account and its payout nonce, if approved under V22.
    pub fn withdrawal_approval(&self, account: AccountId, nonce: u64) -> Option<&WithdrawalApproval> {
        self.withdrawal_approvals.get(&withdrawal_approval_key(account, nonce))
    }

    /// Conserved unapproved physical cash, or `None` before V22. Only its positive whole-USDC
    /// portion is payable; a negative legacy gap must be covered before further approvals.
    pub fn withdrawal_cash(&self) -> Option<Fixed> {
        self.withdrawal_cash
    }

    /// Debits the entire reservation from trading cash before any part can be approved.
    pub(crate) fn reserve_withdrawal(&mut self, account: AccountId, amount: Fixed, events: &mut Vec<Event>) {
        let requested_seq = self.last_seq.expect("a withdrawal is sequenced");
        self.accounts.get_mut(&account).expect("a funded withdrawal has an account").balance -= amount;
        let ticket = self.withdrawal_next_ticket;
        self.withdrawal_next_ticket = ticket.checked_add(1).expect("enqueue tickets cannot exhaust before seqs");
        let queued = QueuedWithdrawal { account, amount, remaining: amount, requested_seq, ticket };
        assert!(self.withdrawal_queue.insert(requested_seq, queued).is_none(), "one withdrawal request per seq");
        events.push(Event::WithdrawalQueued { account, amount, requested_seq });
    }

    /// Approves at most a fixed number of FIFO heads, partially when physical cash is short.
    /// Trading changes ledger ownership but never manufactures physical payout capacity.
    pub(crate) fn process_withdrawal_queue(&mut self, events: &mut Vec<Event>) {
        for _ in 0..MAX_WITHDRAWAL_APPROVALS_PER_COMMAND {
            let Some((&requested_seq, &head)) = self.withdrawal_queue.first_key_value() else { break };
            let cash = self.withdrawal_cash.expect("V22 initializes physical cash on activation");
            let amount = head.remaining.min(whole_usdc(cash.max(Fixed::ZERO)));
            if !amount.is_positive() {
                break;
            }
            let acct = self.accounts.get_mut(&head.account).expect("a queued account exists");
            acct.withdrawal_nonce = acct.withdrawal_nonce.checked_add(1).expect("payout nonce cannot exhaust");
            let approval = WithdrawalApproval {
                account: head.account,
                amount,
                nonce: acct.withdrawal_nonce,
                approved_seq: self.last_seq.expect("approval follows a command"),
            };
            let key = withdrawal_approval_key(approval.account, approval.nonce);
            assert!(self.withdrawal_approvals.insert(key, approval).is_none(), "payout nonces are unique");
            self.withdrawal_cash = Some(cash - amount);
            let remaining = head.remaining - amount;
            if remaining.is_zero() {
                self.withdrawal_queue.remove(&requested_seq);
            } else {
                self.withdrawal_queue.get_mut(&requested_seq).expect("the FIFO head exists").remaining = remaining;
            }
            events.push(Event::WithdrawalApproved { account: approval.account, amount, nonce: approval.nonce });
            events.push(Event::WithdrawalQueueUpdated { account: approval.account, requested_seq, remaining });
        }
    }
}

/// Whole USDC base units exactly representing `amount`, or `None` for negatives or precision loss.
pub fn usdc_amount(amount: Fixed) -> Option<u128> {
    if amount.raw() % USDC_BASE_UNIT_RAW != 0 {
        return None;
    }
    u128::try_from(amount.raw() / USDC_BASE_UNIT_RAW).ok()
}

/// An exact engine amount from whole USDC base units, or `None` if it exceeds the representation.
pub fn from_usdc(amount: u128) -> Option<Fixed> {
    i128::try_from(amount).ok()?.checked_mul(USDC_BASE_UNIT_RAW).map(Fixed::from_raw)
}

/// Rounds toward zero to whole USDC base units; positive request dust stays in account cash.
pub(crate) fn whole_usdc(amount: Fixed) -> Fixed {
    Fixed::from_raw(amount.raw() / USDC_BASE_UNIT_RAW * USDC_BASE_UNIT_RAW)
}
