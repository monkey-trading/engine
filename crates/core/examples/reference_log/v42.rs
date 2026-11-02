//! The session's `V42` part: the introduction budget of new session-key addresses refills over
//! sequenced time in place of `V41`'s lifetime count.

use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::session_keys::{MAX_SESSION_KEY_REGISTRATIONS, SESSION_KEY_REFILL_MS};
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;

use crate::common::{Client, Harness};

/// Seeds of the two accounts this part registers session keys for.
const SPENT_SEED: u8 = 0x3c;
const FRESH_SEED: u8 = 0x3d;

/// The `index`th session key. Its bytes are not a repeated seed, so no key here is an address an
/// earlier part of the session has already used.
fn session_key(index: u8) -> Client {
    let mut bytes = [0x5d; 32];
    bytes[0] = 0x7e;
    bytes[31] = index;
    let key = SigningKey::from_slice(&bytes).expect("a valid scalar");
    Client { account: eip712::address(key.verifying_key()), key, ..Client::new(1) }
}

/// `seed`'s account, signing requests that expire a minute after the sequenced time.
fn account(h: &Harness, seed: u8) -> Client {
    Client { expires_at: h.ts + 60_000, ..Client::new(seed) }
}

/// Registers `key` for `account` until a millisecond ahead, with the account's next nonce.
fn register(h: &mut Harness, account: &Client, key: &Client) -> Vec<Event> {
    let nonce = h.engine.next_request_nonce(account.account);
    h.cmd(account.register_session_key(nonce, key.account, h.ts + 1))
}

fn registered(events: &[Event]) -> bool {
    matches!(events, [Event::SessionKeyRegistered { .. }])
}

fn assert_budget_empty(events: &[Event], account: AccountId, key: &Client) {
    let reason = Reject::TooManySessionKeys { max: MAX_SESSION_KEY_REGISTRATIONS as usize };
    assert_eq!(events, [Event::SessionKeyRejected { account, session_key: key.account, reason }]);
}

/// Moves the sequenced time to `ts` with a `Tick`.
fn tick_to(h: &mut Harness, ts: Millis) {
    h.advance_to(ts);
    h.cmd(Command::Tick);
}

/// One account introduces a hundred session keys under `V41` and the lifetime cap refuses the
/// next; `V42` activates, which starts that account's budget empty and another's full. Past
/// activation the empty budget refuses a new address until an hour of sequenced time returns one,
/// a known address registers free at empty, and the other account's budget falls by one.
pub(crate) fn session(h: &mut Harness) {
    let spent = account(h, SPENT_SEED);
    for index in 0..100 {
        let events = register(h, &spent, &session_key(index));
        assert!(registered(&events), "key {index}: {events:?}");
        h.advance(2);
    }
    let events = register(h, &spent, &session_key(100));
    assert_budget_empty(&events, spent.account, &session_key(100));

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V42 });
    let activated = h.ts;
    let fresh = account(h, FRESH_SEED);
    assert_eq!(h.engine.session_key_budget(spent.account), 0);
    assert_eq!(h.engine.session_key_budget(fresh.account), MAX_SESSION_KEY_REGISTRATIONS);
    let events = register(h, &spent, &session_key(100));
    assert_budget_empty(&events, spent.account, &session_key(100));

    // The refill counts from the activation: a millisecond short of the hour nothing has
    // returned. Half an hour past it one address has, and spending it keeps the half hour already
    // earned toward the next, which returns at two hours.
    tick_to(h, activated + SESSION_KEY_REFILL_MS - 1);
    let spent = account(h, SPENT_SEED);
    let events = register(h, &spent, &session_key(100));
    assert_budget_empty(&events, spent.account, &session_key(100));
    tick_to(h, activated + SESSION_KEY_REFILL_MS + SESSION_KEY_REFILL_MS / 2);
    let spent = account(h, SPENT_SEED);
    assert_eq!(h.engine.session_key_budget(spent.account), 1);
    let events = register(h, &spent, &session_key(100));
    assert!(registered(&events), "{events:?}");
    h.advance(2);
    let events = register(h, &spent, &session_key(101));
    assert_budget_empty(&events, spent.account, &session_key(101));
    tick_to(h, activated + 2 * SESSION_KEY_REFILL_MS);
    let spent = account(h, SPENT_SEED);
    assert_eq!(h.engine.session_key_budget(spent.account), 1);
    let events = register(h, &spent, &session_key(101));
    assert!(registered(&events), "{events:?}");
    h.advance(2);

    // A lapsed address the account introduced under `V41` registers again at an empty budget,
    // and a stranger's budget falls by one for a new address.
    let events = register(h, &spent, &session_key(7));
    assert!(registered(&events), "{events:?}");
    let fresh = account(h, FRESH_SEED);
    let events = register(h, &fresh, &session_key(200));
    assert!(registered(&events), "{events:?}");
    h.advance(2);
    assert_eq!(h.engine.session_key_budget(fresh.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
    assert_eq!(h.engine.session_key_budget(spent.account), 0);

    // Left idle for a hundred and a half hours, the budget is full, and the next spend restarts
    // its clock at that spend: the half hour past the capacity is not kept.
    let idle_to = h.ts + 100 * SESSION_KEY_REFILL_MS + SESSION_KEY_REFILL_MS / 2;
    while h.ts < idle_to {
        tick_to(h, (h.ts + 24 * SESSION_KEY_REFILL_MS).min(idle_to));
    }
    let fresh = account(h, FRESH_SEED);
    assert_eq!(h.engine.session_key_budget(fresh.account), MAX_SESSION_KEY_REGISTRATIONS);
    let events = register(h, &fresh, &session_key(201));
    assert!(registered(&events), "{events:?}");
    assert_eq!(h.engine.session_key_budget(fresh.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
    tick_to(h, idle_to + SESSION_KEY_REFILL_MS - 1);
    assert_eq!(h.engine.session_key_budget(fresh.account), MAX_SESSION_KEY_REGISTRATIONS - 1);
    tick_to(h, idle_to + SESSION_KEY_REFILL_MS);
    assert_eq!(h.engine.session_key_budget(fresh.account), MAX_SESSION_KEY_REGISTRATIONS);
}
