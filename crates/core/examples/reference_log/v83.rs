//! The session's `V83` part: a reorganisation's receipt moves as one command, a write-off repaid by the
//! same deposit's next credit, and epochs closing in turn to their own ends. A vote activates the
//! version; the epochs its notice spanned close in turn, oldest first, none skipped, once the mirror
//! passes their ends. A client's two deposits, credited at consecutive vault ids, are included again one
//! id later while the client holds a BTC long: one `custody_reorganisation` moves both, and the position
//! and cash stay. The client closes the long at a loss and both receipts are reversed, the second
//! reversal's close-out writing the loss off; the same deposit, included again, is credited less what
//! was written off. The mirror then passes an epoch's end five minutes late, and the next epoch counts
//! time in office from that end.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::receipt_access_deposit;
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate, ReceiptMove};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

use crate::common::close_spread::{attest, evidence, trade};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, Harness, f};
use crate::v56::pass;

/// The seed of the key this part deposits and trades through.
const CLIENT_SEED: u8 = 0x83;
/// The first of this part's proposal ids.
const PROPOSAL: u64 = 250;
/// The vault deposit id of this part's first receipt, past every earlier part's.
const FIRST_ID: u64 = 8_300_000;
const MINUTE_MS: Millis = 60 * 1000;

/// Activates `V83` by vote; then the moves, the write-off and the held close described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V83 };
    let events = pass(h, PROPOSAL, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let mut events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: PROPOSAL, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V83);
    // An activation leaves the rollovers its notice spanned to the next command.
    events.extend(h.cmd(Command::Tick));
    events.extend(frontier(h, h.ts));
    events.extend(h.cmd(Command::Tick));
    let closed = closes(&events);
    assert!(closed.len() > 1 && closed.windows(2).all(|pair| pair[1] == pair[0] + 1), "{closed:?}");
    assert!(!events.iter().any(|event| matches!(event, Event::DistributionEpochsSkipped { .. })));
    let staking = h.engine.staking().expect("the session staked");
    assert_eq!(staking.epoch, staking.config.epochs_elapsed(h.ts), "every epoch the mirror passed closed");

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    let moved = reorganisation(h, &client);
    write_off(h, &client, &moved);
    held_close(h);
}

/// Credits the client's two deposits at consecutive ids and opens a long, then moves both one id up in
/// one reorganisation. Returns the receipts as they stand after it.
fn reorganisation(h: &mut Harness, client: &Client) -> [DepositReceipt; 2] {
    let originals = [receipt(client, FIRST_ID, 0), receipt(client, FIRST_ID + 1, 1)];
    for receipt in &originals {
        let events = h.cmd(credit(h, receipt, client));
        assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { .. })), "{events:?}");
    }
    let price = h.engine.markets()[&BTC].mark.expect("BTC has traded").to_string();
    h.price(BTC, &price);
    only_fill(&trade(h, client, BTC, Side::Buy, "0.01", &price));
    let (cash, equity) = (h.balance(client.account), h.equity_and_margin(client.account).0);
    let moved = originals.clone().map(|original| DepositReceipt {
        deposit_id: original.deposit_id + 1,
        block: original.block + 10,
        block_hash: B256::repeat_byte(0x93),
        ..original
    });
    let moves = originals
        .iter()
        .zip(&moved)
        .map(|(original, replacement)| {
            let deposit = receipt_access_deposit(replacement, Some(client.account));
            ReceiptMove {
                receipt: original.clone(),
                observed_at_seq: h.engine.recorded_deposit(&original.reference()).expect("credited").updated_at_seq,
                replacement: replacement.clone(),
                replacement_reversed_at_seq: None,
                payer: Some(client.account),
                evidence: Some(evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer)),
            }
        })
        .collect();
    let events = h.cmd(Command::CustodyReorganisation { moves });
    assert!(!events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. } | Event::CloseOut { .. })));
    assert_eq!((h.balance(client.account), h.equity_and_margin(client.account).0), (cash, equity));
    assert!(h.engine.accounts()[&client.account].positions.contains_key(&BTC));
    moved
}

/// Closes the long at a loss, reverses both moved receipts, the second leaving the client below zero, and
/// credits the second's deposit again in another block: it repays the house what was written off.
fn write_off(h: &mut Harness, client: &Client, moved: &[DepositReceipt; 2]) {
    let lower = h.engine.markets()[&BTC].mark.expect("BTC has traded").mul_bps(9_800).to_string();
    h.price(BTC, &lower);
    only_fill(&trade(h, client, BTC, Side::Sell, "0.01", &lower));
    let cash = h.balance(client.account);
    assert!(cash < f("2000"), "{cash}");
    let mut written_off = Fixed::ZERO;
    for receipt in moved {
        let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).expect("credited").updated_at_seq;
        let reverse = DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq };
        let events = h.cmd(Command::CustodyDeposit(Box::new(reverse)));
        written_off += events
            .iter()
            .filter_map(|event| match event {
                Event::BadDebt { account, amount } if *account == client.account => Some(*amount),
                _ => None,
            })
            .fold(Fixed::ZERO, |total, amount| total + amount);
    }
    assert_eq!(written_off, f("2000") - cash, "the second reversal writes the loss off");
    let again = DepositReceipt { block: moved[1].block + 10, block_hash: B256::repeat_byte(0x94), ..moved[1].clone() };
    let events = h.cmd(credit(h, &again, client));
    assert!(events.contains(&Event::WriteOffRepaid { account: client.account, amount: written_off }), "{events:?}");
    assert_eq!(h.balance(client.account), cash - f("1000"));
}

/// The mirror passes the open epoch's end five minutes late: the close splits the operator's share to the
/// end, and the next epoch counts from it.
fn held_close(h: &mut Harness) {
    let config = h.engine.staking().expect("the session staked").config;
    let end = config.epoch_end(config.epochs_elapsed(h.ts));
    h.advance_to(end + 5 * MINUTE_MS);
    assert!(closes(&h.cmd(Command::Tick)).is_empty(), "the close waits for the mirror");
    frontier(h, end);
    let events = h.cmd(Command::Tick);
    assert_eq!(closes(&events).len(), 1, "{events:?}");
    let office = h.engine.staking().and_then(|staking| staking.office.clone()).expect("V50 counts time in office");
    assert_eq!(office.since, end);
    assert!(office.served.values().all(|served| *served <= 5 * MINUTE_MS), "{office:?}");
}

/// A deposit of 1,000 USDC for `client` at vault id `id` in a block of its own, from the transaction
/// numbered `tx` in this part.
fn receipt(client: &Client, id: u64, tx: u8) -> DepositReceipt {
    DepositReceipt {
        block: id,
        block_hash: B256::repeat_byte(0x83),
        tx_hash: B256::repeat_byte(0x85 + tx),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account: client.account },
        amount: f("1000"),
    }
}

/// The custody watcher's screened credit of `receipt`, paid by the client.
fn credit(h: &Harness, receipt: &DepositReceipt, client: &Client) -> Command {
    let deposit = receipt_access_deposit(receipt, Some(client.account));
    let evidence = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
    Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(client.account), evidence }
}

/// The staking mirror completes a scan through a new block, stamped `time` on chain.
fn frontier(h: &mut Harness, time: Millis) -> Vec<Event> {
    let (block, block_hash) = (83_000_000 + h.log.len() as u64, B256::repeat_byte(0x83));
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(time), named_turn: None });
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    events
}

/// The epochs `events` closed, in order.
fn closes(events: &[Event]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::DistributionEpochClosed { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .collect()
}
