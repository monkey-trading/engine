//! From `V32`, a reversal or bad-debt write-off that lands in a period after the losses it unwinds
//! becomes a per-account emission debt: later loss weight pays the debt down before any of it earns
//! emission weight again. The scenarios run under the access rules every version from `V24` keeps.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::{AccessEvidence, receipt_access_deposit};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Side};

use crate::common::emissions::{allocations, next_period};
use crate::common::events::only_fill;
use crate::common::{BTC, Client, ETH, Harness, f};
use crate::support::*;

#[test]
fn a_reversal_landing_after_the_losses_becomes_an_emission_debt_that_later_losses_pay_first() {
    let (mut h, client) = emitting(ProtocolVersion::V32);
    let account = client.account;
    let budget = h.engine.emissions().expect("initialised").schedule.budget(0);
    // A root now makes every later one incremental, so the debt leaf is rehashed from what changed.
    h.engine.state_root();

    // Period 0 loses half the receipt's principal in fills and earns its allocation.
    let deposit = receipt(1, account, "100");
    credit(&mut h, &deposit);
    lose(&mut h, "100", "90");
    assert_eq!(allocations(&next_period(&mut h)), [(account, 0, f("50"), budget)]);

    // Period 1: a reorg reverses the receipt. The write-off finds no losses to absorb.
    let events = reverse(&mut h, &deposit);
    assert!(events.iter().any(|e| matches!(e, Event::BadDebt { amount, .. } if *amount == f("50"))), "{events:?}");
    let events = next_period(&mut h);
    assert!(allocations(&events).is_empty(), "{events:?}");
    assert_eq!(h.engine.emission_debt(account), f("50"));
    assert_root_matches_rebuild(&mut h.engine);

    // Period 2: new losses pay the debt down before earning, so nothing is allocated yet.
    credit(&mut h, &receipt(2, account, "100"));
    lose(&mut h, "100", "94");
    let events = next_period(&mut h);
    assert!(allocations(&events).is_empty(), "{events:?}");
    assert_eq!(h.engine.emission_debt(account), f("20"));
    assert_root_matches_rebuild(&mut h.engine);
    // A restart from the serialized engine keeps the debt, and the rest of the scenario runs on it.
    h.engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(h.engine.emission_debt(account), f("20"));
    h.engine.state_root();

    // Period 3: the weight beyond the debt earns, and the debt is gone.
    credit(&mut h, &receipt(3, account, "100"));
    lose(&mut h, "100", "90");
    assert_eq!(allocations(&next_period(&mut h)), [(account, 3, f("30"), budget)]);
    assert_eq!(h.engine.emission_debt(account), Fixed::ZERO);
    assert_eq!(h.engine.entitlement(account).0, 2 * budget);
    assert_root_matches_rebuild(&mut h.engine);

    // The debt is a state-tree leaf: a replay recomputes the same root.
    let root = h.engine.state_root();
    let mut replay = engine_core::Engine::new();
    for command in &h.log {
        replay.apply(command).expect("recorded sequence");
    }
    assert_eq!(replay.state_root(), root);
}

#[test]
fn the_state_root_commits_to_each_accounts_debt() {
    let (mut h, client) = emitting(ProtocolVersion::V32);
    let account = client.account;
    let deposit = receipt(1, account, "100");
    credit(&mut h, &deposit);
    lose(&mut h, "100", "90");
    next_period(&mut h);
    reverse(&mut h, &deposit);
    next_period(&mut h);
    assert_eq!(h.engine.emission_debt(account), f("50"));

    let root = h.engine.state_root();
    let altered = |edit: &dyn Fn(&mut serde_json::Map<String, serde_json::Value>)| {
        let mut state = serde_json::to_value(&h.engine).unwrap();
        edit(state["emission_debts"].as_object_mut().expect("the debt is encoded"));
        serde_json::from_value::<engine_core::Engine>(state).unwrap().state_root()
    };
    let key = account.to_string();
    let larger = serde_json::to_value(f("51")).unwrap();
    assert_ne!(altered(&|debts| _ = debts.insert(key.clone(), larger.clone())), root, "the amount is committed");
    assert_ne!(altered(&|debts| _ = debts.remove(&key)), root, "the debt's existence is committed");
}

#[test]
fn weight_forfeited_for_negative_cash_pays_no_debt() {
    let (mut h, client) = emitting(ProtocolVersion::V32);
    let account = client.account;
    let deposit = receipt(1, account, "100");
    credit(&mut h, &deposit);
    lose(&mut h, "100", "90");
    next_period(&mut h);
    reverse(&mut h, &deposit);
    next_period(&mut h);
    assert_eq!(h.engine.emission_debt(account), f("50"));

    // A BTC loss larger than the cash, while an ETH gain keeps the account above close-out, leaves
    // its cash negative at the cutoff: the loss earns nothing and does not pay the debt either.
    credit(&mut h, &receipt(2, account, "12000"));
    h.price(ETH, "4000");
    assert_eq!(only_fill(&order(&mut h, ETH, Side::Buy, "10", "4000")).qty, f("10"));
    h.price(ETH, "6000");
    h.price(BTC, "100000");
    only_fill(&order(&mut h, BTC, Side::Buy, "1", "100000"));
    h.price(BTC, "85000");
    only_fill(&order(&mut h, BTC, Side::Sell, "1", "85000"));
    assert!(h.balance(account).is_negative());
    let events = next_period(&mut h);
    assert!(allocations(&events).is_empty(), "{events:?}");
    assert_eq!(h.engine.emission_debt(account), f("50"));
}

#[test]
fn losses_in_the_reversal_period_absorb_only_part_of_the_write_off() {
    let (mut h, client) = emitting(ProtocolVersion::V32);
    let account = client.account;
    let budget = h.engine.emissions().expect("initialised").schedule.budget(0);

    let deposit = receipt(1, account, "100");
    credit(&mut h, &deposit);
    lose(&mut h, "100", "90");
    assert_eq!(allocations(&next_period(&mut h)), [(account, 0, f("50"), budget)]);

    // Period 1 writes off 50 and loses 20 more: the losses absorb 20 of it, and 30 stays owed.
    reverse(&mut h, &deposit);
    credit(&mut h, &receipt(2, account, "100"));
    lose(&mut h, "100", "96");
    let result = h.engine.period_results()[&account];
    assert_eq!((result.realised, result.written_off), (f("-20"), f("50")));
    let events = next_period(&mut h);
    assert!(allocations(&events).is_empty(), "{events:?}");
    assert_eq!(h.engine.emission_debt(account), f("30"));
}

#[test]
fn a_write_off_the_same_period_losses_absorb_leaves_no_debt() {
    let (mut h, client) = emitting(ProtocolVersion::V32);
    let account = client.account;
    let deposit = receipt(1, account, "100");
    credit(&mut h, &deposit);
    lose(&mut h, "100", "90");
    reverse(&mut h, &deposit);
    let events = next_period(&mut h);
    assert!(allocations(&events).is_empty(), "the 50 loss and the 50 write-off cancel: {events:?}");
    assert_eq!(h.engine.emission_debt(account), Fixed::ZERO);
}

#[test]
fn before_v32_a_later_period_reversal_leaves_earlier_allocations_unclawed() {
    let (mut h, client) = emitting(ProtocolVersion::V26);
    let account = client.account;
    let budget = h.engine.emissions().expect("initialised").schedule.budget(0);
    let deposit = receipt(1, account, "100");
    credit(&mut h, &deposit);
    lose(&mut h, "100", "90");
    assert_eq!(allocations(&next_period(&mut h)), [(account, 0, f("50"), budget)]);
    reverse(&mut h, &deposit);
    assert!(allocations(&next_period(&mut h)).is_empty());
    assert_eq!(h.engine.emission_debt(account), Fixed::ZERO);

    // Later losses earn in full: the earlier allocation keeps the MONKEY the reversal unwound.
    credit(&mut h, &receipt(2, account, "100"));
    lose(&mut h, "100", "94");
    assert_eq!(allocations(&next_period(&mut h)), [(account, 2, f("30"), budget)]);
    assert_eq!(h.engine.entitlement(account).0, 2 * budget);
}

/// The engine's incremental root, which must equal the root of the same state rebuilt from its
/// serialization.
fn assert_root_matches_rebuild(engine: &mut engine_core::Engine) -> [u8; 32] {
    let incremental = engine.state_root();
    let mut rebuilt: engine_core::Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), incremental, "the incremental root differs from a rebuild's");
    incremental
}

/// An emitting log at `version` under the access rules, with the client declared eligible.
fn emitting(version: ProtocolVersion) -> (Harness, Client) {
    let mut h = Harness::emitting(1_000_000);
    for step in [ProtocolVersion::V20, ProtocolVersion::V21, ProtocolVersion::V22] {
        h.admin(AdminCommand::ActivateProtocol { version: step });
    }
    h.admin(AdminCommand::SetAccessPolicy(engine_core::access::AccessPolicy {
        source: eip712::address(source().verifying_key()),
        max_age_ms: 1_000,
    }));
    h.admin(AdminCommand::ActivateProtocol { version });
    let client = Client::new(1);
    attest(&mut h, &client);
    (h, client)
}

/// Buys five BTC at `from`, lets the move window pass, and sells them at `to`, realising a loss of
/// five times the difference.
fn lose(h: &mut Harness, from: &str, to: &str) {
    h.price(BTC, from);
    assert_eq!(only_fill(&trade(h, Side::Buy, from)).qty, f("5"));
    h.advance(61_000);
    h.price(BTC, to);
    assert_eq!(h.engine.markets()[&BTC].stress.level, 0);
    only_fill(&trade(h, Side::Sell, to));
}

/// A screened market order for five BTC quoted at `quoted`.
fn trade(h: &mut Harness, side: Side, quoted: &str) -> Vec<Event> {
    order(h, BTC, side, "5", quoted)
}

/// A screened market order for `qty` of `market` quoted at `quoted`.
fn order(h: &mut Harness, market: MarketId, side: Side, qty: &str, quoted: &str) -> Vec<Event> {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    let request = OrderRequest {
        account: client.account,
        client_id: 1,
        market,
        side,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(h.engine.next_request_nonce(client.account), request) else {
        unreachable!()
    };
    let evidence = order_evidence(h, &request);
    h.cmd(Command::AccessOrder { request, evidence })
}

fn receipt(id: u8, account: AccountId, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: 100 + u64::from(id),
        block_hash: B256::repeat_byte(id),
        tx_hash: B256::repeat_byte(0x40 + id),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: u64::from(id),
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
    }
}

/// Credits `receipt` on clear evidence, paid by its own beneficiary.
fn credit(h: &mut Harness, receipt: &DepositReceipt) {
    let DepositBeneficiary::Trader { account } = receipt.beneficiary else { unreachable!("trader receipts only") };
    let deposit = receipt_access_deposit(receipt, Some(account));
    let evidence: AccessEvidence =
        evidence(h, eip712::deposit_screening_hash(&deposit), deposit.account, Some(account));
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(account), evidence });
    assert!(events.iter().any(|e| matches!(e, Event::CustodyDepositChanged { .. })), "{events:?}");
}

/// The reorg that drops `receipt`.
fn reverse(h: &mut Harness, receipt: &DepositReceipt) -> Vec<Event> {
    let observed_at_seq = h.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    let events =
        h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: receipt.clone(), observed_at_seq })));
    assert!(!events.iter().any(|e| matches!(e, Event::CustodyDepositRejected { .. })), "{events:?}");
    events
}
