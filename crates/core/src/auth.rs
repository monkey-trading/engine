//! Who may act for an account, and who may change the rules. From protocol `V3`, every order,
//! cancel, and withdrawal is authorised as the log's `ClientAuth` records: under `Eip712`, by the
//! account's signature over the request, the account's next nonce, and an expiry the request has
//! not passed, so a replay of the log proves each request was the account's and ran in time. `V2`
//! logs, and dev logs that record `Unsigned`, accept unsigned requests. From `V8`, the operator
//! signs admin commands the same way, with the key the log's `Operator` records. From `V41`, an
//! account's registered session key may sign orders and cancels within its expiry, on the key's
//! own nonce sequence; a revoked or expired key, a key signing a withdrawal, and any registration
//! or revocation not signed by the account key refuse with their own rejections (see
//! `session_keys`).

use serde::{Deserialize, Serialize};

use crate::command::AdminCommand;
use crate::eip712::{self, B256, Eip712Domain, Signature};
use crate::engine::Engine;
use crate::event::Reject;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};

/// How far past the sequenced time a signed request's expiry may lie: five minutes. The engine
/// refuses a request with a later expiry and uses its nonce, so clients must sign short-lived
/// requests; the expiry a client signs is what caps how long the host, which controls the network,
/// can hold a captured request before delivering it. Five minutes leaves time to confirm in a
/// wallet and room for a clock a few minutes off. The cap holds only as far as sequenced time
/// tracks real time: it follows the host clock, which nothing yet bounds (the sequencer's
/// trusted-time TODO).
pub const MAX_EXPIRY_AHEAD_MS: Millis = 5 * 60 * 1000;

/// How a log authorises client requests from `V3`. Recorded once, so a replay needs only the log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientAuth {
    /// Anyone may act for any account. Development logs only.
    Unsigned,
    /// Each request carries the account's EIP-712 signature under the domain of `vault` on
    /// `chain_id`, the account's next nonce, and an expiry.
    Eip712 { chain_id: u64, vault: AccountId },
}

/// What a client request may do, which decides whether a session key may sign it: from `V41`, a
/// registered, unexpired session key may sign `Trade` requests only; `Funds` requests refuse it as
/// out of scope, and `Keys` requests (registrations and revocations) never admit one, so a session
/// key cannot widen its own scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestScope {
    Trade,
    Funds,
    Keys,
}

/// The key that signs admin commands from `V8`, by its address, and the domain it signs under: the
/// vault `vault` on chain `chain_id`. Recorded once, so a replay needs only the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operator {
    pub address: AccountId,
    pub chain_id: u64,
    pub vault: AccountId,
}

/// A client's signature over a request, the account nonce the request uses, and the latest
/// sequenced time the request itself is accepted at. A limit or stop order it places rests until it
/// fills or is cancelled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestAuth {
    pub nonce: u64,
    pub expires_at: Millis,
    pub signature: Signature,
}

impl ClientAuth {
    /// The domain requests are signed under, or `None` when they are unsigned.
    pub fn domain(&self) -> Option<Eip712Domain> {
        match self {
            ClientAuth::Unsigned => None,
            ClientAuth::Eip712 { chain_id, vault } => Some(eip712::domain(*chain_id, *vault)),
        }
    }

    /// Checks a request may act for `account` under these rules, leaving its nonce and expiry to
    /// the caller: unsigned under `Unsigned`, and signed by `account` under `Eip712`. `hash` gives
    /// the digest the request signs under a domain with its nonce and expiry.
    /// Error cases:
    /// - `MissingSignature`: unsigned under `Eip712`
    /// - `InvalidSignature`: signed under `Unsigned`, or not by `account`
    pub fn check(
        &self,
        account: AccountId,
        auth: Option<&RequestAuth>,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
    ) -> Result<(), Reject> {
        match (self.domain(), auth) {
            (None, None) => Ok(()),
            (Some(_), None) => Err(Reject::MissingSignature),
            (Some(domain), Some(auth)) if eip712::recover(&hash(&domain, auth), &auth.signature) == Some(account) => {
                Ok(())
            }
            (_, Some(_)) => Err(Reject::InvalidSignature),
        }
    }
}

impl Operator {
    /// Checks `auth` carries this operator's signature over `command` with its nonce and expiry,
    /// leaving the nonce and expiry to the caller.
    /// Error cases:
    /// - `InvalidSignature`: not signed by this operator's key, or over other content
    pub fn check(&self, command: &AdminCommand, auth: &RequestAuth) -> Result<(), Reject> {
        let domain = eip712::domain(self.chain_id, self.vault);
        let hash = eip712::admin_hash(&domain, command, auth.nonce, auth.expires_at);
        match eip712::recover(&hash, &auth.signature) {
            Some(signer) if signer == self.address => Ok(()),
            _ => Err(Reject::InvalidSignature),
        }
    }
}

impl RequestAuth {
    /// Checks the request may execute at sequenced time `now`.
    /// Error cases:
    /// - `Expired`: `now` is past `expires_at`
    /// - `ExpiryTooFar`: `expires_at` is more than `MAX_EXPIRY_AHEAD_MS` past `now`
    pub fn check_expiry(&self, now: Millis) -> Result<(), Reject> {
        let latest = now.saturating_add(MAX_EXPIRY_AHEAD_MS);
        if now > self.expires_at {
            Err(Reject::Expired { expires_at: self.expires_at })
        } else if self.expires_at > latest {
            Err(Reject::ExpiryTooFar { latest })
        } else {
            Ok(())
        }
    }
}

impl Engine {
    /// Checks a client request may act for `account` under the rules in force, and uses its nonce
    /// when it is signed. Nothing changes when the signature or nonce fails. A request that passes
    /// both uses its nonce even if it is then refused, for its expiry or by the engine, so it
    /// cannot be replayed from the log once it would succeed. From `V41`, under a signature-based
    /// ruleset, a request signed by a registered, unexpired session key is accepted for
    /// [`RequestScope::Trade`] under the key's own nonce sequence; `Funds` and `Keys` requests and
    /// dead keys refuse with their own rejections.
    /// Error cases:
    /// - `MissingSignature` or `InvalidSignature`: see `ClientAuth::check`
    /// - `InvalidNonce`: a nonce other than the signer's next
    /// - `Expired` or `ExpiryTooFar`: see `RequestAuth::check_expiry`
    /// - `SessionKeyRevoked` or `SessionKeyExpired`: a session key the account revoked, or one past
    ///   its registration's expiry
    /// - `SessionKeyScope`: a session key signed a `Funds` request
    pub(crate) fn authorise(
        &mut self,
        account: AccountId,
        auth: Option<&RequestAuth>,
        scope: RequestScope,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
    ) -> Result<(), Reject> {
        if self.protocol_version.supports_access() {
            let domain = self.access_domain().ok_or(Reject::InvalidSignature)?;
            let auth = auth.ok_or(Reject::MissingSignature)?;
            return self.authorise_signer(account, auth, scope, |auth| hash(&domain, auth));
        }
        const ORIGINAL: ClientAuth = ClientAuth::Unsigned;
        let rules = if self.protocol_version >= ProtocolVersion::V3 {
            // `ActivateProtocol` refuses `V3` until client auth is recorded.
            self.client_auth.as_ref().expect("client auth is recorded before V3")
        } else {
            &ORIGINAL
        };
        match (rules.domain(), auth) {
            (None, None) => Ok(()),
            (Some(_), None) => Err(Reject::MissingSignature),
            (Some(domain), Some(auth)) => self.authorise_signer(account, auth, scope, |a| hash(&domain, a)),
            (None, Some(_)) => Err(Reject::InvalidSignature),
        }
    }

    /// Uses `signer`'s next nonce if the request carries it, then checks the request's expiry.
    /// Error cases:
    /// - `InvalidNonce`: a nonce other than `signer`'s next; nothing changes
    /// - `Expired` or `ExpiryTooFar`: see `RequestAuth::check_expiry`; the nonce is used
    pub(crate) fn use_nonce(&mut self, signer: AccountId, auth: &RequestAuth) -> Result<(), Reject> {
        let expected = self.next_request_nonce(signer);
        if auth.nonce != expected {
            return Err(Reject::InvalidNonce { expected });
        }
        // One nonce per request: 2^64 requests from one signer cannot happen.
        self.request_nonces.insert(signer, expected + 1);
        auth.check_expiry(self.now)
    }

    /// Checks a signed request's signature names `account`, or a session key of `account` allowed
    /// by `scope`, and uses the signer's nonce. A request a registered session key signs spends
    /// the key's nonce even when the key's state or the request's scope then refuses it, exactly as
    /// an account-signed request a refusal cannot replay behaves.
    /// Error cases:
    /// - `InvalidSignature`: not signed by `account` or, from `V41`, by one of its session keys
    ///   within `scope`
    /// - the rejections of `use_nonce`
    /// - `SessionKeyRevoked` or `SessionKeyExpired`: the key's state refuses it
    /// - `SessionKeyScope`: a live session key signed a `Funds` request
    fn authorise_signer(
        &mut self,
        account: AccountId,
        auth: &RequestAuth,
        scope: RequestScope,
        hash: impl FnOnce(&RequestAuth) -> B256,
    ) -> Result<(), Reject> {
        let signer = eip712::recover(&hash(auth), &auth.signature).ok_or(Reject::InvalidSignature)?;
        if signer == account {
            return self.use_nonce(account, auth);
        }
        if scope == RequestScope::Keys || self.protocol_version < ProtocolVersion::V41 {
            return Err(Reject::InvalidSignature);
        }
        let Some(key) = self.session_keys.get(&(account, signer)).copied() else {
            return Err(Reject::InvalidSignature);
        };
        // The key is registered, so the request spends its nonce even when the key's state or the
        // request's scope then refuses it: a captured request cannot replay across a revocation
        // and re-registration, exactly as an account-signed request behaves.
        self.use_nonce(signer, auth)?;
        if key.revoked {
            return Err(Reject::SessionKeyRevoked);
        }
        if self.now > key.expires_at {
            return Err(Reject::SessionKeyExpired { expires_at: key.expires_at });
        }
        if scope == RequestScope::Funds {
            return Err(Reject::SessionKeyScope);
        }
        Ok(())
    }

    /// Checks an admin command is signed by the operator the log records, and uses the operator's
    /// nonce, as `authorise` does for a client request.
    /// Error cases:
    /// - `InvalidSignature`: see `Operator::check`
    /// - `InvalidNonce`, `Expired`, or `ExpiryTooFar`: see `use_nonce`
    pub(crate) fn authorise_admin(&mut self, command: &AdminCommand, auth: &RequestAuth) -> Result<(), Reject> {
        // `ActivateProtocol` refuses `V8` until the operator is recorded, and callers check `V8`.
        let operator = self.operator.expect("the operator is recorded before V8");
        operator.check(command, auth)?;
        self.use_nonce(operator.address, auth)
    }
}
