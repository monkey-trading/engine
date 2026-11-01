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

/// How many session keys an account may register over its life. Each registration that introduces
/// a new key leaves the key's request nonce in `request_nonces` for good, so it can keep its
/// replay protection after pruning; the lifetime cap bounds that state. Renewing a key the
/// account already holds does not count.
pub const MAX_SESSION_KEY_REGISTRATIONS: u64 = 100;

/// The session keys one account registered, and how many it has introduced: `total` bounds the
/// permanent nonce state a session key leaves, and lets a registration refuse past the lifetime
/// cap even while `keys` is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSessionKeys {
    /// The live registrations, pruned of expired and revoked keys by each registration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<AccountId, SessionKey>,
    /// Every key the account has introduced, for the lifetime cap.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub total: u64,
}

/// Whether the count is zero, so its default stays out of earlier state encodings.
fn is_zero(count: &u64) -> bool {
    *count == 0
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
    /// [`MAX_SESSION_KEYS_PER_ACCOUNT`] live or [`MAX_SESSION_KEY_REGISTRATIONS`] over the
    /// account's life, so one account's state stays bounded; renewing a key the account already
    /// holds replaces it and never counts against either cap.
    /// Error cases:
    /// - before `V41`
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`)
    /// - the registration names the account's own key or the operator's
    /// - `expires_at` is not in the future, or more than `MAX_SESSION_KEY_EXPIRY_MS` ahead
    /// - the account already holds `MAX_SESSION_KEYS_PER_ACCOUNT` live keys and `session_key`
    ///   is not one of them, or has introduced `MAX_SESSION_KEY_REGISTRATIONS` keys over its life
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
        let entry = self.session_keys.entry(account).or_default();
        if !held && entry.keys.len() >= MAX_SESSION_KEYS_PER_ACCOUNT {
            events.push(rejected(Reject::TooManySessionKeys { max: MAX_SESSION_KEYS_PER_ACCOUNT }));
            return;
        }
        if !held && entry.total >= MAX_SESSION_KEY_REGISTRATIONS {
            events.push(rejected(Reject::TooManySessionKeys { max: MAX_SESSION_KEY_REGISTRATIONS as usize }));
            return;
        }
        if !held {
            entry.total += 1;
        }
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

    /// Every session key `account` has introduced, for the lifetime cap.
    pub fn session_key_registrations(&self, account: AccountId) -> u64 {
        self.session_keys.get(&account).map_or(0, |keys| keys.total)
    }

    /// Drops `account`'s expired and revoked session keys. Spent nonces stay in
    /// `request_nonces`, so pruning never revives a request a pruned key signed: a later request
    /// from a pruned key still spends its nonce and refuses as an invalid signature.
    fn prune_session_keys(&mut self, account: AccountId) {
        let Some(keys) = self.session_keys.get_mut(&account) else { return };
        keys.keys.retain(|_, key| !key.revoked && self.now <= key.expires_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_key_round_trips_its_state() {
        let key = SessionKey { expires_at: 1_700_000_000_000, revoked: true };
        let json = serde_json::to_string(&key).expect("session keys serialise");
        assert_eq!(serde_json::from_str::<SessionKey>(&json).expect("session keys deserialise"), key);
    }
}
