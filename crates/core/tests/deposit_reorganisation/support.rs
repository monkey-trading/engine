//! What the reorganisation and write-off tests share: credited vault deposits, the custody watcher's
//! commands for them, and readings of their effect.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::receipt_access_deposit;
use engine_core::command::Command;
use engine_core::deposits::{
    DepositBeneficiary, DepositReceipt, DepositReject, DepositStatus, DepositUpdate, ReceiptMove,
};
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};

use crate::common::close_spread::{attest, evidence, trade};
use crate::common::events::fills;
use crate::common::{BTC, Client, Harness, f};
use crate::support::harness;

pub const V83: ProtocolVersion = ProtocolVersion::V83;

/// A vault deposit of `amount` for `account`, funded by it, at vault id `id` in block `block`, whose
/// hash is `fork` repeated, from the transaction whose hash is `tx` repeated.
pub fn receipt(id: u64, block: u64, fork: u8, tx: u8, account: AccountId, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block,
        block_hash: B256::repeat_byte(fork),
        tx_hash: B256::repeat_byte(tx),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
    }
}

/// `old`'s deposit, unchanged, canonical again at vault id `id` in a block ten on whose hash is `fork`
/// repeated.
pub fn moved(old: &DepositReceipt, id: u64, fork: u8) -> DepositReceipt {
    DepositReceipt { deposit_id: id, block: old.block + 10, block_hash: B256::repeat_byte(fork), ..old.clone() }
}

/// The watcher's screened credit of a new canonical receipt funded by `payer`.
pub fn credit(h: &mut Harness, receipt: &DepositReceipt, payer: AccountId) -> Vec<Event> {
    let deposit = receipt_access_deposit(receipt, Some(payer));
    let ev = evidence(h, deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(payer), evidence: ev })
}

/// The watcher's screened single move of `orphan`, read at `observed_at_seq`, to `replacement`.
pub fn replace(
    h: &mut Harness,
    orphan: &DepositReceipt,
    observed_at_seq: u64,
    replacement: &DepositReceipt,
    payer: AccountId,
) -> Vec<Event> {
    let deposit = receipt_access_deposit(replacement, Some(payer));
    let ev = evidence(h, deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    let update = DepositUpdate::Replace {
        receipt: orphan.clone(),
        observed_at_seq,
        replacement: replacement.clone(),
        replacement_reversed_at_seq: None,
    };
    h.cmd(Command::AccessCustodyReplacement { update: Box::new(update), payer: Some(payer), evidence: ev })
}

/// The watcher's screened moves of each orphan to its replacement, funded by the payer named, each at
/// the orphan's current revision.
pub fn moves<'a>(
    h: &Harness,
    pairs: impl IntoIterator<Item = (&'a DepositReceipt, &'a DepositReceipt, AccountId)>,
) -> Vec<ReceiptMove> {
    pairs
        .into_iter()
        .map(|(orphan, replacement, payer)| {
            let deposit = receipt_access_deposit(replacement, Some(payer));
            ReceiptMove {
                receipt: orphan.clone(),
                observed_at_seq: h.engine.recorded_deposit(&orphan.reference()).map_or(0, |d| d.updated_at_seq),
                replacement: replacement.clone(),
                replacement_reversed_at_seq: None,
                payer: Some(payer),
                evidence: Some(evidence(h, deposit_screening_hash(&deposit), deposit.account, deposit.payer)),
            }
        })
        .collect()
}

/// Sequences `moves` as one reorganisation.
pub fn reorganise(h: &mut Harness, moves: Vec<ReceiptMove>) -> Vec<Event> {
    h.cmd(Command::CustodyReorganisation { moves })
}

/// The revision a receipt was last changed at.
pub fn revision(h: &Harness, receipt: &DepositReceipt) -> u64 {
    h.engine.recorded_deposit(&receipt.reference()).expect("recorded").updated_at_seq
}

/// Why the transition among `events` was refused, if it was.
pub fn rejected(events: &[Event]) -> Option<DepositReject> {
    events.iter().find_map(|event| match event {
        Event::CustodyDepositRejected { reason, .. } => Some(reason.clone()),
        _ => None,
    })
}

/// The references of the receipts `events` reversed, in order.
pub fn reversed(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::CustodyDepositChanged { receipt, status: DepositStatus::Reversed, .. } => Some(receipt.reference()),
            _ => None,
        })
        .collect()
}

/// Every account's cash and the house's together, which a move leaves where it was.
pub fn total_cash(h: &Harness) -> Fixed {
    h.engine.accounts().values().fold(h.engine.house_cash(), |total, account| total + account.balance)
}

/// Two declared clients whose deposits of 1,000 USDC are credited at vault ids 0 and 1, at `version`,
/// and wait for Ethereum finality. With `long`, the second holds a long of 0.05 BTC opened at 100,000,
/// and BTC is at 97,000: 150 behind, and well within its margin on its own deposit.
pub fn credited_deposits(version: ProtocolVersion, long: bool) -> (Harness, [Client; 2], [DepositReceipt; 2]) {
    let mut h = harness(version);
    h.price(BTC, "100000");
    h.cmd(Command::Tick);
    let (first, second) = (Client::new(81), Client::new(82));
    attest(&mut h, &first);
    attest(&mut h, &second);
    let receipts =
        [receipt(0, 9_000, 0xa0, 0xb0, first.account, "1000"), receipt(1, 9_001, 0xa1, 0xb1, second.account, "1000")];
    for (receipt, client) in receipts.iter().zip([&first, &second]) {
        let events = credit(&mut h, receipt, client.account);
        assert_eq!(rejected(&events), None, "{events:?}");
        assert_eq!(h.balance(client.account), f("1000"));
    }
    if long {
        let opened = trade(&mut h, &second, BTC, Side::Buy, "0.05", "100000");
        assert_eq!(fills(&opened).len(), 1, "{opened:?}");
        h.price(BTC, "97000");
        let (equity, margin) = h.equity_and_margin(second.account);
        assert!(equity > margin && equity < f("860") && equity > f("800"), "solvent: {equity}, {margin}");
    }
    (h, [first, second], receipts)
}

/// Four declared clients whose deposits of 1,000 USDC are credited at vault ids 0 to 3 under `V83`,
/// each holding a long of 0.01 BTC.
pub fn four_credited_deposits() -> (Harness, Vec<Client>, Vec<DepositReceipt>) {
    let mut h = harness(V83);
    h.price(BTC, "100000");
    h.cmd(Command::Tick);
    let clients: Vec<Client> = (0..4).map(|n| Client::new(0x90 + n)).collect();
    let receipts: Vec<DepositReceipt> = clients
        .iter()
        .enumerate()
        .map(|(id, client)| {
            receipt(id as u64, 9_000 + id as u64, 0xa0 + id as u8, 0xb0 + id as u8, client.account, "1000")
        })
        .collect();
    for (receipt, client) in receipts.iter().zip(&clients) {
        attest(&mut h, client);
        assert_eq!(rejected(&credit(&mut h, receipt, client.account)), None);
        assert_eq!(fills(&trade(&mut h, client, BTC, Side::Buy, "0.01", "100000")).len(), 1);
    }
    (h, clients, receipts)
}

/// How many positions each client holds.
pub fn positions(h: &Harness, clients: &[Client]) -> Vec<usize> {
    clients.iter().map(|client| h.engine.accounts()[&client.account].positions.len()).collect()
}
