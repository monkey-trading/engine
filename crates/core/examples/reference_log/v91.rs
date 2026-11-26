//! The session's `V91` part: the state root binds each pending and held deposit's whole record. Before the version,
//! an encoding of the engine whose pending house receipt, from the `V38` part, names another reference inside its
//! record, or whose held receipt, from the `V77` part, names another payer or reason, decodes to an engine with the
//! same root. A vote activates the version, and the same changes then move the root, while both receipts' exit
//! leaves keep the form the vault checks. Under it a client's receipt waits on an unavailable screening source and is
//! credited on a retry, and another is held, each record leaf written and removed as the deposit changes, as an
//! engine decoded from the encoding computes it afresh.

use alloy_primitives::B256;
use engine_core::Engine;
use engine_core::access::{AccessEvidence, SourceFailure, receipt_access_deposit};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::merkle::ReceiptKind;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use serde_json::Value;

use crate::common::close_spread::{attest, evidence};
use crate::common::{Client, Harness, f};
use crate::v77::hold;
use crate::v81::{applied, scheduled};

/// This part's proposal id.
const PROPOSAL: u64 = 301;
/// The seed of the key this part deposits through.
const CLIENT_SEED: u8 = 0x91;
/// The vault deposit id of this part's first receipt, past every earlier part's.
const FIRST_ID: u64 = 9_100_000;
/// The `V38` part's pending house receipt.
const PENDING_ID: u64 = 2_000_000;
/// The `V77` part's held receipt.
const HELD_ID: u64 = 7_800_002;

/// Activates `V91` by vote; then the records described above.
pub(crate) fn session(h: &mut Harness) {
    let pending = reference(h, PENDING_ID, ReceiptKind::Pending);
    let held = reference(h, HELD_ID, ReceiptKind::Held);
    let unbound = |engine: &Engine| {
        [
            rebuilt(engine, |state| state["pending_deposits"][&pending]["chain_ref"] = "0xv91:elsewhere".into()),
            rebuilt(engine, |state| state["held_deposits"][&held]["deposit"]["payer"] = Value::Null),
            rebuilt(engine, |state| state["held_deposits"][&held]["reason"] = "payer_unknown".into()),
        ]
    };
    let root = h.engine.state_root();
    assert_eq!(unbound(&h.engine), [root; 3], "before V91 the root leaves the records' other fields out");

    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V91 };
    scheduled(h, PROPOSAL, &activation);
    h.advance(NOTICE_MS);
    applied(h, PROPOSAL, activation);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V91);
    // An activation leaves the rollovers its notice spanned to the next command.
    h.cmd(Command::Tick);
    let root = h.engine.state_root();
    assert_eq!(rebuilt(&h.engine, |_| {}), root, "the activation commits every record");
    assert!(unbound(&h.engine).iter().all(|moved| *moved != root), "each change moves the root");
    for (id, kind) in [(PENDING_ID, ReceiptKind::Pending), (HELD_ID, ReceiptKind::Held)] {
        let proof = h.engine.receipt_proof(id).expect("the receipt is proven");
        assert!(proof.kind == kind && proof.verify(B256::from(root)), "{proof:?}");
    }

    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    attest(h, &client);
    let waiting = receipt(FIRST_ID, client.account, "40");
    let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
    let command =
        Command::AccessCustodyDeposit { receipt: waiting.clone(), payer: Some(client.account), evidence: unavailable };
    let events = h.cmd(command);
    assert!(events.iter().any(|event| matches!(event, Event::DepositPending { .. })), "{events:?}");
    let root = h.engine.state_root();
    assert_eq!(rebuilt(&h.engine, |_| {}), root, "the pending record is written");
    let moved = rebuilt(&h.engine, |state| {
        state["pending_deposits"][waiting.reference()]["chain_ref"] = "0xv91:elsewhere".into()
    });
    assert_ne!(moved, root);

    let deposit = receipt_access_deposit(&waiting, Some(client.account));
    let cleared = evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, Some(client.account));
    h.cmd(Command::RetryDepositScreening { chain_ref: waiting.reference(), evidence: cleared });
    assert!(!h.engine.pending_deposits().contains_key(&waiting.reference()), "the retry credits it");
    let root = h.engine.state_root();
    assert_eq!(rebuilt(&h.engine, |_| {}), root, "the pending record is removed");

    let sanctioned = receipt(FIRST_ID + 1, client.account, "30");
    hold(h, &sanctioned, AccountId([0x91; 20]));
    let root = h.engine.state_root();
    assert_eq!(rebuilt(&h.engine, |_| {}), root, "the held record is written");
    let proof = h.engine.receipt_proof(FIRST_ID + 1).expect("the held receipt");
    assert!(proof.kind == ReceiptKind::Held && proof.verify(B256::from(root)), "{proof:?}");
}

/// The reference of the receipt at vault deposit id `id`, which must be of `kind`.
fn reference(h: &mut Harness, id: u64, kind: ReceiptKind) -> String {
    h.engine.state_root();
    let proof = h.engine.receipt_proof(id).expect("the receipt is proven");
    assert_eq!(proof.kind, kind);
    proof.chain_ref
}

/// The root of an engine decoded from `engine`'s encoding changed by `alter`, as a start decodes a snapshot's.
fn rebuilt(engine: &Engine, alter: impl FnOnce(&mut Value)) -> [u8; 32] {
    let mut state = serde_json::to_value(engine).expect("the engine encodes");
    alter(&mut state);
    let mut engine: Engine = serde_json::from_value(state).expect("the changed encoding decodes");
    engine.rebuild_resting();
    engine.restore_indexes();
    engine.state_root()
}

/// A receipt for `account`'s trading account at vault deposit id `id`, in a block of its own.
fn receipt(id: u64, account: AccountId, amount: &str) -> DepositReceipt {
    let byte = u8::try_from(id - FIRST_ID).expect("a few receipts") + 0x91;
    DepositReceipt {
        block: id,
        block_hash: B256::repeat_byte(byte),
        tx_hash: B256::repeat_byte(byte.wrapping_add(0x10)),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
    }
}
