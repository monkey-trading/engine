//! Sequenced frontier admission, refusal, financial state and replay contracts.

mod common;

use alloy_primitives::B256;
use common::{Harness, acct, f};
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use serde_json::{Value, json};

#[test]
fn sequenced_frontier_refusals_preserve_receipts_cash_locks_and_coverage_after_replay() {
    let mut h = baseline();
    let mut replayed = Engine::new();
    for command in &h.log {
        replayed.apply(command).unwrap();
    }
    assert_eq!(replayed.state_root(), h.engine.state_root());
    for replay in [false, true] {
        for (name, rejected) in [
            ("pending at boundary", checkpoint(10, 10, 2)),
            ("pending below boundary", checkpoint(11, 11, 3)),
            ("height decreases", checkpoint(4, 4, 1)),
            ("count decreases at a newer height", checkpoint(8, 8, 0)),
            ("same height changes hash", checkpoint(6, 7, 1)),
            ("same height changes count", checkpoint(6, 6, 2)),
            ("zero block hash", checkpoint(8, 0, 1)),
        ] {
            let mut engine = fresh(&h.log, replay);
            assert_eq!(engine.state_root(), h.engine.state_root());
            assert_refused(&mut engine, rejected, name);
        }
        let mut settled = fresh(&h.log, replay);
        settle(&mut settled, &receipt(2, 10, false, "100"));
        assert_refused(&mut settled, checkpoint(11, 11, 3), "pending house receipt at boundary");
        settle(&mut settled, &receipt(3, 11, true, "30"));
        assert_refused(&mut settled, checkpoint(u64::MAX, 1, 3), "maximum height without pending receipts");
        let before = financial(&settled);
        assert!(send(&mut settled, Command::CustodyFinalised(checkpoint(11, 11, 3))).is_empty());
        assert_eq!(settled.custody_checkpoint(), Some(checkpoint(11, 11, 3)));
        assert_eq!(settled.custody_deposit_coverage(), Some(3));
        assert_eq!(financial(&settled), before, "settling the frontier never applies cash again");
        let stable = disposition(&settled);
        assert!(send(&mut settled, Command::CustodyFinalised(checkpoint(11, 11, 3))).is_empty());
        assert_eq!(disposition(&settled), stable, "an identical frontier is idempotent");
    }
}

/// Rebuilds each independent case through live commands or the recorded sequenced transcript.
fn fresh(log: &[Sequenced], replay: bool) -> Engine {
    if !replay {
        return baseline().engine;
    }
    let mut engine = Engine::new();
    for command in log {
        engine.apply(command).unwrap();
    }
    engine
}

#[test]
fn inactive_protocol_refuses_frontiers_and_active_protocol_accepts_zero_height_and_count() {
    for version in [ProtocolVersion::V17, ProtocolVersion::V18] {
        let mut h = Harness::v17();
        if version == ProtocolVersion::V18 {
            h.admin(AdminCommand::ActivateProtocol { version });
        }
        h.deposit(acct(1), "50");
        assert!(!h.engine.deposit_finality_enabled());
        assert_refused(&mut h.engine, checkpoint(0, 1, 0), "inactive protocol");
    }
    let mut h = active();
    h.deposit(acct(1), "50");
    let before = financial(&h.engine);
    assert!(send(&mut h.engine, Command::CustodyFinalised(checkpoint(0, 1, 0))).is_empty());
    assert_eq!(h.engine.custody_checkpoint(), Some(checkpoint(0, 1, 0)));
    assert_eq!(h.engine.custody_deposit_coverage(), Some(0));
    assert_eq!(financial(&h.engine), before);
}

/// Creates a real finalized credit and pending trader/house receipts above it.
fn baseline() -> Harness {
    let mut h = active();
    let finalised = receipt(1, 5, false, "50");
    assert!(h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: finalised.clone() }))).contains(
        &Event::CustodyDepositChanged {
            receipt: finalised.clone(),
            status: DepositStatus::Unfinalised,
            principal_delta: f("50"),
        }
    ));
    let observed_at_seq = h.engine.recorded_deposit(&finalised.reference()).unwrap().updated_at_seq;
    h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt: finalised, observed_at_seq })));
    for receipt in [receipt(2, 10, false, "100"), receipt(3, 11, true, "30")] {
        let amount = receipt.amount;
        assert!(h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() }))).contains(
            &Event::CustodyDepositChanged { receipt, status: DepositStatus::Unfinalised, principal_delta: amount }
        ));
    }
    assert!(h.cmd(Command::CustodyFinalised(checkpoint(6, 6, 1))).is_empty());
    assert_eq!(h.engine.custody_deposit_coverage(), Some(3));
    assert_eq!(
        (h.engine.accounts()[&acct(1)].balance, h.engine.accounts()[&acct(1)].unfinalised_deposits),
        (f("150"), f("100"))
    );
    h
}

/// Activates the actual released protocol without editing engine fields or historical inputs.
fn active() -> Harness {
    let mut h = Harness::v17();
    h.admin(AdminCommand::ActivateCustodyFinality { version: ProtocolVersion::V19, legacy_deposits: Vec::new() });
    assert!(h.engine.deposit_finality_enabled());
    h
}

/// Checks refusal changes neither canonical disposition nor any financial component.
fn assert_refused(engine: &mut Engine, rejected: CustodyCheckpoint, name: &str) {
    let before = disposition(engine);
    assert_eq!(
        send(engine, Command::CustodyFinalised(rejected)),
        vec![Event::AdminRejected { reason: "invalid custody finalised frontier".into() }],
        "{name}"
    );
    assert_eq!(disposition(engine), before, "{name}");
}

/// Finalizes the exact recorded revision and verifies there is no new cash application.
fn settle(engine: &mut Engine, receipt: &DepositReceipt) {
    let before = engine.accounts()[&acct(1)].balance;
    let house_before = engine.house_cash();
    let observed_at_seq = engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    assert!(
        send(
            engine,
            Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq }))
        )
        .contains(&Event::CustodyDepositChanged {
            receipt: receipt.clone(),
            status: DepositStatus::Finalised,
            principal_delta: f("0"),
        })
    );
    assert_eq!((engine.accounts()[&acct(1)].balance, engine.house_cash()), (before, house_before));
}

/// Stamps a command deterministically at the engine's current time.
fn send(engine: &mut Engine, cmd: Command) -> Vec<Event> {
    engine.apply(&Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: engine.now(), cmd }).unwrap()
}

/// Captures cash and locks independently of the frontier/index fields.
fn financial(engine: &Engine) -> Value {
    let raw = serde_json::to_value(engine).unwrap();
    json!({"accounts": engine.accounts(), "house_cash": engine.house_cash(),
        "house_lock": raw.get("unfinalised_house_deposits").cloned().unwrap_or(json!("0"))})
}

/// Captures authoritative receipt state and its current canonical coverage.
fn disposition(engine: &Engine) -> Value {
    let raw = serde_json::to_value(engine).unwrap();
    json!({"financial": financial(engine), "receipts": raw.get("deposits"),
        "frontier": engine.custody_checkpoint(), "coverage": engine.custody_deposit_coverage(),
        "pending": engine.unfinalised_deposit_records(0, u64::MAX)})
}

/// Explicit deterministic protocol inputs, not observations of a live chain.
fn receipt(id: u64, block: u64, house: bool, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block,
        block_hash: B256::repeat_byte(block as u8),
        tx_hash: B256::repeat_byte(id as u8),
        log_index: id,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: if house { DepositBeneficiary::House } else { DepositBeneficiary::Trader { account: acct(1) } },
        amount: f(amount),
    }
}

/// An explicit proposed frontier whose nonzero hash is unrelated to its receipt count.
fn checkpoint(block: u64, hash: u8, deposit_count: u64) -> CustodyCheckpoint {
    CustodyCheckpoint { block, block_hash: B256::repeat_byte(hash), deposit_count }
}
