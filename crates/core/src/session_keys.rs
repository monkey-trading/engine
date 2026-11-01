//! Session keys: an account authorises a short-lived, browser-held key to trade without a wallet
//! signature per order.
//!
//! One account-signed [`Command::RegisterSessionKey`](crate::command::Command::RegisterSessionKey) binds `session_key` to the account until
//! `expires_at`, capped at [`MAX_SESSION_KEY_EXPIRY_MS`] ahead, or until an account-signed
//! [`Command::RevokeSessionKey`](crate::command::Command::RevokeSessionKey). From `V41`, a request for the account signed by a registered,
//! unexpired session key is accepted for orders and cancels only — never withdrawals — and runs on
//! the key's own nonce sequence in `request_nonces` under the same per-request expiry cap as
//! account-signed requests (see `auth`). Registrations and revocations are sequenced commands, so a
//! log replays to its roots; an account's keys are one leaf of the state tree, pruned and
//! capped so it stays bounded.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::auth::{RequestAuth, RequestScope};
use crate::eip712;
use crate::engine::Engine;
use crate::event::{Event, Reject};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};

/// How far past the sequenced time a session key's registration may expire: 24 hours. The cap is a
/// protocol invariant: a stolen in-browser key can trade for at most this long before its
/// authorisation lapses, whatever expiry the account signed.
pub const MAX_SESSION_KEY_EXPIRY_MS: Millis = 24 * 60 * 60 * 1000;

/// How many session keys an account may hold: generous for one key per device, small enough that
/// an account's leaf and the work a root spends on it stay bounded whatever one account sequences.
pub const MAX_SESSION_KEYS_PER_ACCOUNT: usize = 10;

/// How many distinct session-key addresses an account may introduce in one burst: the capacity of
/// its introduction budget. Each introduction seeds the key's request-nonce entry, which stays in
/// `request_nonces` for good so a pruned key keeps its replay protection; the budget bounds that
/// state. Under `V41` the count was a lifetime cap; from `V42` the budget refills at one address
/// per [`SESSION_KEY_REFILL_MS`], so an account holds at most this many plus one per elapsed
/// refill interval. Re-registering a key the account has used before — held, lapsed, or revoked —
/// introduces nothing and spends nothing.
pub const MAX_SESSION_KEY_REGISTRATIONS: u64 = 100;

/// From `V42`, the sequenced time that returns one address to an account's introduction budget:
/// one hour, so a client that generates a fresh key at each connect never locks itself out.
pub const SESSION_KEY_REFILL_MS: Millis = 60 * 60 * 1000;

/// The session keys one account registered, and how many distinct keys it has introduced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSessionKeys {
    /// The live registrations, pruned of expired and revoked keys by each registration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<AccountId, SessionKey>,
    /// Every distinct key the account has introduced. Under `V41` this was the lifetime cap; from
    /// `V42` it is a count only, and `budget` bounds introductions.
    #[serde(default)]
    pub total: u64,
    /// From `V42`, the account's introduction budget; absent for an account that has introduced
    /// nothing since `V42`, whose budget is full. Absent in every leaf before `V42`, so earlier
    /// roots are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<IntroductionBudget>,
}

/// An account's budget of new session-key addresses, refilling one per [`SESSION_KEY_REFILL_MS`]
/// of sequenced time up to [`MAX_SESSION_KEY_REGISTRATIONS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntroductionBudget {
    /// Addresses that may be introduced as of `refilled_at`; at most the capacity.
    pub available: u64,
    /// The sequenced time the refill has been counted up to. While the budget is below capacity it
    /// trails the latest refill by the part of an interval not yet earned, so a partial interval
    /// carries; at capacity it is the time the budget was last read.
    pub refilled_at: Millis,
}

impl IntroductionBudget {
    /// A full budget as of `now`.
    fn full(now: Millis) -> Self {
        Self { available: MAX_SESSION_KEY_REGISTRATIONS, refilled_at: now }
    }

    /// The budget at `now`: every whole interval since `refilled_at` returns one address, the
    /// remainder carries, and a budget that reaches capacity stops accruing. Sequenced time never
    /// runs backwards, and a reading before `refilled_at` earns nothing.
    fn at(self, now: Millis) -> Self {
        let earned = u64::try_from(now.saturating_sub(self.refilled_at) / SESSION_KEY_REFILL_MS).unwrap_or(0);
        let available = self.available.saturating_add(earned);
        if available >= MAX_SESSION_KEY_REGISTRATIONS {
            return Self::full(now);
        }
        // `earned` whole intervals fit in the elapsed time, so this cannot overflow.
        Self { available, refilled_at: self.refilled_at.saturating_add(earned as Millis * SESSION_KEY_REFILL_MS) }
    }
}

/// A session key registered for an account, as the state tree holds it. The key's next request
/// nonce lives in `Engine::request_nonces` under the key's address, as every signer's does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKey {
    /// The sequenced time at and before which the key's requests run; later ones refuse as
    /// expired.
    pub expires_at: Millis,
    /// Set by a revocation and cleared by a fresh registration; a revoked key refuses distinctly
    /// until the account's next registration prunes it, and its nonce stays spent.
    pub revoked: bool,
}

impl Engine {
    /// Registers `session_key` for `account` until `expires_at`, or refuses with
    /// `Event::SessionKeyRejected`. Only the account key's signature authorises a registration or a
    /// revocation: a session key never widens its own scope. The account's nonce is spent by any
    /// refusal after its signature checks, so a refused registration cannot be replayed from the
    /// log once it would succeed. A registration first prunes the account's expired and revoked
    /// keys — their nonces stay spent in `request_nonces` — and refuses a new key past
    /// [`MAX_SESSION_KEYS_PER_ACCOUNT`] live keys or an empty introduction budget, so one account's
    /// state stays bounded: under `V41` at most [`MAX_SESSION_KEY_REGISTRATIONS`] distinct keys
    /// over the account's life, and from `V42` at most that many plus one per elapsed
    /// [`SESSION_KEY_REFILL_MS`]. Re-registering a key the account has used before, held or
    /// lapsed, replaces it and spends nothing.
    /// Error cases:
    /// - before `V41`
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`)
    /// - the registration names the account's own key or the operator's
    /// - `expires_at` is not in the future, or more than `MAX_SESSION_KEY_EXPIRY_MS` ahead
    /// - the account already holds `MAX_SESSION_KEYS_PER_ACCOUNT` live keys and `session_key`
    ///   is not one of them
    /// - `session_key` is a new address and, under `V41`, the account has introduced
    ///   `MAX_SESSION_KEY_REGISTRATIONS` distinct keys, or, from `V42`, its introduction budget is
    ///   empty
    pub(crate) fn register_session_key(
        &mut self,
        account: AccountId,
        session_key: AccountId,
        expires_at: Millis,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        let rejected = |reason| Event::SessionKeyRejected { account, session_key, reason };
        if self.protocol_version < ProtocolVersion::V41 {
            events.push(rejected(Reject::SessionKeyUnsupported));
            return;
        }
        if let Err(reason) = self.authorise(account, auth, RequestScope::Keys, |domain, auth| {
            eip712::session_key_registration_hash(domain, account, session_key, expires_at, auth.nonce, auth.expires_at)
        }) {
            events.push(rejected(reason));
            return;
        }
        if session_key == account || self.operator.is_some_and(|operator| operator.address == session_key) {
            events.push(rejected(Reject::InvalidSessionKey));
            return;
        }
        self.prune_session_keys(account);
        let latest = self.now.saturating_add(MAX_SESSION_KEY_EXPIRY_MS);
        if self.now >= expires_at {
            events.push(rejected(Reject::Expired { expires_at }));
            return;
        }
        if expires_at > latest {
            events.push(rejected(Reject::ExpiryTooFar { latest }));
            return;
        }
        let held = self.session_keys.get(&account).is_some_and(|keys| keys.keys.contains_key(&session_key));
        // A key whose nonce entry exists has been introduced already: re-registering it, lapsed
        // or revoked, introduces nothing and spends nothing.
        let known = self.request_nonces.contains_key(&session_key);
        let entry = self.session_keys.entry(account).or_default();
        if !held && entry.keys.len() >= MAX_SESSION_KEYS_PER_ACCOUNT {
            events.push(rejected(Reject::TooManySessionKeys { max: MAX_SESSION_KEYS_PER_ACCOUNT }));
            return;
        }
        if !known {
            let too_many = Reject::TooManySessionKeys { max: MAX_SESSION_KEY_REGISTRATIONS as usize };
            if self.protocol_version >= ProtocolVersion::V42 {
                let budget = entry.budget.unwrap_or(IntroductionBudget::full(self.now)).at(self.now);
                if budget.available == 0 {
                    events.push(rejected(too_many));
                    return;
                }
                entry.budget = Some(IntroductionBudget { available: budget.available - 1, ..budget });
            } else if entry.total >= MAX_SESSION_KEY_REGISTRATIONS {
                events.push(rejected(too_many));
                return;
            }
            entry.total += 1;
        }
        // Every registered key owns its nonce entry from the start, so even a key pruned before
        // its first request keeps its replay protection (see `auth`'s pruned-signer path).
        self.request_nonces.entry(session_key).or_insert(0);
        entry.keys.insert(session_key, SessionKey { expires_at, revoked: false });
        events.push(Event::SessionKeyRegistered { account, session_key, expires_at });
    }

    /// Revokes `session_key` for `account`, refusing its later requests distinctly even where the
    /// key's expiry has not passed. The key's nonce stays spent, so a revoked key re-registered
    /// later never replays a request it signed before the revocation; the account's nonce is spent
    /// by any refusal after its signature checks, so a refused revocation cannot be replayed once
    /// the key is registered.
    /// Error cases:
    /// - before `V41`
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`)
    /// - the key is not registered for the account
    pub(crate) fn revoke_session_key(
        &mut self,
        account: AccountId,
        session_key: AccountId,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        let rejected = |reason| Event::SessionKeyRejected { account, session_key, reason };
        if self.protocol_version < ProtocolVersion::V41 {
            events.push(rejected(Reject::SessionKeyUnsupported));
            return;
        }
        if let Err(reason) = self.authorise(account, auth, RequestScope::Keys, |domain, auth| {
            eip712::session_key_revocation_hash(domain, account, session_key, auth.nonce, auth.expires_at)
        }) {
            events.push(rejected(reason));
            return;
        }
        let Some(keys) = self.session_keys.get_mut(&account) else {
            events.push(rejected(Reject::InvalidSessionKey));
            return;
        };
        let Some(key) = keys.keys.get_mut(&session_key) else {
            events.push(rejected(Reject::InvalidSessionKey));
            return;
        };
        key.revoked = true;
        events.push(Event::SessionKeyRevoked { account, session_key });
    }

    /// The session key `session_key` registered for `account`, whatever its state.
    pub fn session_key(&self, account: AccountId, session_key: AccountId) -> Option<SessionKey> {
        self.session_keys.get(&account)?.keys.get(&session_key).copied()
    }

    /// Every distinct session key `account` has introduced over its life.
    pub fn session_key_registrations(&self, account: AccountId) -> u64 {
        self.session_keys.get(&account).map_or(0, |keys| keys.total)
    }

    /// How many new session-key addresses `account` may introduce now: its introduction budget at
    /// the engine's time from `V42`, and the lifetime cap's remainder before.
    pub fn session_key_budget(&self, account: AccountId) -> u64 {
        let keys = self.session_keys.get(&account);
        if self.protocol_version < ProtocolVersion::V42 {
            return MAX_SESSION_KEY_REGISTRATIONS.saturating_sub(keys.map_or(0, |keys| keys.total));
        }
        keys.and_then(|keys| keys.budget).unwrap_or(IntroductionBudget::full(self.now)).at(self.now).available
    }

    /// Starts every account's introduction budget at `V42` activation: the capacity less the
    /// addresses it introduced under `V41`, counting up from the activation time. An account at
    /// the old lifetime cap starts empty and earns its first address one interval later; none
    /// starts above the capacity.
    pub(crate) fn start_introduction_budgets(&mut self) {
        let now = self.now;
        for keys in self.session_keys.iter_mut().map(|(_, keys)| keys) {
            let available = MAX_SESSION_KEY_REGISTRATIONS.saturating_sub(keys.total);
            keys.budget = Some(IntroductionBudget { available, refilled_at: now });
        }
    }

    /// Drops `account`'s expired and revoked session keys. Every key's nonce entry was seeded at
    /// its registration and stays in `request_nonces`, so pruning never revives a request a
    /// pruned key signed: a later request from a pruned key still spends its nonce and refuses as
    /// an invalid signature.
    fn prune_session_keys(&mut self, account: AccountId) {
        let Some(keys) = self.session_keys.get_mut(&account) else { return };
        keys.keys.retain(|_, state| !state.revoked && self.now <= state.expires_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Reading the budget in steps equals reading it once, so frequent registrations neither
        /// lose nor gain refill; it never exceeds the capacity or earns more than the time allows;
        /// and nothing overflows for any times the sequencer can stamp.
        #[test]
        fn the_budget_refills_by_elapsed_time_whatever_the_reads_between(
            available in 0..=MAX_SESSION_KEY_REGISTRATIONS,
            start in 0..=i64::MAX / 4,
            first in 0..=150 * SESSION_KEY_REFILL_MS,
            second in 0..=150 * SESSION_KEY_REFILL_MS,
        ) {
            let budget = IntroductionBudget { available, refilled_at: start };
            let (mid, end) = (start + first, start + first + second);
            let stepped = budget.at(mid).at(end);
            let once = budget.at(end);
            prop_assert_eq!(stepped, once);
            prop_assert!(once.available <= MAX_SESSION_KEY_REGISTRATIONS);
            prop_assert!(once.available >= available);
            let earned = (end - start) / SESSION_KEY_REFILL_MS;
            prop_assert!(once.available - available <= earned as u64);
            if once.available < MAX_SESSION_KEY_REGISTRATIONS {
                prop_assert_eq!(once.available, available + earned as u64);
                prop_assert!(end - once.refilled_at < SESSION_KEY_REFILL_MS);
            }
        }

        /// Any times the sequencer can stamp neither overflow nor leave the capacity.
        #[test]
        fn the_budget_stays_within_the_capacity_at_any_time(
            available in 0..=MAX_SESSION_KEY_REGISTRATIONS,
            refilled_at in 0..=i64::MAX,
            now in 0..=i64::MAX,
        ) {
            let read = IntroductionBudget { available, refilled_at }.at(now);
            prop_assert!(read.available <= MAX_SESSION_KEY_REGISTRATIONS);
        }

        /// A reading before the counted time earns nothing and cannot underflow.
        #[test]
        fn a_reading_before_the_counted_time_earns_nothing(
            available in 0..=MAX_SESSION_KEY_REGISTRATIONS,
            refilled_at in 0..=i64::MAX,
            back in 0..=i64::MAX,
        ) {
            let budget = IntroductionBudget { available, refilled_at };
            let read = budget.at(refilled_at.saturating_sub(back));
            prop_assert_eq!(read.available, available);
        }
    }

    #[test]
    fn a_session_key_round_trips_its_state() {
        let key = SessionKey { expires_at: 1_700_000_000_000, revoked: true };
        let json = serde_json::to_string(&key).expect("session keys serialise");
        assert_eq!(serde_json::from_str::<SessionKey>(&json).expect("session keys deserialise"), key);
    }
}
