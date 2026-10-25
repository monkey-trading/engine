//! V20 receipt lifecycle, collateral and withdrawal eligibility through Engine::apply.

mod common;

use alloy_primitives::B256;
use common::emissions::{allocations, next_period};
use common::events::only_fill;
use common::{BTC, Harness, acct, f};
use engine_core::command::{AdminCommand, Command};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::{Engine, Fixed};

#[test]
fn pending_principal_trades_and_only_finalised_cash_with_margin_headroom_can_be_withdrawn() {
    let mut h = harness(false);
    let account = acct(1);
    h.deposit(account, "50");
    let deposit = receipt(1, account, "200");
    update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
    h.price(BTC, "100");
    assert!(
        h.market_order(account, BTC, Side::Buy, "10", "100")
            .iter()
            .any(|event| matches!(event, Event::Fill(fill) if fill.qty == f("10")))
    );
    assert_eq!(
        withdraw(&mut h, account, "51"),
        vec![Event::WithdrawalRejected {
            account,
            amount: f("51"),
            reason: Reject::InsufficientFunds { available: f("50") }
        }]
    );
    assert_eq!(withdraw(&mut h, account, "50"), vec![Event::WithdrawalApproved { account, amount: f("50"), nonce: 1 }]);
    assert_eq!(
        (h.engine.accounts()[&account].balance, h.engine.accounts()[&account].unfinalised_deposits),
        (f("200"), f("200"))
    );
    update(&mut h, DepositUpdate::Finalise { receipt: deposit.clone(), observed_at_seq: 0 });
    assert_eq!(h.engine.accounts()[&account].balance, f("200"));
    assert_eq!(
        withdraw(&mut h, account, "101"),
        vec![Event::WithdrawalRejected {
            account,
            amount: f("101"),
            reason: Reject::InsufficientFunds { available: f("100") }
        }]
    );
    assert_eq!(
        withdraw(&mut h, account, "100"),
        vec![Event::WithdrawalApproved { account, amount: f("100"), nonce: 2 }]
    );
    assert!(update(&mut h, DepositUpdate::Finalise { receipt: deposit, observed_at_seq: 0 }).is_empty());
    assert_eq!(h.engine.accounts()[&account].balance, f("100"));
}

#[test]
fn reversal_preserves_finalised_funds_after_long_or_short_trading_losses() {
    for (side, closing_price) in [(Side::Buy, "80"), (Side::Sell, "120")] {
        let mut h = harness(false);
        let account = acct(1);
        h.deposit(account, "50");
        let deposit = receipt(1, account, "100");
        update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
        h.price(BTC, "100");
        assert_eq!(only_fill(&h.market_order(account, BTC, side, "1", "100")).qty, f("1"));
        price_after_move_window(&mut h, closing_price);
        let closing = if side == Side::Buy { Side::Sell } else { Side::Buy };
        assert_eq!(only_fill(&h.market_order(account, BTC, closing, "1", closing_price)).realised_pnl, f("-20"));
        let events = update(&mut h, DepositUpdate::Reverse { receipt: deposit.clone(), observed_at_seq: 0 });
        assert!(!events.iter().any(|event| matches!(event, Event::BadDebt { .. })));
        assert_eq!(h.engine.accounts()[&account].balance, f("30"));
        assert_eq!(h.engine.accounts()[&account].unfinalised_deposits, Fixed::ZERO);
        assert!(update(&mut h, DepositUpdate::Reverse { receipt: deposit, observed_at_seq: 0 }).is_empty());
    }
}

#[test]
fn flat_client_losses_are_written_off_against_the_house_once_on_reversal() {
    for (side, closing_price) in [(Side::Buy, "80"), (Side::Sell, "120")] {
        let mut h = harness(true);
        let account = acct(1);
        let deposit = receipt(1, account, "100");
        update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
        h.price(BTC, "100");
        assert_eq!(only_fill(&h.market_order(account, BTC, side, "1", "100")).qty, f("1"));
        price_after_move_window(&mut h, closing_price);
        let closing = if side == Side::Buy { Side::Sell } else { Side::Buy };
        assert_eq!(only_fill(&h.market_order(account, BTC, closing, "1", closing_price)).realised_pnl, f("-20"));
        assert!(h.engine.accounts()[&account].positions.is_empty());
        assert_eq!(h.engine.accounts()[&account].balance, f("80"));
        assert_eq!(
            withdraw(&mut h, account, "1"),
            vec![Event::WithdrawalRejected {
                account,
                amount: f("1"),
                reason: Reject::InsufficientFunds { available: Fixed::ZERO }
            }]
        );
        let events = update(&mut h, DepositUpdate::Reverse { receipt: deposit.clone(), observed_at_seq: 0 });
        assert_eq!(events.iter().filter(|event| matches!(event, Event::BadDebt { account: a, amount } if *a == account && *amount == f("20"))).count(), 1);
        assert_eq!(h.engine.accounts()[&account].balance, Fixed::ZERO);
        assert_eq!(h.engine.house_nav(), Fixed::ZERO);
        assert!(update(&mut h, DepositUpdate::Reverse { receipt: deposit, observed_at_seq: 0 }).is_empty());
        assert_eq!(h.engine.house_nav(), Fixed::ZERO);
    }
}

#[test]
fn reversal_closes_executable_longs_and_shorts_and_waits_for_stale_positions_to_price() {
    for (side, price) in [(Side::Buy, "90"), (Side::Sell, "110")] {
        for stale in [false, true] {
            let mut h = harness(true);
            let account = acct(1);
            let deposit = receipt(1, account, "100");
            update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
            h.price(BTC, "100");
            assert_eq!(only_fill(&h.market_order(account, BTC, side, "5", "100")).qty, f("5"));
            price_after_move_window(&mut h, price);
            if stale {
                h.advance(100_000);
            }
            let mut events = update(&mut h, DepositUpdate::Reverse { receipt: deposit.clone(), observed_at_seq: 0 });
            if stale {
                assert_eq!(h.engine.accounts()[&account].positions.len(), 1);
                assert!(!events.iter().any(|event| matches!(event, Event::CloseOut { .. } | Event::BadDebt { .. })));
                events = h.price(BTC, price);
            }
            assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })));
            assert!(events.iter().any(
                |event| matches!(event, Event::BadDebt { account: a, amount } if *a == account && *amount == f("50"))
            ));
            assert!(h.engine.accounts()[&account].positions.is_empty());
            assert_eq!(h.engine.accounts()[&account].balance, Fixed::ZERO);
            assert_eq!(h.engine.house_nav(), Fixed::ZERO);
            assert!(update(&mut h, DepositUpdate::Reverse { receipt: deposit, observed_at_seq: 0 }).is_empty());
        }
    }
}

#[test]
fn a_reincluded_deposit_keeps_a_solvent_position_open_and_an_unpaired_orphan_still_closes_out() {
    for (side, price) in [(Side::Buy, "90"), (Side::Sell, "110")] {
        let mut h = harness(true);
        let account = acct(1);
        let deposit = receipt(1, account, "100");
        update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
        h.price(BTC, "100");
        assert_eq!(only_fill(&h.market_order(account, BTC, side, "5", "100")).qty, f("5"));
        // Equity 50 still covers the position's margin; without the deposit it would be -50.
        price_after_move_window(&mut h, price);
        let nav = h.engine.house_nav();
        let moved = DepositReceipt {
            block: deposit.block + 2,
            block_hash: B256::repeat_byte(0x55),
            log_index: 9,
            deposit_id: 9,
            ..deposit.clone()
        };
        let events = update(
            &mut h,
            DepositUpdate::Replace {
                receipt: deposit.clone(),
                observed_at_seq: 0,
                replacement: moved.clone(),
                replacement_reversed_at_seq: None,
            },
        );
        assert!(
            !events.iter().any(|event| matches!(event, Event::CloseOut { .. } | Event::BadDebt { .. })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::CustodyDepositChanged { status: DepositStatus::Reversed, .. }))
        );
        assert_eq!(h.engine.accounts()[&account].positions.len(), 1);
        assert_eq!(
            (h.engine.accounts()[&account].balance, h.engine.accounts()[&account].unfinalised_deposits),
            (f("100"), f("100"))
        );
        assert_eq!(h.engine.house_nav(), nav);
        // The same account with no replacement takes the full reversal path.
        let events = update(&mut h, DepositUpdate::Reverse { receipt: moved, observed_at_seq: 0 });
        assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })));
        assert!(events.iter().any(
            |event| matches!(event, Event::BadDebt { account: a, amount } if *a == account && *amount == f("50"))
        ));
    }
}

#[test]
fn a_reversal_shortfall_the_house_writes_off_is_subtracted_from_emission_weight() {
    let mut h = Harness::emitting(1_000_000);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
    assert!(h.engine.deposit_finality_enabled() && h.engine.emissions().is_some());
    let account = acct(1);
    let deposit = receipt(1, account, "100");
    update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
    h.price(BTC, "100");
    assert_eq!(only_fill(&h.market_order(account, BTC, Side::Buy, "5", "100")).qty, f("5"));
    price_after_move_window(&mut h, "90");
    let events = update(&mut h, DepositUpdate::Reverse { receipt: deposit, observed_at_seq: 0 });
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::BadDebt { account: a, amount } if *a == account && *amount == f("50")))
    );
    let result = h.engine.period_results()[&account];
    assert_eq!(result.written_off, f("50"), "the reversal's write-off is recorded against emissions");
    assert_eq!(result.realised, f("-50"));
    // The loss the house wrote off earns nothing.
    assert!(allocations(&next_period(&mut h)).iter().all(|(allocated, ..)| *allocated != account));
}

#[test]
fn house_reversal_may_make_nav_negative_without_halting_orders() {
    let mut h = harness(true);
    let account = acct(1);
    let house = DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt(1, account, "100") };
    update(&mut h, DepositUpdate::Credit { receipt: house.clone() });
    h.deposit(account, "100");
    h.price(BTC, "100");
    assert_eq!(only_fill(&h.market_order(account, BTC, Side::Buy, "1", "100")).qty, f("1"));
    price_after_move_window(&mut h, "120");
    assert_eq!(only_fill(&h.market_order(account, BTC, Side::Sell, "1", "120")).realised_pnl, f("20"));
    let before = h.engine.house_nav();
    update(&mut h, DepositUpdate::Reverse { receipt: house.clone(), observed_at_seq: 0 });
    assert_eq!(h.engine.house_nav(), before - f("100"));
    assert!(h.engine.house_nav().is_negative());
    assert!(h.market_order(account, BTC, Side::Buy, "1", "120").iter().any(|event| matches!(event, Event::Fill(_))));
    let nav = h.engine.house_nav();
    assert!(update(&mut h, DepositUpdate::Reverse { receipt: house, observed_at_seq: 0 }).is_empty());
    assert_eq!(h.engine.house_nav(), nav);
}

#[test]
fn house_withdrawals_preserve_pending_principal_and_release_no_second_credit() {
    let mut h = harness(true);
    let house = DepositReceipt { beneficiary: DepositBeneficiary::House, ..receipt(1, acct(1), "100") };
    h.cmd(Command::HouseCapital { amount: f("50"), chain_ref: "finalised-house".into() });
    update(&mut h, DepositUpdate::Credit { receipt: house.clone() });
    let events = h.cmd(Command::HouseCapital { amount: f("-50"), chain_ref: "house-out-1".into() });
    assert_eq!(events, vec![Event::HouseCapitalMoved { amount: f("-50"), nav: f("100") }]);
    let events = h.cmd(Command::HouseCapital { amount: f("-1"), chain_ref: "house-out-2".into() });
    assert!(matches!(events.as_slice(), [Event::AdminRejected { .. }]));
    assert_eq!(h.engine.house_nav(), f("100"));
    update(&mut h, DepositUpdate::Finalise { receipt: house, observed_at_seq: 0 });
    assert_eq!(h.engine.house_nav(), f("100"));
    assert_eq!(
        h.cmd(Command::HouseCapital { amount: f("-1"), chain_ref: "house-out-2".into() }),
        vec![Event::HouseCapitalMoved { amount: f("-1"), nav: f("99") }]
    );
}

#[test]
fn lifecycle_log_replay_and_serialised_restart_preserve_roots_locks_and_exact_once_debits() {
    let mut h = harness(false);
    let account = acct(1);
    let deposit = receipt(1, account, "100");
    update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
    let pending_root = h.engine.state_root();
    let mut replay = Engine::new();
    for command in &h.log {
        replay.apply(command).unwrap();
    }
    assert_eq!(replay.state_root(), pending_root);
    h.engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(h.engine.state_root(), pending_root);
    update(&mut h, DepositUpdate::Reverse { receipt: deposit.clone(), observed_at_seq: 0 });
    assert!(update(&mut h, DepositUpdate::Reverse { receipt: deposit.clone(), observed_at_seq: 0 }).is_empty());
    let root = h.engine.state_root();
    let mut replay = Engine::new();
    for command in &h.log {
        replay.apply(command).unwrap();
    }
    assert_eq!(replay.state_root(), root);
    assert_eq!(replay.accounts()[&account].balance, Fixed::ZERO);
    assert_eq!(replay.recorded_deposit(&deposit.reference()).unwrap().status, DepositStatus::Reversed);
}

// ── Helpers ──

#[test]
fn legacy_upgrade_records_pending_principal_before_withdrawals_and_releases_only_after_proof() {
    use engine_core::deposits::LegacyDeposit;
    assert!(ProtocolVersion::LATEST.supports_deposit_finality(), "this build activates deposit finality");
    let mut h = Harness::v17();
    let account = acct(1);
    let deposit = receipt(1, account, "100");
    let legacy = LegacyDeposit {
        chain_ref: format!("{}:{}", deposit.tx_hash, deposit.log_index),
        beneficiary: deposit.beneficiary,
        amount: deposit.amount,
    };
    h.cmd(Command::Deposit { account, amount: deposit.amount, chain_ref: legacy.chain_ref.clone() });
    h.deposit(account, "50");
    let historical_root = h.engine.state_root();
    let mut historical = Engine::new();
    for command in &h.log {
        historical.apply(command).unwrap();
    }
    assert_eq!(historical.state_root(), historical_root);
    assert!(
        h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }))
            .iter()
            .any(|event| matches!(event, Event::AdminRejected { .. }))
    );
    h.admin(AdminCommand::ActivateCustodyFinality { version: ProtocolVersion::LATEST, legacy_deposits: vec![legacy] });
    let raw = receipt(99, account, "100");
    let raw_ref = format!("{}:{}", raw.tx_hash, raw.log_index);
    assert!(
        h.cmd(Command::Deposit { account, amount: raw.amount, chain_ref: raw_ref.clone() })
            .iter()
            .any(|event| matches!(event, Event::DepositRejected { reason: Reject::CustodyReceiptRequired, .. }))
    );
    assert!(
        h.cmd(Command::HouseCapital { amount: raw.amount, chain_ref: raw_ref })
            .iter()
            .any(|event| matches!(event, Event::AdminRejected { .. }))
    );
    assert_eq!(
        (h.engine.accounts()[&account].balance, h.engine.accounts()[&account].unfinalised_deposits),
        (f("150"), f("100"))
    );
    assert_eq!(
        withdraw(&mut h, account, "51"),
        vec![Event::WithdrawalRejected {
            account,
            amount: f("51"),
            reason: Reject::InsufficientFunds { available: f("50") }
        }]
    );
    assert!(matches!(withdraw(&mut h, account, "50").as_slice(), [Event::WithdrawalApproved { .. }]));
    update(&mut h, DepositUpdate::Credit { receipt: deposit.clone() });
    assert_eq!(
        (h.engine.accounts()[&account].balance, h.engine.accounts()[&account].unfinalised_deposits),
        (f("100"), f("100"))
    );
    assert!(matches!(withdraw(&mut h, account, "1").as_slice(), [Event::WithdrawalRejected { .. }]));
    update(&mut h, DepositUpdate::Finalise { receipt: deposit, observed_at_seq: 0 });
    assert!(matches!(withdraw(&mut h, account, "1").as_slice(), [Event::WithdrawalApproved { .. }]));
    let mut replayed = Engine::new();
    for command in &h.log {
        replayed.apply(command).unwrap();
    }
    assert_eq!(replayed.state_root(), h.engine.state_root());
    assert_eq!(replayed.accounts()[&account].balance, f("99"));
}

#[test]
fn custody_activation_waits_for_dated_financing_without_charging_legacy_rates() {
    use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
    use engine_core::deposits::LegacyDeposit;
    use engine_core::fixed::Rounding;

    let mut h = Harness::v17();
    let account = acct(1);
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 1_000 });
    let deposit = receipt(1, account, "100000");
    let command = Command::Deposit {
        account,
        amount: deposit.amount,
        chain_ref: format!("{}:{}", deposit.tx_hash, deposit.log_index),
    };
    let events = h.cmd(command.clone());
    let legacy = LegacyDeposit::from_command(&command, &events).expect("successful historical credit");
    h.price(BTC, "100000");
    assert!(h.market_order(account, BTC, Side::Buy, "1", "100000").iter().any(|e| matches!(e, Event::Fill(_))));
    let before = h.balance(account);
    let rollover = h.engine.next_rollover().expect("genesis schedules financing");
    h.advance_to(rollover + 1);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateCustodyFinality {
        version: ProtocolVersion::V20,
        legacy_deposits: vec![legacy],
    }));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Financing { .. })));
    assert_eq!(h.balance(account), before);
    assert_eq!(h.engine.accounts()[&account].unfinalised_deposits, deposit.amount);
    assert_eq!(h.engine.next_rollover(), Some(rollover));

    let date = h.engine.benchmark_date(h.ts);
    let financing = &h.engine.markets()[&BTC].spec.financing;
    let expected = f("100000")
        .mul_rounded(f("0.01") + Fixed::from_bps(financing.markup_bps), Rounding::Up)
        .div_int_rounded(financing.day_basis, Rounding::Up);
    let events = h.operate(AdminCommand::SetBenchmarkRates(BenchmarkBatch {
        as_of: date,
        checked: ["SOFR".to_owned()].into(),
        rates: vec![BenchmarkRate { benchmark: "SOFR".into(), effective_date: date, annual_rate: f("0.01") }],
    }));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    let charged: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Financing { account: a, amount, .. } if *a == account => Some(*amount),
            _ => None,
        })
        .collect();
    assert_eq!(charged, [expected]);
    assert_eq!(h.balance(account), before - expected);
    assert_eq!(h.engine.accounts()[&account].unfinalised_deposits, deposit.amount);
    assert!(!h.cmd(Command::Tick).iter().any(|e| matches!(e, Event::Financing { .. })));
    let mut replay = Engine::new();
    for command in &h.log {
        replay.apply(command).expect("recorded sequence");
    }
    assert_eq!(replay.state_root(), h.engine.state_root());
}

fn harness(zero_house: bool) -> Harness {
    let mut h = if zero_house {
        let mut h = Harness::zero_house(ProtocolVersion::V6);
        let mut params = h.engine.params().unwrap().clone();
        params.house_total_cap_floor = f("1000000");
        h.admin(AdminCommand::SetParams(params));
        let mut market = h.engine.markets()[&BTC].spec.clone();
        market.house_cap_floor = f("1000000");
        h.admin(AdminCommand::UpsertMarket(market));
        h.admin(AdminCommand::SetOperator(common::operator()));
        h
    } else {
        Harness::v17()
    };
    if h.engine.protocol_version() < ProtocolVersion::LATEST {
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
    }
    assert!(h.engine.deposit_finality_enabled(), "V20 activation enables deposit finality");
    h
}

/// Expires the fixture's 60s move window so the loss oracle uses normal mid-price fills.
fn price_after_move_window(h: &mut Harness, price: &str) {
    h.advance(61_000);
    h.price(BTC, price);
    assert_eq!(h.engine.markets()[&BTC].stress.level, 0);
}

fn receipt(id: u64, account: engine_core::types::AccountId, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: id + 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(id as u8 + 1),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
    }
}

fn update(h: &mut Harness, mut update: DepositUpdate) -> Vec<Event> {
    observe(&h.engine, &mut update);
    let events = h.cmd(Command::CustodyDeposit(Box::new(update)));
    assert!(!events.iter().any(|event| matches!(event, Event::CustodyDepositRejected { .. })), "{events:?}");
    events
}

fn withdraw(h: &mut Harness, account: engine_core::types::AccountId, amount: &str) -> Vec<Event> {
    h.cmd(Command::Withdraw { account, amount: f(amount), auth: None })
}

/// Copies the revision a custody reader observed before sequencing its conditional update.
fn observe(e: &Engine, update: &mut DepositUpdate) {
    let observed = e.recorded_deposit(&update.subject().reference()).map_or(0, |d| d.updated_at_seq);
    match update {
        DepositUpdate::Finalise { observed_at_seq, .. }
        | DepositUpdate::Reverse { observed_at_seq, .. }
        | DepositUpdate::Replace { observed_at_seq, .. }
            if *observed_at_seq == 0 =>
        {
            *observed_at_seq = observed
        }
        _ => {}
    }
}
