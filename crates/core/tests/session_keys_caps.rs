//! Session-key retention: the per-account live cap and its pruning, the lifetime cap on
//! distinct introduced keys, and the replay protection a pruned key keeps.

mod common;

use common::events::{fills, rejection};
use common::session_keys::{HOUR_MS, buy, expiring_trade, funded, register, register_expiring, trade};
use common::{BTC, Client};
use engine_core::event::{Event, Reject};
use engine_core::session_keys::{MAX_SESSION_KEY_REGISTRATIONS, MAX_SESSION_KEYS_PER_ACCOUNT};

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
    // Renewing a key the account already holds replaces it and is not a new key: it succeeds at
    // the cap, where an eleventh distinct key refused.
    let cmd = register_expiring(&h, &client, &keys[0], h.ts + 2 * HOUR_MS, h.ts + 60_000);
    let events = h.cmd(cmd);
    assert!(
        matches!(events[..], [Event::SessionKeyRegistered { session_key, .. }] if session_key == keys[0].account),
        "{events:?}"
    );

    // Expired keys are pruned by the next registration, which then fits under the cap; a revoked
    // key is pruned the same way, and its nonce stays spent.
    h.advance(HOUR_MS + 1);
    h.price(BTC, "100000");
    // The client's fixed request expiry has passed; the same key signs with a fresh one.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    let events = h.cmd(register_expiring(&h, &client, &key, h.ts + HOUR_MS, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    let nonce = h.engine.next_request_nonce(client.account);
    let events = h.cmd(client.revoke_session_key(nonce, key.account));
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
fn a_pruned_key_keeps_its_replay_protection() {
    let (mut h, client, key) = funded();
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    assert_eq!(fills(&trade(&mut h, &key, 0, buy(client.account))).len(), 1);
    h.cmd(client.revoke_session_key(2, key.account));
    // A fresh registration of another key prunes the revoked one; its nonce entry stays spent.
    let other = Client::new(9);
    h.cmd(register(&h, &client, &other, h.ts + HOUR_MS));
    assert_eq!(h.engine.session_key(client.account, key.account), None);
    assert_eq!(h.engine.next_request_nonce(key.account), 1);

    // An order the pruned key signed before the revocation lands now: it refuses as an invalid
    // signature but still spends the key's nonce, so re-registering the key cannot replay it.
    let events = trade(&mut h, &key, 1, buy(client.account));
    assert_eq!(rejection(&events), &Reject::InvalidSignature);
    assert_eq!(h.engine.next_request_nonce(key.account), 2);
    // Two keys introduced so far (`key` and `other`); re-registering the pruned one adds nothing.
    assert_eq!(h.engine.session_key_registrations(client.account), 2);
    h.cmd(register(&h, &client, &key, h.ts + HOUR_MS));
    assert_eq!(h.engine.session_key_registrations(client.account), 2);
    let events = trade(&mut h, &key, 1, buy(client.account));
    assert_eq!(rejection(&events), &Reject::InvalidNonce { expected: 2 });
    let events = trade(&mut h, &key, 2, buy(client.account));
    assert_eq!(fills(&events).len(), 1, "{events:?}");
}

#[test]
fn an_account_cannot_introduce_more_than_a_lifetime_of_session_keys() {
    let (mut h, client, _) = funded();
    assert_eq!(MAX_SESSION_KEY_REGISTRATIONS, 100);
    // Nine distinct short-lived keys a batch, expired before the next batch: pruning keeps the
    // live count under the cap while the lifetime total grows to its bound of a hundred.
    for batch in 0..11u64 {
        for i in 0..9u8 {
            let key = Client::new(0x60 + batch as u8 * 9 + i);
            let cmd = register_expiring(&h, &client, &key, h.ts + 1_000, h.ts + 60_000);
            let events = h.cmd(cmd);
            assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "batch {batch}: {events:?}");
        }
        h.advance(1_001);
    }
    let hundredth = Client::new(0x60 + 99);
    let cmd = register_expiring(&h, &client, &hundredth, h.ts + 1_000, h.ts + 60_000);
    let events = h.cmd(cmd);
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    assert_eq!(h.engine.session_key_registrations(client.account), 100);
    // Renewing the one live held key still succeeds at the lifetime limit and counts nothing.
    let events = h.cmd(register_expiring(&h, &client, &hundredth, h.ts + HOUR_MS, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    assert_eq!(h.engine.session_key_registrations(client.account), 100);
    // Re-registering a lapsed key the account has used before introduces nothing either.
    let lapsed = Client::new(0x60);
    let events = h.cmd(register_expiring(&h, &client, &lapsed, h.ts + HOUR_MS, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    assert_eq!(h.engine.session_key_registrations(client.account), 100);
    // The hundred-and-first distinct key refuses even with live keys pruned under the cap.
    let overflow = Client::new(0xd0);
    let cmd = register_expiring(&h, &client, &overflow, h.ts + 1_000, h.ts + 60_000);
    let events = h.cmd(cmd);
    assert_eq!(
        events,
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: overflow.account,
            reason: Reject::TooManySessionKeys { max: MAX_SESSION_KEY_REGISTRATIONS as usize },
        }]
    );
}
