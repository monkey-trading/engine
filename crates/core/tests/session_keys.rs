//! Session keys: one account-signed registration authorises a browser-held key to place and
//! cancel orders on the key's own nonce sequence until its expiry or an account-signed
//! revocation; withdrawals keep requiring the account key.

mod common;

use common::close_spread::{attest, ceilings, evidence, fund, source_key};
use common::events::{fills, rejection};
use common::{BTC, CHAIN_ID, Client, Harness, VAULT, f};
use engine_core::Engine;
use engine_core::access::AccessPolicy;
use engine_core::auth::{ClientAuth, RequestAuth, MAX_EXPIRY_AHEAD_MS};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::session_keys::{MAX_SESSION_KEY_EXPIRY_MS, SessionKey};
use engine_core::types::{AccountId, Millis, Side};

const HOUR_MS: Millis = 60 * 60 * 1000;

#[test]
fn one_account_signature_registers_a_key_that_trades_without_the_wallet() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    let cmd = register(&h, &client, &key, expires_at);
    let events = h.cmd(cmd);
    assert_eq!(
        events,
        [Event::SessionKeyRegistered { account: client.account, session_key: key.account, expires_at }]
    );
    assert_eq!(
        h.engine.session_key(client.account, key.account),
        Some(SessionKey { expires_at, revoked: false })
    );

    // The key's order fills on the key's nonce sequence; the account's sequence never moves.
    let events = trade(&mut h, &key, 0, buy(client.account));
    assert_eq!(fills(&events).len(), 1, "{events:?}");
    assert_eq!(h.engine.next_request_nonce(key.account), 1);
    assert_eq!(h.engine.next_request_nonce(client.account), 2);

    // A resting order placed by the key is cancelled by the key.
    let events = trade(&mut h, &key, 1, limit(client.account));
    let order_id = match events[..] {
        [Event::OrderAccepted { order_id, .. }] => order_id,
        _ => panic!("expected acceptance, got {events:?}"),
    };
    let hash = eip712::cancel_hash(&key.domain, client.account, order_id, 2, key.expires_at);
    let auth = RequestAuth { nonce: 2, expires_at: key.expires_at, signature: eip712::sign(&key.key, &hash) };
    let events = h.cmd(Command::CancelOrder { account: client.account, order_id, auth: Some(auth) });
    assert_eq!(events, [Event::OrderCancelled { account: client.account, order_id, reason: CancelReason::Client }]);
    assert_eq!(h.engine.next_request_nonce(key.account), 3);
    assert_eq!(h.engine.next_request_nonce(client.account), 2);
}

#[test]
fn a_log_with_registrations_replays_to_its_roots() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    h.cmd(register(&h, &client, &key, expires_at));
    trade(&mut h, &key, 0, buy(client.account));
    h.cmd(client.revoke_session_key(2, key.account));
    let events = trade(&mut h, &key, 1, buy(client.account));
    assert_eq!(rejection(&events), &Reject::SessionKeyRevoked);

    // The log alone must reproduce every state root: a first replay records them, a second must
    // match each one, and the refusals the log records must be its final state.
    let expected = roots(&h.log);
    assert_eq!(roots(&h.log), expected);
    assert_eq!(
        h.engine.session_key(client.account, key.account),
        Some(SessionKey { expires_at, revoked: true })
    );
}

/// The state root after each command of `log`, from a fresh engine.
fn roots(log: &[engine_core::command::Sequenced]) -> Vec<[u8; 32]> {
    let mut engine = Engine::new();
    log.iter()
        .map(|sequenced| {
            engine.apply(sequenced).expect("in sequence");
            engine.state_root()
        })
        .collect()
}

#[test]
fn a_session_key_withdrawal_is_refused_distinctly_and_moves_nothing() {
    let (mut h, client, key) = funded();
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    let hash = eip712::withdrawal_hash(&key.domain, client.account, f("100"), 0, h.ts + 60_000);
    let auth = RequestAuth { nonce: 0, expires_at: h.ts + 60_000, signature: eip712::sign(&key.key, &hash) };
    let events = h.cmd(Command::Withdraw { account: client.account, amount: f("100"), auth: Some(auth) });
    assert!(
        matches!(&events[..], [Event::WithdrawalRejected { reason: Reject::SessionKeyScope, .. }]),
        "{events:?}"
    );
    assert_eq!(h.balance(client.account), f("100000"));
    // The refused request used the key's nonce, exactly as an account-signed refusal does.
    assert_eq!(h.engine.next_request_nonce(key.account), 1);
    assert_eq!(h.engine.next_request_nonce(client.account), 2);
}

#[test]
fn an_expired_session_key_is_refused_distinctly_until_it_is_registered_again() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    h.cmd(register(&h, &client, &key, expires_at));
    h.advance(HOUR_MS + 1);
    h.price(BTC, "100000");
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 0, buy(client.account), expiry);
    assert_eq!(rejection(&events), &Reject::SessionKeyExpired { expires_at });
    // The expiry refusal used the key's nonce.
    assert_eq!(h.engine.next_request_nonce(key.account), 1);

    // A fresh registration with a later expiry revives the key; its nonce stays spent, so a
    // request captured before the expiry cannot replay.
    let cmd = register_expiring(&h, &client, &key, h.ts + HOUR_MS, h.ts + 60_000);
    let events = h.cmd(cmd);
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 0, buy(client.account), expiry);
    assert_eq!(rejection(&events), &Reject::InvalidNonce { expected: 1 });
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 1, buy(client.account), expiry);
    assert_eq!(fills(&events).len(), 1, "{events:?}");
}

#[test]
fn a_revoked_session_key_is_refused_distinctly() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    h.cmd(register(&h, &client, &key, expires_at));
    let events = h.cmd(client.revoke_session_key(2, key.account));
    assert_eq!(events, [Event::SessionKeyRevoked { account: client.account, session_key: key.account }]);
    assert_eq!(
        h.engine.session_key(client.account, key.account),
        Some(SessionKey { expires_at, revoked: true })
    );
    let events = trade(&mut h, &key, 0, buy(client.account));
    assert_eq!(rejection(&events), &Reject::SessionKeyRevoked);
    // A revocation the account never registered refuses as an invalid key.
    let stranger = Client::new(9);
    let events = h.cmd(client.revoke_session_key(3, stranger.account));
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: stranger.account,
            reason: Reject::InvalidSessionKey,
        }]
    );
}

#[test]
fn each_session_key_keeps_its_own_nonce_sequence() {
    let (mut h, client, key) = funded();
    let other = Client::new(8);
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    h.cmd(register(&h, &client, &other, h.ts + HOUR_MS));
    assert_eq!(fills(&trade(&mut h, &key, 0, buy(client.account))).len(), 1);
    assert_eq!(fills(&trade(&mut h, &other, 0, buy(client.account))).len(), 1);
    assert_eq!(rejection(&trade(&mut h, &key, 0, buy(client.account))), &Reject::InvalidNonce { expected: 1 });
    assert_eq!(rejection(&trade(&mut h, &other, 2, buy(client.account))), &Reject::InvalidNonce { expected: 1 });
    assert_eq!(fills(&trade(&mut h, &key, 1, buy(client.account))).len(), 1);
    assert_eq!(h.engine.next_request_nonce(client.account), 3);
    assert_eq!((h.engine.next_request_nonce(key.account), h.engine.next_request_nonce(other.account)), (2, 1));
}

#[test]
fn a_session_key_request_obeys_the_same_request_expiry_rules_and_uses_its_nonce_when_refused() {
    let (mut h, client, key) = funded();
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    let latest = h.ts + MAX_EXPIRY_AHEAD_MS;
    let events = expiring_trade(&mut h, &key, 0, buy(client.account), latest + 1);
    assert_eq!(rejection(&events), &Reject::ExpiryTooFar { latest });
    assert_eq!(h.engine.next_request_nonce(key.account), 1);

    let expiry = h.ts - 1;
    let events = expiring_trade(&mut h, &key, 1, buy(client.account), expiry);
    assert_eq!(rejection(&events), &Reject::Expired { expires_at: h.ts - 1 });
    assert_eq!(h.engine.next_request_nonce(key.account), 2);

    // An engine refusal after a valid signature uses the nonce, so it cannot be replayed later.
    let small = OrderRequest { qty: f("1000"), ..buy(client.account) };
    let events = trade(&mut h, &key, 2, small);
    assert!(matches!(rejection(&events), Reject::InsufficientMargin { .. }), "{events:?}");
    let events = trade(&mut h, &key, 2, buy(client.account));
    assert_eq!(rejection(&events), &Reject::InvalidNonce { expected: 3 });
}

#[test]
fn a_registration_expiry_is_capped_at_a_day_and_must_lie_in_the_future() {
    let (mut h, client, key) = funded();
    assert_eq!(MAX_SESSION_KEY_EXPIRY_MS, 24 * HOUR_MS);
    let latest = h.ts + MAX_SESSION_KEY_EXPIRY_MS;
    let cmd = register(&h, &client, &key, latest + 1);
    let events = h.cmd(cmd);
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: key.account,
            reason: Reject::ExpiryTooFar { latest },
        }]
    );
    let cmd = register(&h, &client, &key, h.ts);
    let events = h.cmd(cmd);
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: key.account,
            reason: Reject::Expired { expires_at: h.ts },
        }]
    );
    let cmd = register(&h, &client, &key, h.ts + 1);
    let events = h.cmd(cmd);
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
}

#[test]
fn only_the_account_key_registers_or_revokes() {
    let (mut h, client, key) = funded();
    let stranger = Client::new(9);
    // A third party's registration signature refuses and spends nothing.
    let cmd = register(&h, &client, &key, h.ts + HOUR_MS);
    let Command::RegisterSessionKey { account, session_key, expires_at, auth: _ } = cmd else {
        unreachable!("a registration")
    };
    let auth = RequestAuth {
        nonce: 0,
        expires_at: stranger.expires_at,
        signature: eip712::sign(
            &stranger.key,
            &eip712::session_key_registration_hash(
                &stranger.domain,
                account,
                session_key,
                expires_at,
                0,
                stranger.expires_at,
            ),
        ),
    };
    let events = h.cmd(Command::RegisterSessionKey { account, session_key, expires_at, auth: Some(auth) });
    assert_eq!(
        events,
        [Event::SessionKeyRejected { account, session_key, reason: Reject::InvalidSignature }]
    );
    assert_eq!(h.engine.next_request_nonce(client.account), 1);

    // Once registered, the key cannot register another key or revoke itself: key management is
    // the account's alone.
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    let cmd = register(&h, &client, &stranger, h.ts + HOUR_MS);
    let Command::RegisterSessionKey { account, session_key, expires_at, auth: _ } = cmd else {
        unreachable!("a registration")
    };
    let auth = RequestAuth {
        nonce: 0,
        expires_at: key.expires_at,
        signature: eip712::sign(
            &key.key,
            &eip712::session_key_registration_hash(&key.domain, account, session_key, expires_at, 0, key.expires_at),
        ),
    };
    let events = h.cmd(Command::RegisterSessionKey { account, session_key, expires_at, auth: Some(auth) });
    assert_eq!(
        events,
        [Event::SessionKeyRejected { account, session_key, reason: Reject::InvalidSignature }]
    );
    let auth = RequestAuth {
        nonce: 1,
        expires_at: key.expires_at,
        signature: eip712::sign(
            &key.key,
            &eip712::session_key_revocation_hash(&key.domain, account, key.account, 1, key.expires_at),
        ),
    };
    let events = h.cmd(Command::RevokeSessionKey { account, session_key: key.account, auth: Some(auth) });
    assert_eq!(
        events,
        [Event::SessionKeyRejected { account, session_key: key.account, reason: Reject::InvalidSignature }]
    );
}

#[test]
fn an_unsigned_registration_is_missing_its_signature() {
    let (mut h, client, key) = funded();
    let events = h.cmd(Command::RegisterSessionKey {
        account: client.account,
        session_key: key.account,
        expires_at: h.ts + HOUR_MS,
        auth: None,
    });
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: key.account,
            reason: Reject::MissingSignature,
        }]
    );
}

#[test]
fn the_account_and_operator_addresses_cannot_be_session_keys() {
    let (mut h, client, key) = funded();
    let _ = key;
    for invalid in [client.account, Client::operator().account] {
        let events = h.cmd(register(&h, &client, &Client { account: invalid, ..Client::new(3) }, h.ts + HOUR_MS));
        assert_eq!(
            events,
            [Event::SessionKeyRejected {
                account: client.account,
                session_key: invalid,
                reason: Reject::InvalidSessionKey,
            }]
        );
    }
}

#[test]
fn session_keys_are_unsupported_before_v41() {
    let mut h = Harness::signed();
    let client = Client::new(1);
    h.price(BTC, "100000");
    h.deposit(client.account, "100000");
    let key = Client::new(7);
    let cmd = register(&h, &client, &key, h.ts + HOUR_MS);
    let events = h.cmd(cmd);
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: key.account,
            reason: Reject::SessionKeyUnsupported,
        }]
    );
    assert!(h.engine.session_key(client.account, key.account).is_none());
}

// ── Helpers ──

/// The `V41` harness under access rules: signed requests, the operator recorded, the ceiling
/// registry the `V40` activation check wants, a screening source whose evidence stays fresh across
/// an hour of sequenced time, client 1 declared and holding 100000, and BTC priced at 100000.
fn funded() -> (Harness, Client, Client) {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() }));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V41 });
    let client = Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client, "100000");
    h.price(BTC, "100000");
    (h, client, Client::new(7))
}

/// `client`'s signed registration of `key` until `session_expires_at`, using the account's next
/// nonce and the client's request expiry.
fn register(h: &Harness, client: &Client, key: &Client, session_expires_at: Millis) -> Command {
    register_expiring(h, client, key, session_expires_at, client.expires_at)
}

/// `register` with an explicit request expiry, for registrations sequenced after the client's
/// fixed request expiry has passed.
fn register_expiring(h: &Harness, client: &Client, key: &Client, session_expires_at: Millis, expires_at: Millis) -> Command {
    let nonce = h.engine.next_request_nonce(client.account);
    let hash = eip712::session_key_registration_hash(
        &client.domain,
        client.account,
        key.account,
        session_expires_at,
        nonce,
        expires_at,
    );
    Command::RegisterSessionKey {
        account: client.account,
        session_key: key.account,
        expires_at: session_expires_at,
        auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) }),
    }
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
    }
}

/// A resting 0.4 BTC limit buy at 90000 for `account`, unsigned.
fn limit(account: AccountId) -> OrderRequest {
    OrderRequest { kind: OrderKind::Limit { price: f("90000") }, qty: f("0.4"), ..buy(account) }
}

/// `req` signed by `key` for `account` with `nonce` and the key client's request expiry, screened
/// like the account's own orders, and sequenced.
fn trade(h: &mut Harness, key: &Client, nonce: u64, mut req: OrderRequest) -> Vec<Event> {
    let Command::PlaceOrder(req) = signed_order(key, nonce, &mut req, key.expires_at) else { unreachable!() };
    let auth = req.auth.as_ref().expect("signed above");
    let hash = eip712::order_hash(&key.domain, &req, auth.nonce, auth.expires_at);
    let ev = evidence(h, hash, Some(req.account), None);
    h.cmd(Command::AccessOrder { request: req, evidence: ev })
}

/// `trade` with a request expiry of `expires_at` instead of the key client's.
fn expiring_trade(h: &mut Harness, key: &Client, nonce: u64, mut req: OrderRequest, expires_at: Millis) -> Vec<Event> {
    let Command::PlaceOrder(req) = signed_order(key, nonce, &mut req, expires_at) else { unreachable!() };
    let auth = req.auth.as_ref().expect("signed above");
    let hash = eip712::order_hash(&key.domain, &req, auth.nonce, auth.expires_at);
    let ev = evidence(h, hash, Some(req.account), None);
    h.cmd(Command::AccessOrder { request: req, evidence: ev })
}

/// `req` with `key`'s signature for its account over `expires_at`, as `Command::PlaceOrder`.
fn signed_order(key: &Client, nonce: u64, req: &mut OrderRequest, expires_at: Millis) -> Command {
    let hash = eip712::order_hash(&key.domain, req, nonce, expires_at);
    req.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&key.key, &hash) });
    Command::PlaceOrder(req.clone())
}
