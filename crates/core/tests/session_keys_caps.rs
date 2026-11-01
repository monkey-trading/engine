//! Session-key retention: the per-account live cap and its pruning, the `V41` lifetime cap on
//! distinct introduced keys and the `V42` budget that refills over sequenced time in its place, and
//! the replay protection a pruned key keeps.

mod common;

use common::events::{fills, rejection};
use common::session_keys::{
    HOUR_MS, buy, expiring_trade, fresh_key, funded, funded_at, idle_until, introduce, register, register_expiring,
    trade,
};
use common::{BTC, Client, Harness};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::session_keys::{MAX_SESSION_KEY_REGISTRATIONS, MAX_SESSION_KEYS_PER_ACCOUNT, SESSION_KEY_REFILL_MS};
use engine_core::types::{AccountId, Millis};

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

// ── V42: the introduction budget refills over sequenced time ──

fn registered(events: &[Event]) -> bool {
    matches!(events, [Event::SessionKeyRegistered { .. }])
}

fn budget_empty(account: AccountId, session_key: AccountId) -> [Event; 1] {
    [Event::SessionKeyRejected {
        account,
        session_key,
        reason: Reject::TooManySessionKeys { max: MAX_SESSION_KEY_REGISTRATIONS as usize },
    }]
}

/// Moves the clock to `ts` and sequences a `Tick`, so the engine's time (which the budget reads)
/// is `ts`.
fn tick_to(h: &mut Harness, ts: Millis) {
    h.advance_to(ts);
    h.cmd(Command::Tick);
}

/// Introduces keys `0..n` and returns the time of the first, when the budget left its capacity.
fn spend(h: &mut Harness, client: &Client, n: u8) -> Millis {
    let first = h.ts;
    for index in 0..n {
        let events = introduce(h, client, index);
        assert!(registered(&events), "key {index}: {events:?}");
    }
    first
}

#[test]
fn a_v42_account_spends_to_empty_and_refuses_the_next_new_address() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    assert_eq!(SESSION_KEY_REFILL_MS, HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS);
    for index in 0..100u8 {
        let events = introduce(&mut h, &client, index);
        assert!(registered(&events), "key {index}: {events:?}");
        assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS - 1 - index as u64);
    }
    assert_eq!(h.engine.session_key_registrations(client.account), 100);

    let nonce = h.engine.next_request_nonce(client.account);
    let events = introduce(&mut h, &client, 100);
    assert_eq!(events, budget_empty(client.account, fresh_key(100).account));
    // The refusal spends the account's nonce and introduces no address.
    assert_eq!(h.engine.next_request_nonce(client.account), nonce + 1);
    assert_eq!(h.engine.session_key_registrations(client.account), 100);
    assert_eq!(h.engine.session_key_budget(client.account), 0);
}

#[test]
fn an_empty_budget_returns_one_address_per_elapsed_hour() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    let t0 = spend(&mut h, &client, 100);
    assert_eq!(h.engine.session_key_budget(client.account), 0);

    // One millisecond short of the hour nothing has returned; at the hour exactly one has.
    tick_to(&mut h, t0 + HOUR_MS - 1);
    let events = introduce(&mut h, &client, 100);
    assert_eq!(events, budget_empty(client.account, fresh_key(100).account));
    tick_to(&mut h, t0 + HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), 1);
    assert!(registered(&introduce(&mut h, &client, 100)));
    // That spent it: the next new address waits for the next hour.
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    assert!(!registered(&introduce(&mut h, &client, 101)));
    tick_to(&mut h, t0 + 2 * HOUR_MS);
    assert!(registered(&introduce(&mut h, &client, 101)));
}

#[test]
fn a_partial_hour_carries_across_registrations() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    let t0 = spend(&mut h, &client, 100);

    // Half an hour past the first refill, one address is spent. The half hour already earned
    // toward the next one is kept, not restarted by the spend.
    tick_to(&mut h, t0 + HOUR_MS + HOUR_MS / 2);
    assert_eq!(h.engine.session_key_budget(client.account), 1);
    assert!(registered(&introduce(&mut h, &client, 100)));
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    tick_to(&mut h, t0 + 2 * HOUR_MS - 1);
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    assert!(!registered(&introduce(&mut h, &client, 101)));
    tick_to(&mut h, t0 + 2 * HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), 1);
    assert!(registered(&introduce(&mut h, &client, 101)));

    // A read or a refusal in between neither loses nor adds refill: across one hour of
    // refusals and reads at odd offsets, exactly one more address has returned.
    for offset in [10, HOUR_MS / 3, HOUR_MS - 1] {
        tick_to(&mut h, t0 + 2 * HOUR_MS + offset);
        assert!(!registered(&introduce(&mut h, &client, 102)), "offset {offset}");
    }
    tick_to(&mut h, t0 + 3 * HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), 1);
}

#[test]
fn a_long_idle_gap_refills_only_to_the_capacity() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    let t0 = spend(&mut h, &client, 100);
    idle_until(&mut h, t0 + 150 * HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS);
    // The whole capacity is spendable at once and no more: the hundred-and-first refuses.
    for index in 0..100u8 {
        assert!(registered(&introduce(&mut h, &client, 100u8.wrapping_add(index))), "key {index}");
    }
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    let events = introduce(&mut h, &client, 200);
    assert_eq!(events, budget_empty(client.account, fresh_key(200).account));
}

#[test]
fn saturating_mid_hour_restarts_the_refill_clock_at_the_spend() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    let t0 = spend(&mut h, &client, 100);
    // Idle to capacity plus half an hour: the partial hour is not kept, because a full budget
    // earns nothing. One spend leaves 99, and its first refill is a whole hour after the spend.
    let spent_at = t0 + 150 * HOUR_MS + HOUR_MS / 2;
    idle_until(&mut h, spent_at);
    assert!(registered(&introduce(&mut h, &client, 100)));
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
    tick_to(&mut h, spent_at + HOUR_MS / 2);
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
    tick_to(&mut h, spent_at + HOUR_MS - 1);
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
    tick_to(&mut h, spent_at + HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS);
}

#[test]
fn a_refused_live_cap_spends_no_budget() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    for index in 0..MAX_SESSION_KEYS_PER_ACCOUNT as u8 {
        let cmd = register_expiring(&h, &client, &fresh_key(index), h.ts + HOUR_MS, h.ts + 60_000);
        assert!(registered(&h.cmd(cmd)), "key {index}");
    }
    let before = h.engine.session_key_budget(client.account);
    assert_eq!(before, MAX_SESSION_KEY_REGISTRATIONS - MAX_SESSION_KEYS_PER_ACCOUNT as u64);
    let overflow = fresh_key(50);
    let cmd = register_expiring(&h, &client, &overflow, h.ts + HOUR_MS, h.ts + 60_000);
    assert_eq!(
        h.cmd(cmd),
        [Event::SessionKeyRejected {
            account: client.account,
            session_key: overflow.account,
            reason: Reject::TooManySessionKeys { max: MAX_SESSION_KEYS_PER_ACCOUNT },
        }]
    );
    assert_eq!(h.engine.session_key_budget(client.account), before);
    assert_eq!(h.engine.session_key_registrations(client.account), MAX_SESSION_KEYS_PER_ACCOUNT as u64);
}

#[test]
fn a_known_address_stays_free_when_the_budget_is_empty() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    spend(&mut h, &client, 100);
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    // Keys 0..99 lapsed while the budget drained; re-registering one introduces nothing.
    let lapsed = fresh_key(3);
    let cmd = register_expiring(&h, &client, &lapsed, h.ts + HOUR_MS, h.ts + 60_000);
    assert!(registered(&h.cmd(cmd)));
    // A held key renews the same way, and a revoked one revives.
    let cmd = register_expiring(&h, &client, &lapsed, h.ts + 2 * HOUR_MS, h.ts + 60_000);
    assert!(registered(&h.cmd(cmd)));
    let nonce = h.engine.next_request_nonce(client.account);
    assert!(matches!(h.cmd(client.revoke_session_key(nonce, lapsed.account))[..], [Event::SessionKeyRevoked { .. }]));
    let cmd = register_expiring(&h, &client, &lapsed, h.ts + HOUR_MS, h.ts + 60_000);
    assert!(registered(&h.cmd(cmd)));
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    assert_eq!(h.engine.session_key_registrations(client.account), 100);
}

#[test]
fn a_budget_belongs_to_one_account() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V42);
    spend(&mut h, &client, 100);
    let other = Client::new(2);
    assert_eq!(h.engine.session_key_budget(other.account), MAX_SESSION_KEY_REGISTRATIONS);
    let cmd = register_expiring(&h, &other, &fresh_key(150), h.ts + 1_000, h.ts + 60_000);
    assert!(registered(&h.cmd(cmd)));
    assert_eq!(h.engine.session_key_budget(other.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
}

#[test]
fn v41_keeps_its_fixed_lifetime_cap_however_long_the_account_waits() {
    let (mut h, client, _) = funded();
    let first = h.ts;
    for index in 0..100u8 {
        assert!(registered(&introduce(&mut h, &client, index)), "key {index}");
    }
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    idle_until(&mut h, first + 150 * HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    let events = introduce(&mut h, &client, 100);
    assert_eq!(events, budget_empty(client.account, fresh_key(100).account));
    assert_eq!(h.engine.session_key_registrations(client.account), 100);
}

#[test]
fn activation_starts_each_budget_at_the_capacity_less_what_v41_spent() {
    let (mut h, client, _) = funded();
    let thirty = Client::new(2);
    let untouched = Client::new(3);
    for index in 0..100u8 {
        assert!(registered(&introduce(&mut h, &client, index)), "key {index}");
    }
    for index in 100..130u8 {
        let cmd = register_expiring(&h, &thirty, &fresh_key(index), h.ts + 1, h.ts + 60_000);
        assert!(registered(&h.cmd(cmd)), "key {index}");
        h.advance(2);
    }
    let until = h.ts + 5 * HOUR_MS;
    idle_until(&mut h, until);
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    assert_eq!(h.engine.session_key_budget(thirty.account), 70);

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V42 });
    let activated = h.ts;
    // The five idle hours before activation earn nothing: the exhausted account starts empty and
    // the partly spent one starts at 70, neither above the capacity.
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    assert_eq!(h.engine.session_key_budget(thirty.account), 70);
    assert_eq!(h.engine.session_key_budget(untouched.account), MAX_SESSION_KEY_REGISTRATIONS);
    let events = introduce(&mut h, &client, 130);
    assert_eq!(events, budget_empty(client.account, fresh_key(130).account));
    // The refill counts from the activation: its first hour returns the first address.
    tick_to(&mut h, activated + HOUR_MS - 1);
    assert_eq!(h.engine.session_key_budget(client.account), 0);
    tick_to(&mut h, activated + HOUR_MS);
    assert_eq!(h.engine.session_key_budget(client.account), 1);
    assert_eq!(h.engine.session_key_budget(thirty.account), 71);
    assert!(registered(&introduce(&mut h, &client, 130)));
    // Registrations after the activation keep counting introductions.
    assert_eq!(h.engine.session_key_registrations(client.account), 101);

    // Idle long enough, both accounts stop at the capacity, never above it.
    let until = h.ts + 150 * HOUR_MS;
    idle_until(&mut h, until);
    assert_eq!(h.engine.session_key_budget(client.account), MAX_SESSION_KEY_REGISTRATIONS);
    assert_eq!(h.engine.session_key_budget(thirty.account), MAX_SESSION_KEY_REGISTRATIONS);
}
