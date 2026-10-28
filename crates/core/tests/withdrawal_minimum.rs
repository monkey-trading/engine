//! From V34 a withdrawal request is at least 10 USDC and the FIFO queue approves less than that
//! only as a request's final remainder; V31 keeps dust requests and parts.

#[allow(dead_code)]
#[path = "access/support.rs"]
mod access;
mod common;

use common::{BTC, Client, Harness, f, params};
use engine_core::Fixed;
use engine_core::access::{AccessDeposit, AccessPolicy};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::withdrawal::MIN_WITHDRAWAL;

const V31: ProtocolVersion = ProtocolVersion::V31;
const V34: ProtocolVersion = ProtocolVersion::V34;

/// A client holding 1000 under `version`, with 1000 of physical payout capacity.
fn setup(version: ProtocolVersion) -> (Harness, Client) {
    let mut h = Harness::staking(f("100"), 600_000, 86_400_000);
    let mut parameters = params();
    parameters.profit_fee_bps = 0;
    parameters.stress_levels.truncate(1);
    parameters.toxicity.tiers.clear();
    parameters.house_total_cap_floor = f("10000");
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
        source: engine_core::eip712::address(access::source().verifying_key()),
        max_age_ms: 60_000,
    }));
    h.cmd(Command::HouseCapital { amount: f("-99999000"), chain_ref: "house-1000".into() });
    let client = Client::new(1);
    h.deposit(client.account, "1000");
    h.price(BTC, "100");
    let events = h.market_order(client.account, BTC, Side::Buy, "1", "100");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    h.admin(AdminCommand::ActivateProtocol { version });
    access::attest(&mut h, &client);
    (h, client)
}

/// Closes the position at 2100, a gain of 2000 against 2000 of physical cash.
fn close_with_gain(h: &mut Harness, old_client: &Client) {
    h.price(BTC, "2100");
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    assert_eq!(client.account, old_client.account);
    let request = OrderRequest {
        account: client.account,
        client_id: 31,
        market: BTC,
        side: Side::Sell,
        qty: f("1"),
        kind: OrderKind::Market { quoted: f("2100"), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(h.engine.next_request_nonce(client.account), request) else {
        unreachable!()
    };
    let evidence = access::order_evidence(h, &request);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
}

fn withdraw(h: &mut Harness, old_client: &Client, amount: &str) -> Vec<Event> {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    assert_eq!(client.account, old_client.account);
    let Command::Withdraw { account, amount, auth } =
        client.withdraw(h.engine.next_request_nonce(client.account), amount)
    else {
        unreachable!()
    };
    let request = WithdrawalRequest { account, amount, auth };
    let auth = request.auth.as_ref().unwrap();
    let hash = engine_core::eip712::withdrawal_hash(&client.domain, account, amount, auth.nonce, auth.expires_at);
    let evidence = access::evidence(h, hash, Some(account), None);
    h.cmd(Command::AccessWithdraw { request, evidence, payout_available: true })
}

fn approved(events: &[Event]) -> Vec<Fixed> {
    events
        .iter()
        .filter_map(|e| if let Event::WithdrawalApproved { amount, .. } = e { Some(*amount) } else { None })
        .collect()
}

fn below_minimum(events: &[Event]) -> bool {
    matches!(
        events,
        [Event::WithdrawalRejected { reason: Reject::BelowMinimumWithdrawal { minimum }, .. }] if *minimum == MIN_WITHDRAWAL
    )
}

#[test]
fn the_minimum_is_ten_usdc() {
    assert_eq!(MIN_WITHDRAWAL, f("10"));
}

#[test]
fn a_request_below_ten_usdc_is_refused_from_v34_and_exactly_ten_is_accepted() {
    for amount in ["0.000001", "1", "9.999999", "9.9999999"] {
        let (mut h, client) = setup(V34);
        let events = withdraw(&mut h, &client, amount);
        assert!(below_minimum(&events), "{amount}: {events:?}");
        assert_eq!(h.balance(client.account), f("1000"), "{amount} reserves nothing");
        assert!(h.engine.withdrawal_queue().next().is_none());
    }
    // Excess below one base unit rounds down to exactly ten.
    for amount in ["10", "10.0000009"] {
        let (mut h, client) = setup(V34);
        let events = withdraw(&mut h, &client, amount);
        assert_eq!(approved(&events), [f("10")], "{amount}: {events:?}");
        assert_eq!(h.balance(client.account), f("990"));
    }
}

#[test]
fn dust_requests_are_still_accepted_before_v34() {
    for amount in ["0.000001", "9.999999"] {
        let (mut h, client) = setup(V31);
        let events = withdraw(&mut h, &client, amount);
        assert!(events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })), "{amount}: {events:?}");
    }
}

/// The client's request for 3000 is approved for the 2000 of physical cash, leaving 1000 at the head.
fn waiting(version: ProtocolVersion) -> (Harness, Client) {
    let (mut h, client) = setup(version);
    close_with_gain(&mut h, &client);
    (h, client)
}

/// Screened house capital, the only cash source once access screening applies.
fn capital(h: &mut Harness, amount: &str, tag: &str) -> Vec<Event> {
    let payer = Some(Client::new(1).account);
    let deposit = AccessDeposit { account: None, payer, amount: f(amount), chain_ref: format!("0xcap:{tag}") };
    let evidence = access::evidence(h, engine_core::eip712::deposit_screening_hash(&deposit), None, payer);
    h.cmd(Command::AccessDeposit { deposit, evidence })
}

#[test]
fn a_part_below_the_minimum_waits_from_v34_and_the_final_remainder_is_exempt() {
    let (mut h, client) = waiting(V34);
    let events = withdraw(&mut h, &client, "3000");
    assert_eq!(approved(&events), [f("2000")], "{events:?}");
    let queued = h.engine.withdrawal_queue().next().unwrap().remaining;
    assert_eq!(queued, f("1000"));
    assert!(approved(&capital(&mut h, "5", "a")).is_empty(), "five is below the minimum");
    assert_eq!(approved(&capital(&mut h, "5", "b")), [f("10")], "ten accumulated is approved");
    let rest = queued - f("10");
    assert_eq!(approved(&capital(&mut h, &(rest - f("4")).to_string(), "c")), [rest - f("4")]);
    assert_eq!(h.engine.withdrawal_queue().next().unwrap().remaining, f("4"));
    assert!(approved(&capital(&mut h, "3", "d")).is_empty(), "a remainder is paid whole, not in parts");
    assert_eq!(approved(&capital(&mut h, "1", "e")), [f("4")], "the final remainder is exempt");
    assert!(h.engine.withdrawal_queue().next().is_none());
}

#[test]
fn a_part_below_the_minimum_is_approved_before_v34() {
    let (mut h, client) = waiting(V31);
    withdraw(&mut h, &client, "3000");
    assert_eq!(approved(&capital(&mut h, "5", "a")), [f("5")]);
}
