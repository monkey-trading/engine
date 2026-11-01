//! Session keys: one account-signed registration authorises a browser-held key to place and
//! cancel orders on the key's own nonce sequence until its expiry or an account-signed
//! revocation; withdrawals keep requiring the account key.

mod common;

use common::close_spread::{attest, fund};
use common::events::{fills, rejection};
use common::session_keys::{
    HOUR_MS, buy, expiring_trade, funded, limit, register, register_expiring, trade, trade_cmd,
};
use common::{BTC, Client, Harness, f};
use engine_core::Engine;
use engine_core::auth::{MAX_EXPIRY_AHEAD_MS, RequestAuth};
use engine_core::command::{Command, OrderRequest};
use engine_core::eip712;
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::session_keys::{MAX_SESSION_KEY_EXPIRY_MS, MAX_SESSION_KEYS_PER_ACCOUNT, SessionKey};

#[test]
fn one_account_signature_registers_a_key_that_trades_without_the_wallet() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    let cmd = register(&h, &client, &key, expires_at);
    let events = h.cmd(cmd);
    assert_eq!(events, [Event::SessionKeyRegistered { account: client.account, session_key: key.account, expires_at }]);
    assert_eq!(h.engine.session_key(client.account, key.account), Some(SessionKey { expires_at, revoked: false }));

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
    let start = h.log.len();
    let expires_at = h.ts + HOUR_MS;
    let mut live = Vec::new();
    let mut sequence = |h: &mut Harness, cmd: Command| {
        let events = h.cmd(cmd);
        live.push(h.engine.state_root());
        events
    };
    let cmd = register(&h, &client, &key, expires_at);
    sequence(&mut h, cmd);
    let cmd = trade_cmd(&h, &key, 0, buy(client.account));
    sequence(&mut h, cmd);
    let cmd = client.revoke_session_key(2, key.account);
    sequence(&mut h, cmd);
    let cmd = trade_cmd(&h, &key, 1, buy(client.account));
    let events = sequence(&mut h, cmd);
    assert_eq!(rejection(&events), &Reject::SessionKeyRevoked);

    // The log alone reproduces the roots the live engine recorded after each command, and its
    // final state is the state the commitment covers.
    let mut replay = Engine::new();
    for (index, sequenced) in h.log.iter().enumerate() {
        replay.apply(sequenced).expect("in sequence");
        if index >= start {
            assert_eq!(&replay.state_root(), &live[index - start], "root diverges at log index {index}");
        }
    }
    assert_eq!(replay.session_key(client.account, key.account), Some(SessionKey { expires_at, revoked: true }));
}

#[test]
fn a_session_key_withdrawal_is_refused_distinctly_and_moves_nothing() {
    let (mut h, client, key) = funded();
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    let hash = eip712::withdrawal_hash(&key.domain, client.account, f("100"), 0, h.ts + 60_000);
    let auth = RequestAuth { nonce: 0, expires_at: h.ts + 60_000, signature: eip712::sign(&key.key, &hash) };
    let events = h.cmd(Command::Withdraw { account: client.account, amount: f("100"), auth: Some(auth) });
    assert!(matches!(&events[..], [Event::WithdrawalRejected { reason: Reject::SessionKeyScope, .. }]), "{events:?}");
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
fn a_key_request_runs_at_exactly_its_expiry_and_not_past_it() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    h.cmd(register(&h, &client, &key, expires_at));
    // The key's requests run at exactly `expires_at`…
    h.advance(HOUR_MS);
    h.price(BTC, "100000");
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 0, buy(client.account), expiry);
    assert_eq!(fills(&events).len(), 1, "{events:?}");
    // …and refuse from the next millisecond on.
    h.advance(1);
    h.price(BTC, "100000");
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 1, buy(client.account), expiry);
    assert_eq!(rejection(&events), &Reject::SessionKeyExpired { expires_at });
}

#[test]
fn a_key_registered_for_one_account_cannot_act_for_another() {
    let (mut h, client, key) = funded();
    let other = Client::new(6);
    attest(&mut h, &other);
    fund(&mut h, &other, "100000");
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));

    // The key is registered for `client` only: an order for `other` it signs is an invalid
    // signature, and neither sequence moves (`other`'s next nonce is 1 after its declaration).
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 0, buy(other.account), expiry);
    assert_eq!(rejection(&events), &Reject::InvalidSignature);
    assert_eq!(h.engine.next_request_nonce(key.account), 0);
    assert_eq!(h.engine.next_request_nonce(other.account), 1);

    // Two accounts may register the same key independently: revoking it for one leaves the
    // other's registration live.
    h.cmd(register(&h, &other, &key, h.ts + 2 * HOUR_MS));
    h.cmd(client.revoke_session_key(2, key.account));
    assert_eq!(
        h.engine.session_key(other.account, key.account),
        Some(SessionKey { expires_at: h.ts + 2 * HOUR_MS, revoked: false })
    );
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 0, buy(other.account), expiry);
    assert_eq!(fills(&events).len(), 1, "{events:?}");
}

#[test]
fn a_registration_past_the_per_account_cap_refuses_and_prunes_expired_keys() {
    let (mut h, client, key) = funded();
    let keys: Vec<Client> = (20..20 + MAX_SESSION_KEYS_PER_ACCOUNT as u8).map(Client::new).collect();
    assert_eq!(MAX_SESSION_KEYS_PER_ACCOUNT, 10);
    for key in &keys {
        h.cmd(register(&h, &client, key, h.ts + HOUR_MS));
    }
    // The eleventh live key refuses; the account's nonce is spent, so the refusal cannot replay
    // once the account prunes its expired keys.
    let overflow = Client::new(0x55);
    let cmd = register(&h, &client, &overflow, h.ts + HOUR_MS);
    let events = h.cmd(cmd);
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: overflow.account,
            reason: Reject::TooManySessionKeys { max: MAX_SESSION_KEYS_PER_ACCOUNT },
        }]
    );
    assert_eq!(h.engine.next_request_nonce(client.account), 12);

    // Expired keys are pruned by the next registration, which then fits under the cap; a revoked
    // key is pruned the same way, and its nonce stays spent.
    h.advance(HOUR_MS + 1);
    h.price(BTC, "100000");
    // The client's fixed request expiry has passed; the same key signs with a fresh one.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    let events = h.cmd(register_expiring(&h, &client, &key, h.ts + HOUR_MS, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    let events = h.cmd(client.revoke_session_key(13, key.account));
    assert!(matches!(events[..], [Event::SessionKeyRevoked { .. }]), "{events:?}");
    let events = h.cmd(register_expiring(&h, &client, &overflow, h.ts + HOUR_MS, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    assert_eq!(h.engine.session_key(client.account, key.account), None);
    // The pruned key is unknown now, and its nonce sequence never rewound.
    let expiry = h.ts + 60_000;
    let events = expiring_trade(&mut h, &key, 0, buy(client.account), expiry);
    assert_eq!(rejection(&events), &Reject::InvalidSignature);
}

#[test]
fn a_revoked_session_key_is_refused_distinctly() {
    let (mut h, client, key) = funded();
    let expires_at = h.ts + HOUR_MS;
    h.cmd(register(&h, &client, &key, expires_at));
    let events = h.cmd(client.revoke_session_key(2, key.account));
    assert_eq!(events, [Event::SessionKeyRevoked { account: client.account, session_key: key.account }]);
    assert_eq!(h.engine.session_key(client.account, key.account), Some(SessionKey { expires_at, revoked: true }));
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
    assert_eq!(events, [Event::SessionKeyRejected { account, session_key, reason: Reject::InvalidSignature }]);
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
    assert_eq!(events, [Event::SessionKeyRejected { account, session_key, reason: Reject::InvalidSignature }]);
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
    let events = h.cmd(client.revoke_session_key(0, key.account));
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: key.account,
            reason: Reject::SessionKeyUnsupported,
        }]
    );
}
