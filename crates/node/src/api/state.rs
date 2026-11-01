//! Authentication and durable sequencer round trips for client routes.

use std::net::IpAddr;

use engine_core::Engine;
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::Command;
use engine_core::eip712::{B256, Eip712Domain, recover};
use engine_core::event::{Event, Reject};
use engine_core::types::{AccountId, Millis};
use tokio::sync::oneshot;
use tracing::field::display;
use tracing::{Span, debug};

use crate::api::{ApiError, ApiState};
use crate::sequencer;

impl ApiState {
    /// Whether the build's access rules apply, including while startup activation waits for trusted
    /// time.
    pub(super) fn access_enforced(&self) -> bool {
        self.protocol.supports_access()
    }

    /// Checks the request may act for `account`, records it as the request's id, counts it against
    /// the account's rate, and returns the auth to sequence with it: the client's, or none where
    /// the node trusts the `account` field.
    /// `hash` gives the digest the request signs under a domain with its nonce and expiry. A signed
    /// request stays out of the log unless its account has been credited, its nonce is the
    /// account's next, and it has not expired: otherwise it could do nothing now, a nonce ahead of
    /// the next would stay replayable from the public log, and free keys or copied requests could
    /// grow the log and engine state. A request refused this way uses no nonce, so it stays valid
    /// until its expiry. The engine checks the nonce and expiry again at the sequenced time.
    /// Error cases:
    /// - signatures are required and the request has none, or one that is not `account`'s
    /// - the request is signed and the engine has never credited `account`
    /// - the request is signed with a nonce other than the account's next
    /// - the request is signed with an expiry already past by the clock or the last sequenced time
    /// - the account is over its rate
    /// - the sequencer has stopped
    /// `authorise_request` with `session` deciding whether a registered session key's signature
    /// may stand in for the account's: true for the requests a session key may make (orders,
    /// cancels, and the withdrawals the engine refuses in the log as out of scope), false for key
    /// management, which is the account key's alone at the API too, so a session key's signature
    /// on a registration or revocation never reaches the log.
    pub(super) async fn authorise(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        session: bool,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
    ) -> Result<Option<RequestAuth>, ApiError> {
        self.authorise_request(account, auth, hash, session, false, None).await
    }

    /// `authorise`, except that a `declaration` may come from a never-credited account once a deposit
    /// for it is pending. That is the only request such an account can put in the log, and it is
    /// admitted only while the account has not declared and with an expiry the engine accepts. The
    /// engine can still refuse it on its screening, using its nonce, so the account may declare again;
    /// each attempt is counted against the account's rate, the share of fresh-account admissions its
    /// client `peer` may take, and all clients' combined budget.
    /// Error cases:
    /// - as `authorise`, except that a never-credited account with a pending deposit may declare
    /// - a never-credited account has already declared, or signs an expiry beyond `MAX_EXPIRY_AHEAD_MS`
    /// - a never-credited declaration is over its client's share or the combined budget
    pub(super) async fn authorise_request(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
        session: bool,
        declaration: bool,
        peer: Option<IpAddr>,
    ) -> Result<Option<RequestAuth>, ApiError> {
        Span::current().record("id", display(account));
        let access = self.access_enforced();
        let rules = if access || declaration {
            match &self.auth {
                ClientAuth::Unsigned => {
                    ClientAuth::Eip712 { chain_id: self.operator.chain_id, vault: self.operator.vault }
                }
                signed => signed.clone(),
            }
        } else {
            self.auth.clone()
        };
        if rules == ClientAuth::Unsigned {
            // An unsigned request may act for any account it names, so the named one is counted.
            self.limits.check_account(account)?;
            return Ok(None);
        }
        // The digest is computed at most once, whichever path checks the signature.
        let mut hash = Some(hash);
        let mut digest = None;
        let mut digest = move |domain: &Eip712Domain, auth: &RequestAuth| -> B256 {
            *digest.get_or_insert_with(|| hash.take().expect("the digest is computed once")(domain, auth))
        };
        if let Err(reason) = rules.check(account, auth.as_ref(), &mut digest) {
            if declaration || !session || !matches!(reason, Reject::InvalidSignature) {
                debug!(?reason, "request refused");
                return Err(ApiError::Unauthorized(match reason {
                    Reject::MissingSignature => "missing signature",
                    _ => "invalid signature",
                }));
            }
            return self.authorise_session_key(account, auth, &mut digest, rules.domain()).await;
        }
        let auth = auth.expect("check refuses an unsigned request wherever signatures are required");
        // Accounts are never removed and nonces only grow, so neither check can pass a request
        // that the engine then sees ahead of its account's next nonce.
        let (credited, pending, declared, next, last_sequenced) = self
            .query(move |e| {
                let credited = e.accounts().contains_key(&account);
                let first = !credited && declaration;
                let pending = first && e.has_pending_deposit(account);
                let declared = first && e.eligibility(account).is_some();
                (credited, pending, declared, e.next_request_nonce(account), e.now())
            })
            .await?;
        if !credited && !pending {
            debug!(declaration, "request refused: account never credited");
            return Err(ApiError::Forbidden(if declaration {
                "a never-credited account declares only once a deposit for it is pending"
            } else {
                "account has never been credited"
            }));
        }
        self.check_fresh(&auth, next, last_sequenced)?;
        if !credited {
            // Only a declaration the engine could record: one not yet made, with an expiry it accepts.
            if declared {
                debug!("request refused: account has already declared");
                return Err(ApiError::Forbidden("account has already declared"));
            }
            if let Err(Reject::ExpiryTooFar { latest }) = auth.check_expiry((self.clock)().max(last_sequenced)) {
                debug!(latest, "request refused: expiry too far ahead");
                return Err(ApiError::BadRequest(format!("a first declaration must expire by {latest}")));
            }
            self.limits.check_initial_account(peer)?;
        }
        // Counted last, so only a request the node would sequence spends the account's rate. Every
        // signed request is in the public log, so anyone can replay one; a replay fails the nonce
        // or expiry check first and spends nothing.
        self.limits.check_account(account)?;
        Ok(Some(auth))
    }

    /// Admits a request `rules.check` refused as an invalid signature when the signature is a
    /// registered, live session key's: from `V41` a session key may act for its account through
    /// orders, cancels, and the requests that carry them. The account must be credited, the key
    /// registered for it and neither revoked nor expired at the sequenced time, and the request
    /// fresh on the key's own nonce sequence. The engine re-checks everything it sequences and
    /// refuses a session-key withdrawal as out of scope, in the log.
    /// Error cases:
    /// - the recovered signer is no session key of the account, or the account was never credited
    /// - the key is revoked or expired
    /// - a nonce other than the key's next, or a request expiry already past
    async fn authorise_session_key(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        digest: &mut impl FnMut(&Eip712Domain, &RequestAuth) -> B256,
        domain: Option<Eip712Domain>,
    ) -> Result<Option<RequestAuth>, ApiError> {
        let Some(domain) = domain else { return Err(ApiError::Unauthorized("invalid signature")) };
        let Some(auth) = auth else { return Err(ApiError::Unauthorized("missing signature")) };
        let Some(signer) = recover(&digest(&domain, &auth), &auth.signature) else {
            return Err(ApiError::Unauthorized("invalid signature"));
        };
        if signer == account {
            return Err(ApiError::Unauthorized("invalid signature"));
        }
        let (credited, key, next, last_sequenced) = self
            .query(move |e| {
                (
                    e.accounts().contains_key(&account),
                    e.session_key(account, signer),
                    e.next_request_nonce(signer),
                    e.now(),
                )
            })
            .await?;
        let Some(key) = key else { return Err(ApiError::Unauthorized("invalid signature")) };
        if !credited {
            debug!("request refused: account never credited");
            return Err(ApiError::Forbidden("account has never been credited"));
        }
        if key.revoked {
            debug!(session_key = %signer, "request refused: session key revoked");
            return Err(ApiError::Unauthorized("session key revoked"));
        }
        if last_sequenced > key.expires_at {
            debug!(session_key = %signer, expires_at = key.expires_at, "request refused: session key expired");
            return Err(ApiError::Unauthorized("session key expired"));
        }
        self.check_fresh(&auth, next, last_sequenced)?;
        self.limits.check_account(account)?;
        Ok(Some(auth))
    }

    /// Checks a signed request carries its signer's next nonce, `next`, and has not expired by the
    /// clock or by `last_sequenced`, the last sequenced time.
    /// Error cases:
    /// - a nonce other than `next`
    /// - an expiry already past
    pub(super) fn check_fresh(&self, auth: &RequestAuth, next: u64, last_sequenced: Millis) -> Result<(), ApiError> {
        if auth.nonce != next {
            debug!(nonce = auth.nonce, expected = next, "request refused: invalid nonce");
            return Err(ApiError::InvalidNonce { expected: next });
        }
        // The sequencer stamps no earlier than its clock reads now or the last sequenced time, so a
        // request already expired by then can never run, unless the host turns its clock back (the
        // sequencer's trusted-time TODO). An expiry too far ahead is left to the engine, which uses
        // the nonce as it refuses it, so the request cannot run later once its expiry comes within
        // the bound.
        let now = (self.clock)().max(last_sequenced);
        if let Err(Reject::Expired { expires_at }) = auth.check_expiry(now) {
            debug!(expires_at, "request refused: expired");
            return Err(ApiError::BadRequest(format!("request expired at {expires_at}")));
        }
        Ok(())
    }

    /// Sequences a command and waits for its events.
    /// Error cases:
    /// - the sequencer has stopped, or refused the command because sequencing is halted
    pub(super) async fn submit(&self, cmd: Command) -> Result<Vec<Event>, ApiError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(sequencer::Request::Submit { cmd, reply: Some(reply) })
            .await
            .map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }

    /// Runs a read on the sequencer between batches, so it sees a consistent, already-durable
    /// state.
    /// Error cases:
    /// - the sequencer has stopped
    pub(super) async fn query<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Engine) -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let (reply, rx) = oneshot::channel();
        let q = Box::new(move |e: &Engine| {
            let _ = reply.send(f(e));
        });
        self.tx.send(sequencer::Request::Query(q)).await.map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }
}
