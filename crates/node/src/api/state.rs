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

/// How the API admits a signed request from an account the engine has never credited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BeforeCredit {
    /// Refused: the request can do nothing until the account holds funds.
    Refused,
    /// A declaration, admitted while the account has not declared.
    Declaration,
    /// A session-key registration or revocation, admitted once the account has declared: the
    /// declaration is where the account and its client are screened, so these cannot skip it.
    SessionKeys,
}

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
    /// - the account, or its session keys, are over their rate
    /// - the sequencer has stopped
    pub(super) async fn authorise(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        session: bool,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
    ) -> Result<Option<RequestAuth>, ApiError> {
        self.authorise_request(account, auth, hash, session, BeforeCredit::Refused, None).await
    }

    /// `session` decides whether a registered session key's signature may stand in for the
    /// account's: true for the requests a session key may make (orders, cancels, and the
    /// withdrawals the engine refuses in the log as out of scope), false for key management,
    /// which is the account key's alone at the API too, so a session key's signature on a
    /// registration or revocation never reaches the log.
    /// `authorise`, except for what `before_credit` lets a never-credited account do: declare, and
    /// once it has declared, manage its session keys. Those are the only requests such an account can
    /// put in the log, and each is admitted only with an expiry the engine accepts. The engine can
    /// still refuse one, using its nonce, so the account may try again; each attempt is counted
    /// against the account's rate, the share of fresh-account admissions its client `peer` may take,
    /// and all clients' combined budget. A declaration from an account with a pending deposit, which
    /// has paid, takes neither, and is held to the share's rate for the account alone.
    /// Error cases:
    /// - as `authorise`, except that `before_credit` may admit a never-credited account
    /// - a never-credited account declares again, manages session keys before declaring, or signs an
    ///   expiry beyond `MAX_EXPIRY_AHEAD_MS`
    /// - a never-credited account's admitted request is over its client's share or the combined budget,
    ///   or a declaration with a pending deposit is over the share's rate for its account or client,
    ///   or the budget all paid declarations share: `SetupLimited`
    pub(super) async fn authorise_request(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
        session: bool,
        before_credit: BeforeCredit,
        peer: Option<IpAddr>,
    ) -> Result<Option<RequestAuth>, ApiError> {
        let mut seen = None;
        let admitted = self.admit(account, auth, hash, session, before_credit, peer, &mut seen).await;
        // A refusal states what the engine had taken in, a nonce a crash could still take back.
        if admitted.is_err() {
            self.durable(seen).await?;
        }
        admitted
    }

    /// `authorise_request`, noting in `seen` the last seq of the state its refusals read.
    /// Error cases:
    /// - as `authorise_request`
    #[allow(clippy::too_many_arguments)]
    async fn admit(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
        session: bool,
        before_credit: BeforeCredit,
        peer: Option<IpAddr>,
        seen: &mut Option<u64>,
    ) -> Result<Option<RequestAuth>, ApiError> {
        Span::current().record("id", display(account));
        let declaration = before_credit == BeforeCredit::Declaration;
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
            return self.authorise_session_key(account, auth, &mut digest, rules.domain(), seen).await;
        }
        let auth = auth.expect("check refuses an unsigned request wherever signatures are required");
        // Accounts and declarations are never removed and nonces only grow, so none of these checks
        // can pass a request that the engine then sees ahead of its account's next nonce.
        let (credited, declared, paid, next, last_sequenced, last_seq) = self
            .query(move |e| {
                let credited = e.accounts().contains_key(&account);
                let declared = !credited && e.eligibility(account).is_some();
                let paid = declaration && !credited && !declared && e.has_pending_deposit(account);
                (credited, declared, paid, e.next_request_nonce(account), e.now(), e.last_seq())
            })
            .await?;
        *seen = last_seq;
        if !credited {
            let refusal = match before_credit {
                BeforeCredit::Refused => Some("account has never been credited"),
                BeforeCredit::SessionKeys if !declared => Some("a never-credited account must declare first"),
                BeforeCredit::SessionKeys | BeforeCredit::Declaration => None,
            };
            if let Some(refusal) = refusal {
                debug!(?before_credit, "request refused: {refusal}");
                return Err(ApiError::Forbidden(refusal));
            }
        }
        self.check_fresh(&auth, next, last_sequenced)?;
        if !credited {
            // Only what the engine could act on: not a second declaration, nor an expiry it refuses.
            if declaration && declared {
                debug!("request refused: account has already declared");
                return Err(ApiError::Forbidden("account has already declared"));
            }
            if let Err(Reject::ExpiryTooFar { latest }) = auth.check_expiry(self.clock.now_ms().max(last_sequenced)) {
                debug!(latest, "request refused: expiry too far ahead");
                return Err(ApiError::BadRequest(format!(
                    "a never-credited account's request must expire by {latest}"
                )));
            }
            if paid {
                // A pending deposit is a vault deposit (10 USDC at least) the custody quorum observed and the
                // log holds, so the wallet has paid. It stays exempt until a declaration is recorded, so its own
                // bucket, its client's and the paid budget bound the declarations the engine refuses.
                self.limits.check_paid_declaration(account, peer)?;
            } else {
                self.limits.check_initial_account(peer)?;
            }
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
    /// - the account's session keys are over their rate
    async fn authorise_session_key(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        digest: &mut impl FnMut(&Eip712Domain, &RequestAuth) -> B256,
        domain: Option<Eip712Domain>,
        seen: &mut Option<u64>,
    ) -> Result<Option<RequestAuth>, ApiError> {
        let Some(domain) = domain else { return Err(ApiError::Unauthorized("invalid signature")) };
        let Some(auth) = auth else { return Err(ApiError::Unauthorized("missing signature")) };
        let Some(signer) = recover(&digest(&domain, &auth), &auth.signature) else {
            return Err(ApiError::Unauthorized("invalid signature"));
        };
        if signer == account {
            return Err(ApiError::Unauthorized("invalid signature"));
        }
        let (credited, key, next, last_sequenced, last_seq) = self
            .query(move |e| {
                (
                    e.accounts().contains_key(&account),
                    e.session_key(account, signer),
                    e.next_request_nonce(signer),
                    e.now(),
                    e.last_seq(),
                )
            })
            .await?;
        *seen = last_seq;
        let Some(key) = key else { return Err(ApiError::Unauthorized("invalid signature")) };
        if !credited {
            debug!("request refused: account never credited");
            return Err(ApiError::Forbidden("account has never been credited"));
        }
        if key.revoked {
            debug!(session_key = %signer, "request refused: session key revoked");
            return Err(ApiError::Unauthorized("session key revoked"));
        }
        // The clock may run ahead of the sequenced time between commands; a key dead by either
        // is dead, exactly as `check_fresh` reads request expiry.
        if self.clock.now_ms().max(last_sequenced) > key.expires_at {
            debug!(session_key = %signer, expires_at = key.expires_at, "request refused: session key expired");
            return Err(ApiError::Unauthorized("session key expired"));
        }
        self.check_fresh(&auth, next, last_sequenced)?;
        // Counted against the account's session bucket, not the account's own: all of an
        // account's keys share one allowance at the per-account rate, so keys cannot multiply
        // it, and a flooding key cannot starve the wallet's requests, its revocation above all.
        self.limits.check_session_account(account)?;
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
        let now = self.clock.now_ms().max(last_sequenced);
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

    /// Runs a read on the sequencer right after the batch it arrives in is written, so it sees the
    /// state every command sequenced so far leaves, whether or not that batch has reached storage
    /// yet. For a decision about the next request; a view a client reads is `read`.
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

    /// Waits until the log holds every command up to `seq`: what a client is shown must not be
    /// something a crash could take back. A manifest that has published no log head, as on a fresh
    /// node before its first batch is synced, has only genesis to show and nothing to wait for.
    /// Error cases:
    /// - the log writer has stopped
    pub(super) async fn durable(&self, seq: Option<u64>) -> Result<(), ApiError> {
        let Some(seq) = seq else { return Ok(()) };
        let mut manifest = self.manifest.clone();
        manifest
            .wait_for(|log| log.flushed.is_none_or(|flushed| flushed.seq >= seq))
            .await
            .map(drop)
            .map_err(|_| ApiError::Unavailable)
    }

    /// `query`, with the answer held until the state it was read from is durable: for a view a
    /// client is shown.
    /// Error cases:
    /// - the sequencer has stopped
    pub(super) async fn query_durable<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Engine) -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let (view, seq) = self.query(move |e| (f(e), e.last_seq())).await?;
        self.durable(seq).await?;
        Ok(view)
    }

    /// Runs a view on the stats worker's copy of the engine, off the sequencer's thread. The copy
    /// holds the commands of every batch that is durable and none that is not, and the read queues
    /// behind them, so it sees each command whose reply its caller has had. With no worker, or one
    /// whose queue is full, the sequencer runs it right after the batch it arrives in is written. Either
    /// way the answer waits for the log to hold every command the engine it read had taken in. A view
    /// of the dashboard record, which runs a batch ahead of the copy, is `query_durable`.
    /// Error cases:
    /// - the sequencer has stopped
    pub(super) async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Engine) -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let (reply, rx) = oneshot::channel();
        let view: sequencer::Query = Box::new(move |e: &Engine| {
            let _ = reply.send((f(e), e.last_seq()));
        });
        let refused = match self.stats.reader() {
            Some(reader) => reader.read(view).err(),
            None => Some(view),
        };
        if let Some(view) = refused {
            self.tx.send(sequencer::Request::Query(view)).await.map_err(|_| ApiError::Unavailable)?;
        }
        let (view, seq) = rx.await.map_err(|_| ApiError::Unavailable)?;
        self.durable(seq).await?;
        Ok(view)
    }
}

#[cfg(test)]
mod durable_tests;
