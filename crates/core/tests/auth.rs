//! Client request authorisation through `Engine::apply`: EIP-712 signatures, per-account nonces,
//! and the rules each protocol version and client auth applies.

mod common;

use common::events::{fills, rejection};
use common::{BTC, CHAIN_ID, Client, Harness, VAULT, f};
use engine_core::auth::{ClientAuth, MAX_EXPIRY_AHEAD_MS};
use engine_core::command::{AdminCommand, CancelRequest, Command, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712;
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};

#[test]
fn a_signed_order_fills_and_uses_the_accounts_next_nonce() {
    let (mut h, client) = funded(Harness::signed());
    let events = h.cmd(client.order(0, buy(client.account)));
    assert_eq!(fills(&events).len(), 1, "{events:?}");
    assert_eq!(h.engine.next_request_nonce(client.account), 1);
}

#[test]
fn each_account_keeps_its_own_nonces() {
    let (mut h, alice) = funded(Harness::signed());
    let bob = Client::new(2);
    h.deposit(bob.account, "100000");
    assert_eq!(fills(&h.cmd(alice.order(0, buy(alice.account)))).len(), 1);
    assert_eq!(fills(&h.cmd(bob.order(0, buy(bob.account)))).len(), 1);
    assert_eq!(fills(&h.cmd(alice.order(1, buy(alice.account)))).len(), 1);
    assert_eq!((h.engine.next_request_nonce(alice.account), h.engine.next_request_nonce(bob.account)), (2, 1));
}

#[test]
fn a_replayed_request_is_rejected_as_a_stale_nonce() {
    let (mut h, client) = funded(Harness::signed());
    let order = client.order(0, buy(client.account));
    h.cmd(order.clone());
    let events = h.cmd(order);
    assert_eq!(rejection(&events), &Reject::InvalidNonce { expected: 1 });
    assert!(fills(&events).is_empty());
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, f("0.5"));
}

#[test]
fn a_request_executes_up_to_its_expiry_and_not_after() {
    let (mut h, client) = funded(Harness::signed());
    let at_expiry = Client { expires_at: h.ts, ..Client::new(1) };
    assert_eq!(fills(&h.cmd(at_expiry.order(0, buy(client.account)))).len(), 1);
    let expired = Client { expires_at: h.ts - 1, ..Client::new(1) };
    let order = expired.order(1, buy(client.account));
    assert_eq!(rejection(&h.cmd(order.clone())), &Reject::Expired { expires_at: h.ts - 1 });
    // The nonce is used, so the expired request can never be sequenced again.
    assert_eq!(rejection(&h.cmd(order)), &Reject::InvalidNonce { expected: 2 });
    assert_eq!(rejection(&h.cmd(expired.cancel(2, 0))), &Reject::Expired { expires_at: h.ts - 1 });
    let withdraw = h.cmd(expired.withdraw(3, "100"));
    assert!(matches!(&withdraw[..], [Event::WithdrawalRejected { reason: Reject::Expired { .. }, .. }]));
    assert_eq!(h.engine.next_request_nonce(client.account), 4);
}

#[test]
fn an_expiry_may_lie_at_most_five_minutes_past_the_sequenced_time() {
    let (mut h, client) = funded(Harness::signed());
    assert_eq!(MAX_EXPIRY_AHEAD_MS, 300_000);
    let at_limit = Client { expires_at: h.ts + MAX_EXPIRY_AHEAD_MS, ..Client::new(1) };
    assert_eq!(fills(&h.cmd(at_limit.order(0, buy(client.account)))).len(), 1);
    let too_far = Client { expires_at: h.ts + MAX_EXPIRY_AHEAD_MS + 1, ..Client::new(1) };
    let latest = h.ts + MAX_EXPIRY_AHEAD_MS;
    assert_eq!(rejection(&h.cmd(too_far.order(1, buy(client.account)))), &Reject::ExpiryTooFar { latest });
    assert_eq!(h.engine.next_request_nonce(client.account), 2);
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, f("0.5"));
}

#[test]
fn a_nonce_ahead_of_the_next_is_rejected_and_leaves_the_next_unused() {
    let (mut h, client) = funded(Harness::signed());
    assert_eq!(rejection(&h.cmd(client.order(1, buy(client.account)))), &Reject::InvalidNonce { expected: 0 });
    assert_eq!(h.engine.next_request_nonce(client.account), 0);
    assert_eq!(fills(&h.cmd(client.order(0, buy(client.account)))).len(), 1);
}

#[test]
fn an_altered_request_is_rejected_as_an_invalid_signature_and_uses_no_nonce() {
    let (mut h, client) = funded(Harness::signed());
    let Command::PlaceOrder(mut req) = client.order(0, buy(client.account)) else { unreachable!() };
    req.qty = f("0.6");
    assert_eq!(rejection(&h.cmd(Command::PlaceOrder(req))), &Reject::InvalidSignature);
    assert_eq!(h.engine.next_request_nonce(client.account), 0);
}

#[test]
fn a_request_signed_by_another_key_is_rejected() {
    let (mut h, owner) = funded(Harness::signed());
    let thief = Client::new(2);
    assert_eq!(rejection(&h.cmd(thief.order(0, buy(owner.account)))), &Reject::InvalidSignature);
    assert_eq!(h.engine.next_request_nonce(owner.account), 0);
}

#[test]
fn a_request_signed_for_another_deployment_is_rejected() {
    let (mut h, client) = funded(Harness::signed());
    let elsewhere = Client { domain: eip712::domain(CHAIN_ID + 1, VAULT), ..Client::new(1) };
    assert_eq!(rejection(&h.cmd(elsewhere.order(0, buy(client.account)))), &Reject::InvalidSignature);
    let other_vault = Client { domain: eip712::domain(CHAIN_ID, AccountId([0xbb; 20])), ..Client::new(1) };
    assert_eq!(rejection(&h.cmd(other_vault.order(0, buy(client.account)))), &Reject::InvalidSignature);
}

#[test]
fn unsigned_requests_are_rejected_once_signatures_are_required() {
    let (mut h, client) = funded(Harness::signed());
    let account = client.account;
    assert_eq!(rejection(&h.cmd(Command::PlaceOrder(buy(account)))), &Reject::MissingSignature);
    let cancel = h.cmd(Command::CancelOrder { account, order_id: 0, auth: None });
    assert_eq!(rejection(&cancel), &Reject::MissingSignature);
    let withdraw = h.cmd(Command::Withdraw { account, amount: f("100"), auth: None });
    assert!(
        matches!(&withdraw[..], [Event::WithdrawalRejected { reason: Reject::MissingSignature, .. }]),
        "{withdraw:?}"
    );
    assert_eq!(h.balance(account), f("100000"));
}

#[test]
fn a_signed_request_the_engine_refuses_still_uses_its_nonce() {
    let mut h = Harness::signed();
    let client = Client::new(1);
    h.price(BTC, "100000");
    h.deposit(client.account, "100");
    let order = client.order(0, buy(client.account));
    assert!(matches!(rejection(&h.cmd(order.clone())), Reject::InsufficientMargin { .. }));
    assert_eq!(h.engine.next_request_nonce(client.account), 1);
    h.deposit(client.account, "100000");
    assert_eq!(rejection(&h.cmd(order)), &Reject::InvalidNonce { expected: 1 });
}

#[test]
fn a_sequenced_request_refused_before_the_first_deposit_cannot_run_after_it() {
    let mut h = Harness::signed();
    let client = Client::new(9);
    h.price(BTC, "100000");
    let order = client.order(0, buy(client.account));
    assert_eq!(rejection(&h.cmd(order.clone())), &Reject::InsufficientFunds { available: f("0") });
    assert_eq!(h.engine.next_request_nonce(client.account), 1);
    h.deposit(client.account, "100000");
    assert_eq!(rejection(&h.cmd(order)), &Reject::InvalidNonce { expected: 1 });
}

#[test]
fn refused_signed_cancels_and_withdrawals_still_use_their_nonce() {
    let (mut h, client) = funded(Harness::signed());
    let cancel = client.cancel(0, 99);
    assert_eq!(rejection(&h.cmd(cancel.clone())), &Reject::UnknownOrder);
    let withdraw = client.withdraw(1, "200000");
    let refused = h.cmd(withdraw.clone());
    assert!(matches!(&refused[..], [Event::WithdrawalRejected { reason: Reject::InsufficientFunds { .. }, .. }]));
    assert_eq!(h.engine.next_request_nonce(client.account), 2);
    assert_eq!(rejection(&h.cmd(cancel)), &Reject::InvalidNonce { expected: 2 });
    let replayed = h.cmd(withdraw);
    assert!(matches!(&replayed[..], [Event::WithdrawalRejected { reason: Reject::InvalidNonce { expected: 2 }, .. }]));
}

#[test]
fn signed_cancels_and_withdrawals_are_checked_like_orders() {
    let (mut h, client) = funded(Harness::signed());
    let limit = OrderRequest { kind: OrderKind::Limit { price: f("90000") }, ..buy(client.account) };
    let order_id = match &h.cmd(client.order(0, limit))[..] {
        [Event::OrderAccepted { order_id, .. }] => *order_id,
        events => panic!("expected the limit order to rest: {events:?}"),
    };
    // Each signature covers its order id or amount, so changing either breaks it.
    let Command::CancelOrder { auth, .. } = client.cancel(1, order_id) else { unreachable!() };
    let moved = h.cmd(Command::CancelOrder { account: client.account, order_id: order_id + 1, auth });
    assert_eq!(rejection(&moved), &Reject::InvalidSignature);
    let cancelled = h.cmd(client.cancel(1, order_id));
    assert_eq!(cancelled, [Event::OrderCancelled { account: client.account, order_id, reason: CancelReason::Client }]);

    let Command::Withdraw { auth, .. } = client.withdraw(2, "100") else { unreachable!() };
    let raised = h.cmd(Command::Withdraw { account: client.account, amount: f("200"), auth });
    assert!(matches!(&raised[..], [Event::WithdrawalRejected { reason: Reject::InvalidSignature, .. }]), "{raised:?}");
    let withdrawn = h.cmd(client.withdraw(2, "100"));
    assert!(matches!(&withdrawn[..], [Event::WithdrawalApproved { .. }]), "{withdrawn:?}");
    assert_eq!(h.engine.next_request_nonce(client.account), 3);
}

#[test]
fn v2_accepts_unsigned_requests_and_refuses_signed_ones() {
    let (mut h, client) = funded(Harness::new());
    // Recording client auth changes nothing until a version that applies it is active.
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    assert_eq!(rejection(&h.cmd(client.order(0, buy(client.account)))), &Reject::InvalidSignature);
    assert_eq!(fills(&h.cmd(Command::PlaceOrder(buy(client.account)))).len(), 1);
    assert_eq!(h.engine.next_request_nonce(client.account), 0);
}

#[test]
fn a_v2_log_upgraded_to_v3_takes_signed_requests_against_its_v2_state() {
    let (mut h, client) = funded(Harness::new());
    let limit = OrderRequest { kind: OrderKind::Limit { price: f("90000") }, ..buy(client.account) };
    let order_id = match &h.cmd(Command::PlaceOrder(limit))[..] {
        [Event::OrderAccepted { order_id, .. }] => *order_id,
        events => panic!("expected the V2 limit order to rest: {events:?}"),
    };
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V3 });

    assert_eq!(rejection(&h.cmd(Command::PlaceOrder(buy(client.account)))), &Reject::MissingSignature);
    assert_eq!(h.engine.next_request_nonce(client.account), 0);
    let cancelled = h.cmd(client.cancel(0, order_id));
    assert_eq!(cancelled, [Event::OrderCancelled { account: client.account, order_id, reason: CancelReason::Client }]);
    assert_eq!(fills(&h.cmd(client.order(1, buy(client.account)))).len(), 1);
}

#[test]
fn a_dev_log_accepts_unsigned_requests_under_v3() {
    let (mut h, client) = funded(Harness::v3(ClientAuth::Unsigned));
    assert_eq!(rejection(&h.cmd(client.order(0, buy(client.account)))), &Reject::InvalidSignature);
    assert_eq!(fills(&h.cmd(Command::PlaceOrder(buy(client.account)))).len(), 1);
}

#[test]
fn v3_activates_only_once_client_auth_is_recorded() {
    let mut h = Harness::new();
    let activate = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V3 });
    assert!(matches!(&h.cmd(activate.clone())[..], [Event::AdminRejected { .. }]));
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V2);
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    assert_eq!(h.cmd(activate), [Event::AdminApplied]);
}

#[test]
fn client_auth_is_recorded_once() {
    let mut h = Harness::signed();
    let events = h.cmd(Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)));
    assert_eq!(events, [Event::AdminRejected { reason: "client auth is already recorded".into() }]);
    assert_eq!(h.engine.client_auth(), Some(&ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
}

#[test]
fn protocol_versions_only_move_forward() {
    let mut h = Harness::new();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    let mut activate = |version| h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version }));
    assert!(matches!(&activate(ProtocolVersion::V2)[..], [Event::AdminRejected { .. }]));
    assert_eq!(activate(ProtocolVersion::V3), [Event::AdminApplied]);
    assert!(matches!(&activate(ProtocolVersion::V3)[..], [Event::AdminRejected { .. }]));
    assert!(matches!(&activate(ProtocolVersion::V2)[..], [Event::AdminRejected { .. }]));
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V3);
}

#[test]
fn client_auth_refuses_fields_this_build_does_not_know() {
    let known =
        format!(r#"{{"type":"admin","action":"set_client_auth","mode":"eip712","chain_id":1,"vault":"{VAULT}""#);
    let auth = ClientAuth::Eip712 { chain_id: 1, vault: VAULT };
    let parsed: Command = serde_json::from_str(&format!("{known}}}")).unwrap();
    assert_eq!(parsed, Command::Admin(AdminCommand::SetClientAuth(auth)));
    assert!(serde_json::from_str::<Command>(&format!(r#"{known},"salt":"0x01"}}"#)).is_err());
}

#[test]
fn a_request_with_a_misspelled_field_does_not_parse() {
    // Signing a request parsed without the field would sign, and run, something the client did not
    // ask for.
    let order =
        r#"{"account":"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266","client_id":1,"market":1,"side":"buy","qty":"1""#;
    let kind = r#""kind":{"kind":"market","quoted":"100000"}"#;
    assert!(serde_json::from_str::<OrderRequest>(&format!("{order},{kind}}}")).is_ok());
    assert!(serde_json::from_str::<OrderRequest>(&format!(r#"{order},{kind},"reduceOnly":true}}"#)).is_err());
    let misspelled = r#""kind":{"kind":"market","quoted":"100000","max_slipage_bps":5}"#;
    assert!(serde_json::from_str::<OrderRequest>(&format!("{order},{misspelled}}}")).is_err());
    let account = r#""account":"0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266""#;
    assert!(serde_json::from_str::<CancelRequest>(&format!(r#"{{{account},"order_id":5}}"#)).is_ok());
    assert!(serde_json::from_str::<CancelRequest>(&format!(r#"{{{account},"order_id":5,"client_id":1}}"#)).is_err());
    assert!(serde_json::from_str::<WithdrawalRequest>(&format!(r#"{{{account},"amount":"1"}}"#)).is_ok());
    assert!(serde_json::from_str::<WithdrawalRequest>(&format!(r#"{{{account},"amount":"1","to":"0x"}}"#)).is_err());
}

#[test]
fn v2_state_encodes_without_the_fields_versions_added() {
    let (mut h, client) = funded(Harness::new());
    h.cmd(Command::PlaceOrder(buy(client.account)));
    let added = ["protocol_version", "client_auth", "request_nonces"];
    let v2 = serde_json::to_value(&h.engine).unwrap();
    assert!(added.iter().all(|field| v2.get(field).is_none()), "{added:?} in a V2 encoding");

    let (mut h, client) = funded(Harness::signed());
    h.cmd(client.order(0, buy(client.account)));
    let v3 = serde_json::to_value(&h.engine).unwrap();
    assert!(added.iter().all(|field| v3.get(field).is_some()), "{added:?} missing from a V3 encoding");
}

// ── Helpers ──

/// `h` with BTC priced at 100000 and client 1 holding 100000.
fn funded(mut h: Harness) -> (Harness, Client) {
    let client = Client::new(1);
    h.price(BTC, "100000");
    h.deposit(client.account, "100000");
    (h, client)
}

/// A 0.5 BTC market buy for `account`, quoted at 100000, unsigned.
fn buy(account: AccountId) -> OrderRequest {
    OrderRequest {
        account,
        client_id: 7,
        market: BTC,
        side: Side::Buy,
        qty: f("0.5"),
        kind: OrderKind::Market { quoted: f("100000"), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
        take_profit: None,
        stop_loss: None,
    }
}
