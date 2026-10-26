use super::*;

use std::collections::{BTreeMap, BTreeSet};

use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Sequenced};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::test_support::{TRADER, dev_config};
mod receipts;

#[derive(serde::Deserialize)]
struct ReceiptState {
    #[serde(default)]
    deposits: BTreeMap<String, engine_core::deposits::RecordedDeposit>,
}

fn receipt_state(engine: &Engine) -> ReceiptState {
    serde_json::from_value(serde_json::to_value(engine).expect("engine state serializes"))
        .expect("receipt state decodes")
}

#[test]
fn accounting_matches_cash_and_references_across_credit_acceptance_rules() {
    for version in
        [ProtocolVersion::V2, ProtocolVersion::V3, ProtocolVersion::V4, ProtocolVersion::V8, ProtocolVersion::V19]
    {
        let mut engine = Engine::new();
        let mut accounting = Accounting::default();
        let mut unpaid = Fixed::ZERO;
        let mut apply = |cmd: Command| {
            let sequenced = Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: 0, cmd };
            let events = engine.apply(&sequenced).unwrap();
            accounting.apply(&sequenced.cmd, &events).unwrap();
            unpaid += approved(&events);
            assert_eq!((accounting.liabilities, accounting.deposits), recompute(&engine, unpaid), "{sequenced:?}");
            assert_eq!(accounting.unpaid(), unpaid);
            events
        };
        apply(Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)));
        apply(Command::Admin(AdminCommand::SetOperator(dev_config().operator().unwrap())));
        if version != ProtocolVersion::V2 {
            let events = apply(Command::Admin(AdminCommand::ActivateProtocol { version }));
            assert!(events.contains(&Event::AdminApplied), "{version:?}: {events:?}");
        }
        let zero = apply(capital(0, "0xzero"));
        assert_eq!(
            zero.iter().any(|event| matches!(event, Event::HouseCapitalMoved { amount, .. } if amount.is_zero())),
            version >= ProtocolVersion::V4,
            "{version:?}: {zero:?}"
        );
        for cmd in [
            capital(0, "0xzero"),
            capital(100, "0xhouse"),
            capital(-10, "0xremove"),
            capital(-1_000, "0xretry-house"),
            capital(10, "0xretry-house"),
            deposit(25, "0xdeposit"),
            deposit(25, "0xdeposit"),
            deposit(-1, "0xretry"),
            deposit(5, "0xretry"),
            deposit(0, "0xzero-deposit"),
            deposit(1, "0xhouse"),
            capital(1, "0xdeposit"),
            deposit(1, "0Xuppercase"),
            deposit(1, "mock-credit"),
            Command::Deposit { account: TRADER, amount: Fixed::MAX_INPUT + Fixed::ONE, chain_ref: "0xlarge".into() },
            Command::HouseCapital { amount: Fixed::MAX_INPUT + Fixed::ONE, chain_ref: "0xlarge-house".into() },
            Command::Withdraw { account: TRADER, amount: Fixed::from_int(10), auth: None },
            Command::Withdraw { account: TRADER, amount: Fixed::from_int(1_000), auth: None },
        ] {
            apply(cmd);
        }
        assert_eq!(engine.protocol_version(), version);
        assert_eq!(accounting.liabilities, Fixed::from_int(132));
        assert_eq!(accounting.deposits, if version >= ProtocolVersion::V4 { 6 } else { 5 });
        assert_eq!(accounting.unpaid(), Fixed::from_int(10));
    }
}

#[test]
fn only_matching_success_events_count_evm_credit_references() {
    let other = AccountId([1; 20]);
    let amount = Fixed::ONE;
    let credit = Event::Deposited { account: TRADER, amount };
    for cmd in [
        Command::Tick,
        Command::Deposit { account: other, amount, chain_ref: "0xcredit".into() },
        deposit(2, "0xcredit"),
        deposit(1, "mock-credit"),
        capital(1, "0xcredit"),
    ] {
        assert!(!evm_disposition(&cmd, &credit), "{cmd:?}");
    }
    assert!(evm_disposition(&deposit(1, "0xcredit"), &credit));
    assert!(!evm_disposition(&capital(2, "0xcredit"), &Event::HouseCapitalMoved { amount, nav: amount }));
    assert!(evm_disposition(&capital(0, "0xzero"), &Event::HouseCapitalMoved { amount: Fixed::ZERO, nav: amount }));
}

#[test]
fn overflow_refuses_the_whole_accounting_batch_without_wrapping() {
    let max = Fixed::from_raw(i128::MAX);
    let cases = [
        (
            Accounting { pending: Fixed::ZERO, held: Fixed::ZERO, liabilities: max, deposits: 2, unpaid: Fixed::ZERO },
            deposit(1, "0xoverflow"),
            vec![Event::Deposited { account: TRADER, amount: Fixed::ONE }],
            "liabilities overflow",
        ),
        (
            Accounting {
                pending: Fixed::ZERO,
                held: Fixed::ZERO,
                liabilities: Fixed::from_raw(i128::MIN),
                deposits: 2,
                unpaid: Fixed::ZERO,
            },
            capital(-1, "0xunderflow"),
            vec![Event::HouseCapitalMoved { amount: -Fixed::ONE, nav: Fixed::ONE }],
            "liabilities overflow",
        ),
        (
            Accounting { pending: Fixed::ZERO, held: Fixed::ZERO, liabilities: Fixed::ONE, deposits: 2, unpaid: max },
            deposit(1, "0xatomic"),
            vec![
                Event::Deposited { account: TRADER, amount: Fixed::ONE },
                Event::WithdrawalApproved { account: TRADER, amount: Fixed::ONE, nonce: 1 },
            ],
            "unpaid approvals overflow",
        ),
        (
            Accounting {
                pending: Fixed::ZERO,
                held: Fixed::ZERO,
                liabilities: Fixed::ONE,
                deposits: u64::MAX,
                unpaid: Fixed::ZERO,
            },
            capital(0, "0xcount-overflow"),
            vec![Event::HouseCapitalMoved { amount: Fixed::ZERO, nav: Fixed::ONE }],
            "deposit count overflow",
        ),
    ];
    for (initial, cmd, events, reason) in cases {
        let mut accounting = initial;
        let err = accounting.apply(&cmd, &events).unwrap_err();
        assert!(err.to_string().contains(reason), "{err:#}");
        assert_eq!(accounting, initial, "failed batch must not publish even its successful prefix");
    }
}

/// Independent full-state scans, including adopted, live and reversed receipt identities.
pub(crate) fn recompute(engine: &Engine, unpaid: Fixed) -> (Fixed, u64) {
    let reserved = engine.withdrawal_queue().fold(Fixed::ZERO, |sum, request| sum + request.remaining);
    let cash =
        engine.accounts().values().fold(engine.house_cash() + unpaid + reserved, |sum, account| sum + account.balance);
    // Uncredited collateral the vault holds: pending and held deposits, receipts among them.
    let held: Fixed = engine.held_deposits().values().map(|hold| hold.deposit.amount).sum();
    let pending: Fixed = engine.pending_deposits().values().map(|deposit| deposit.amount).sum();
    let liabilities = cash + held + pending;
    let state = receipt_state(engine);
    let historical: BTreeSet<_> = engine.credited_refs_with_prefix("0x").collect();
    let represented: BTreeSet<_> = state
        .deposits
        .values()
        .map(|record| format!("{}:{}", record.receipt.tx_hash, record.receipt.log_index))
        .filter(|reference| historical.contains(reference.as_str()))
        .collect();
    let live = state
        .deposits
        .values()
        .filter(|record| record.status != engine_core::deposits::DepositStatus::Reversed)
        .count();
    // Uncredited references count at their first held or pending disposition, as the cached count
    // does; a reference is never both credited and held or pending.
    let uncredited: BTreeSet<_> = engine
        .held_deposits()
        .keys()
        .chain(engine.pending_deposits().keys())
        .map(String::as_str)
        .filter(|reference| reference.starts_with("0x"))
        .collect();
    let count = |n| u64::try_from(n).expect("state count fits u64");
    let deposits = count(historical.len())
        .checked_sub(count(represented.len()))
        .and_then(|n| n.checked_add(count(live)))
        .and_then(|n| n.checked_add(count(uncredited.len())))
        .expect("receipt counts are representable");
    (liabilities, deposits)
}

/// Canonical coverage is a separate full scan, independent of the production hot index.
pub(crate) fn recompute_work(engine: &Engine, unpaid: Fixed) -> (Fixed, u64, crate::checkpoints::DepositCoverage) {
    use crate::checkpoints::DepositCoverage;

    let (liabilities, historical) = recompute(engine, unpaid);
    if !engine.deposit_finality_enabled() {
        return (liabilities, historical, DepositCoverage::Historical);
    }
    let Some(checkpoint) = engine.custody_checkpoint() else {
        return (liabilities, historical, DepositCoverage::AwaitingCanonicalScan);
    };
    let above = receipt_state(engine)
        .deposits
        .values()
        .filter(|record| {
            record.status != engine_core::deposits::DepositStatus::Reversed && record.receipt.block > checkpoint.block
        })
        .count();
    let Some(deposits) = checkpoint.deposit_count.checked_add(u64::try_from(above).expect("receipt count fits u64"))
    else {
        return (liabilities, historical, DepositCoverage::AwaitingCanonicalScan);
    };
    (liabilities, deposits, DepositCoverage::Canonical)
}

/// Compares every cached quantity with an independently replayed approval total and state scan.
pub(crate) fn assert_matches(accounting: Accounting, engine: &Engine, unpaid: Fixed) {
    assert_eq!((accounting.liabilities, accounting.deposits), recompute(engine, unpaid));
    assert_eq!(accounting.unpaid(), unpaid);
    assert_eq!(accounting.held, engine.held_deposits().values().map(|hold| hold.deposit.amount).sum());
    assert_eq!(accounting.pending, engine.pending_deposits().values().map(|deposit| deposit.amount).sum());
}

/// Approvals from computed output; signatures do not prove that custody paid them.
pub(crate) fn approved(events: &[Event]) -> Fixed {
    events
        .iter()
        .filter_map(|event| match event {
            Event::WithdrawalApproved { amount, .. } => Some(*amount),
            _ => None,
        })
        .sum()
}

/// A credit to the test trader.
fn deposit(amount: i64, chain_ref: &str) -> Command {
    Command::Deposit { account: TRADER, amount: Fixed::from_int(amount), chain_ref: chain_ref.into() }
}

/// A signed amount of house capital, using the engine's historical acceptance rules.
fn capital(amount: i64, chain_ref: &str) -> Command {
    Command::HouseCapital { amount: Fixed::from_int(amount), chain_ref: chain_ref.into() }
}
