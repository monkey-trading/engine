//! Session keys: an account authorises a short-lived, browser-held key to trade without a wallet
//! signature per order.
//!
//! One account-signed [`Command::RegisterSessionKey`](crate::command::Command::RegisterSessionKey) binds `session_key` to the account until
//! `expires_at`, capped at [`MAX_SESSION_KEY_EXPIRY_MS`] ahead, or until an account-signed
//! [`Command::RevokeSessionKey`](crate::command::Command::RevokeSessionKey). From `V41`, a request for the account signed by a registered,
//! unexpired session key is accepted for orders and cancels only — never withdrawals — and runs on
//! the key's own nonce sequence in `request_nonces` under the same per-request expiry cap as
//! account-signed requests (see `auth`). Registrations and revocations are sequenced commands, so a
//! log replays to its roots; each key's state is a leaf of the state tree.

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

/// A session key registered for an account, as the state tree holds it. The key's next request
/// nonce lives in `Engine::request_nonces` under the key's address, as every signer's does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionKey {
    /// The sequenced time from which the key's requests are refused as expired.
    pub expires_at: Millis,
    /// Set by a revocation and cleared by a fresh registration; a revoked key stays in state so its
    /// requests refuse distinctly, and its nonce stays spent.
    pub revoked: bool,
}

impl Engine {
    /// Registers `session_key` for `account` until `expires_at`, or refuses with
    /// `Event::SessionKeyRejected`. Only the account key's signature authorises a registration or a
    /// revocation: a session key never widens its own scope.
    /// Error cases:
    /// - before `V41`, or the registration names the account's own key or the operator's
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`)
    /// - `expires_at` is not in the future, or more than `MAX_SESSION_KEY_EXPIRY_MS` ahead
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
        if session_key == account || self.operator.is_some_and(|operator| operator.address == session_key) {
            events.push(rejected(Reject::InvalidSessionKey));
            return;
        }
        let latest = self.now.saturating_add(MAX_SESSION_KEY_EXPIRY_MS);
        if self.now >= expires_at {
            events.push(rejected(Reject::Expired { expires_at }));
            return;
        }
        if expires_at > latest {
            events.push(rejected(Reject::ExpiryTooFar { latest }));
            return;
        }
        let result = self.authorise(account, auth, RequestScope::Keys, |domain, auth| {
            eip712::session_key_registration_hash(domain, account, session_key, expires_at, auth.nonce, auth.expires_at)
        });
        if let Err(reason) = result {
            events.push(rejected(reason));
            return;
        }
        self.session_keys.insert((account, session_key), SessionKey { expires_at, revoked: false });
        events.push(Event::SessionKeyRegistered { account, session_key, expires_at });
    }

    /// Revokes `session_key` for `account`, refusing its later requests distinctly even where the
    /// key's expiry has not passed. The key's nonce stays spent, so a revoked key re-registered
    /// later never replays a request it signed before the revocation.
    /// Error cases:
    /// - before `V41`, or the key is not registered for the account
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`)
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
        if self.session_keys.get(&(account, session_key)).is_none() {
            events.push(rejected(Reject::InvalidSessionKey));
            return;
        }
        let result = self.authorise(account, auth, RequestScope::Keys, |domain, auth| {
            eip712::session_key_revocation_hash(domain, account, session_key, auth.nonce, auth.expires_at)
        });
        if let Err(reason) = result {
            events.push(rejected(reason));
            return;
        }
        let Some(key) = self.session_keys.get_mut(&(account, session_key)) else {
            unreachable!("checked above")
        };
        key.revoked = true;
        events.push(Event::SessionKeyRevoked { account, session_key });
    }

    /// The session key `session_key` registered for `account`, whatever its state.
    pub fn session_key(&self, account: AccountId, session_key: AccountId) -> Option<SessionKey> {
        self.session_keys.get(&(account, session_key)).copied()
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
