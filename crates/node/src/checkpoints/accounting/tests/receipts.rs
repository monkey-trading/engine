//! Receipt-output contracts and a state-based oracle through actual V20 execution.
use super::*;
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};

#[test]
fn adoption_finality_reversal_replacement_and_unpaid_funds_have_exact_net_effects() {
    let mut a = Accounting::default();
    a.apply(&deposit(100, "0xlegacy"), &[Event::Deposited { account: TRADER, amount: Fixed::from_int(100) }]).unwrap();
    a.apply(&deposit(50, "mock-credit"), &[Event::Deposited { account: TRADER, amount: Fixed::from_int(50) }]).unwrap();
    let legacy = receipt(0, DepositBeneficiary::Trader { account: TRADER }, 100);
    let house = receipt(1, DepositBeneficiary::House, 70);
    let replacement = DepositReceipt {
        tx_hash: engine_core::eip712::B256::repeat_byte(9),
        block: 9,
        log_index: 3,
        beneficiary: DepositBeneficiary::Trader { account: TRADER },
        ..house.clone()
    };
    let transitions = [
        (DepositUpdate::Credit { receipt: legacy.clone() }, DepositStatus::Unfinalised, 0, (150, 1)),
        (DepositUpdate::Finalise { receipt: legacy, observed_at_seq: 1 }, DepositStatus::Finalised, 0, (150, 1)),
        (DepositUpdate::Credit { receipt: house.clone() }, DepositStatus::Unfinalised, 70, (220, 2)),
        (DepositUpdate::Reverse { receipt: house, observed_at_seq: 2 }, DepositStatus::Reversed, -70, (150, 1)),
        (DepositUpdate::Credit { receipt: replacement.clone() }, DepositStatus::Unfinalised, 70, (220, 2)),
        (
            DepositUpdate::Reverse { receipt: replacement.clone(), observed_at_seq: 3 },
            DepositStatus::Reversed,
            -70,
            (150, 1),
        ),
        (
            DepositUpdate::Reinstate { receipt: replacement.clone(), reversed_at_seq: 4 },
            DepositStatus::Unfinalised,
            70,
            (220, 2),
        ),
        (DepositUpdate::Finalise { receipt: replacement, observed_at_seq: 5 }, DepositStatus::Finalised, 0, (220, 2)),
    ];
    a.apply(
        &Command::Withdraw { account: TRADER, amount: Fixed::from_int(25), auth: None },
        &[Event::WithdrawalApproved { account: TRADER, amount: Fixed::from_int(25), nonce: 1 }],
    )
    .unwrap();
    for (update, status, delta, (liabilities, count)) in transitions {
        let mut events = vec![changed(&update, status, delta)];
        if delta < 0 {
            events.push(Event::BadDebt { account: TRADER, amount: Fixed::from_int(45) });
        }
        let command = Command::CustodyDeposit(Box::new(update));
        a.apply(&command, &events).unwrap();
        assert_eq!((a.liabilities, a.deposits, a.unpaid()), (Fixed::from_int(liabilities), count, Fixed::from_int(25)));
        a.apply(&command, &[]).unwrap();
        assert_eq!(
            (a.liabilities, a.deposits),
            (Fixed::from_int(liabilities), count),
            "a duplicate has no computed output"
        );
    }
}

#[test]
fn invalid_receipt_output_and_count_or_cash_overflow_publish_no_partial_batch() {
    let d = receipt(0, DepositBeneficiary::Trader { account: TRADER }, 1);
    let credit = DepositUpdate::Credit { receipt: d.clone() };
    let reverse = DepositUpdate::Reverse { receipt: d.clone(), observed_at_seq: 1 };
    let larger_reverse = DepositUpdate::Reverse {
        receipt: DepositReceipt { amount: Fixed::from_int(2), ..d.clone() },
        observed_at_seq: 1,
    };
    let moved = DepositReceipt { block: 9, log_index: 4, ..d.clone() };
    let replace = DepositUpdate::Replace {
        receipt: d.clone(),
        observed_at_seq: 1,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    let moved_reversed = Event::CustodyDepositChanged {
        receipt: moved,
        status: DepositStatus::Reversed,
        principal_delta: -Fixed::ONE,
        uncredited: None,
    };
    let ordinary = Accounting {
        liabilities: Fixed::from_int(100),
        deposits: 1,
        unpaid: Fixed::from_int(5),
        ..Accounting::default()
    };
    let cases = [
        (ordinary, Command::CustodyDeposit(Box::new(replace.clone())), moved_reversed),
        (ordinary, Command::CustodyDeposit(Box::new(replace)), changed(&credit, DepositStatus::Unfinalised, 1)),
        (ordinary, Command::Tick, changed(&credit, DepositStatus::Unfinalised, 1)),
        (ordinary, Command::CustodyDeposit(Box::new(credit.clone())), changed(&credit, DepositStatus::Finalised, 1)),
        (ordinary, Command::CustodyDeposit(Box::new(credit.clone())), changed(&credit, DepositStatus::Unfinalised, 2)),
        (
            Accounting { deposits: u64::MAX, ..ordinary },
            Command::CustodyDeposit(Box::new(credit.clone())),
            changed(&credit, DepositStatus::Unfinalised, 1),
        ),
        (
            Accounting { deposits: 0, ..ordinary },
            Command::CustodyDeposit(Box::new(reverse.clone())),
            changed(&reverse, DepositStatus::Reversed, -1),
        ),
        (
            Accounting { liabilities: Fixed::from_raw(i128::MAX - Fixed::ONE.raw()), ..ordinary },
            Command::CustodyDeposit(Box::new(credit.clone())),
            changed(&credit, DepositStatus::Unfinalised, 1),
        ),
        (
            Accounting { liabilities: Fixed::from_raw(i128::MIN), ..ordinary },
            Command::CustodyDeposit(Box::new(larger_reverse.clone())),
            changed(&larger_reverse, DepositStatus::Reversed, -2),
        ),
    ];
    for (initial, command, event) in cases {
        let mut a = initial;
        // A representable prefix must also roll back when the later receipt cannot be represented.
        let prefix = Event::HouseCapitalMoved { amount: Fixed::ONE, nav: Fixed::ONE };
        assert!(a.apply(&command, &[prefix, event]).is_err());
        assert_eq!(a, initial);
    }
    let mut wrong = d.clone();
    wrong.beneficiary = DepositBeneficiary::House;
    let mut a = ordinary;
    let fake = Event::CustodyDepositChanged {
        receipt: wrong,
        status: DepositStatus::Unfinalised,
        principal_delta: Fixed::ONE,
        uncredited: None,
    };
    assert!(
        a.apply(&Command::CustodyDeposit(Box::new(credit)), &[fake]).is_err(),
        "equal amount is not equal receipt identity"
    );
    assert_eq!(a, ordinary);
}

#[test]
fn state_scan_counts_adoption_orphans_moved_reinclusion_finality_and_reinstatement_once() {
    use engine_core::deposits::LegacyDeposit;

    let (mut engine, mut accounting) = (Engine::new(), Accounting::default());
    for command in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(dev_config().operator().unwrap()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 },
    ] {
        apply_engine(&mut engine, &mut accounting, Command::Admin(command));
    }
    let original = receipt(0, DepositBeneficiary::Trader { account: TRADER }, 100);
    let command = Command::Deposit {
        account: TRADER,
        amount: original.amount,
        chain_ref: format!("{}:{}", original.tx_hash, original.log_index),
    };
    let events = apply_engine(&mut engine, &mut accounting, command.clone());
    let legacy = LegacyDeposit::from_command(&command, &events).expect("actual successful legacy credit");
    apply_engine(
        &mut engine,
        &mut accounting,
        Command::Admin(AdminCommand::ActivateCustodyFinality {
            version: ProtocolVersion::V20,
            legacy_deposits: vec![legacy],
        }),
    );
    apply_engine(
        &mut engine,
        &mut accounting,
        Command::CustodyFinalised(engine_core::deposits::CustodyCheckpoint {
            block: 0,
            block_hash: engine_core::eip712::B256::repeat_byte(1),
            deposit_count: 0,
        }),
    );
    let mut update = |engine: &mut Engine, update| {
        apply_engine(engine, &mut accounting, Command::CustodyDeposit(Box::new(update)));
        accounting.deposits
    };
    assert_eq!(update(&mut engine, DepositUpdate::Credit { receipt: original.clone() }), 1);
    let observed_at_seq = engine.recorded_deposit(&original.reference()).unwrap().updated_at_seq;
    assert_eq!(update(&mut engine, DepositUpdate::Reverse { receipt: original.clone(), observed_at_seq }), 0);
    let replacement =
        DepositReceipt { block: 9, block_hash: engine_core::eip712::B256::repeat_byte(9), log_index: 3, ..original };
    assert_eq!(update(&mut engine, DepositUpdate::Credit { receipt: replacement.clone() }), 1);
    let observed_at_seq = engine.recorded_deposit(&replacement.reference()).unwrap().updated_at_seq;
    assert_eq!(update(&mut engine, DepositUpdate::Finalise { receipt: replacement, observed_at_seq }), 1);

    let house = receipt(1, DepositBeneficiary::House, 50);
    assert_eq!(update(&mut engine, DepositUpdate::Credit { receipt: house.clone() }), 2);
    let observed_at_seq = engine.recorded_deposit(&house.reference()).unwrap().updated_at_seq;
    assert_eq!(update(&mut engine, DepositUpdate::Reverse { receipt: house.clone(), observed_at_seq }), 1);
    let reversed_at_seq = engine.recorded_deposit(&house.reference()).unwrap().updated_at_seq;
    assert_eq!(update(&mut engine, DepositUpdate::Reinstate { receipt: house.clone(), reversed_at_seq }), 2);
    let observed_at_seq = engine.recorded_deposit(&house.reference()).unwrap().updated_at_seq;
    assert_eq!(update(&mut engine, DepositUpdate::Finalise { receipt: house, observed_at_seq }), 2);

    // A reincluded deposit moves to its replacement and back without changing the count.
    let moving = receipt(4, DepositBeneficiary::Trader { account: TRADER }, 30);
    assert_eq!(update(&mut engine, DepositUpdate::Credit { receipt: moving.clone() }), 3);
    let moved = DepositReceipt {
        block: 8,
        block_hash: engine_core::eip712::B256::repeat_byte(8),
        log_index: 6,
        ..moving.clone()
    };
    let observed_at_seq = engine.recorded_deposit(&moving.reference()).unwrap().updated_at_seq;
    let away = DepositUpdate::Replace {
        receipt: moving.clone(),
        observed_at_seq,
        replacement: moved.clone(),
        replacement_reversed_at_seq: None,
    };
    assert_eq!(update(&mut engine, away), 3);
    let observed_at_seq = engine.recorded_deposit(&moved.reference()).unwrap().updated_at_seq;
    let reversed_at_seq = engine.recorded_deposit(&moving.reference()).unwrap().updated_at_seq;
    let back = DepositUpdate::Replace {
        receipt: moved,
        observed_at_seq,
        replacement: moving,
        replacement_reversed_at_seq: Some(reversed_at_seq),
    };
    assert_eq!(update(&mut engine, back), 3);
    apply_engine(
        &mut engine,
        &mut accounting,
        Command::CustodyFinalised(engine_core::deposits::CustodyCheckpoint {
            block: 3,
            block_hash: engine_core::eip712::B256::repeat_byte(3),
            deposit_count: u64::MAX,
        }),
    );
}

fn apply_engine(engine: &mut Engine, accounting: &mut Accounting, command: Command) -> Vec<Event> {
    let sequenced = Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: 0, cmd: command };
    let events = engine.apply(&sequenced).expect("valid sequence");
    assert!(
        !events.iter().any(|event| matches!(
            event,
            Event::AdminRejected { .. } | Event::DepositRejected { .. } | Event::CustodyDepositRejected { .. }
        )),
        "{events:?}"
    );
    accounting.apply(&sequenced.cmd, &events).expect("valid accounting output");
    assert_matches(*accounting, engine, Fixed::ZERO);
    let (seq, root) = (engine.last_seq().unwrap(), engine.state_root());
    let operator = dev_config().operator().unwrap();
    let key = crate::tee::EnclaveKey::test_key();
    let checkpoint = crate::log::Checkpoint {
        seq,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint(operator.chain_id, &operator.vault.0, seq, &root),
        domain: Some(crate::log::CheckpointDomain { chain_id: operator.chain_id, vault: operator.vault }),
        parent: None,
        deferred_withdrawals: false,
    };
    let work = crate::checkpoints::Work::capture(checkpoint, *accounting, engine);
    assert_eq!((work.liabilities, work.deposits, work.coverage), recompute_work(engine, Fixed::ZERO));
    events
}

fn receipt(id: u64, beneficiary: DepositBeneficiary, amount: i64) -> DepositReceipt {
    DepositReceipt {
        block: id + 1,
        block_hash: engine_core::eip712::B256::repeat_byte(1),
        tx_hash: engine_core::eip712::B256::repeat_byte(id as u8 + 1),
        log_index: id,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary,
        amount: Fixed::from_int(amount),
    }
}

fn changed(update: &DepositUpdate, status: DepositStatus, delta: i64) -> Event {
    Event::CustodyDepositChanged {
        receipt: update.subject().clone(),
        status,
        principal_delta: Fixed::from_int(delta),
        uncredited: None,
    }
}
