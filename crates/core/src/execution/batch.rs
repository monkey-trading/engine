//! Batch closes, from `V44`: close whole positions on one signed request (cancel-all is in
//! `cancel`).
//!
//! Each close is the single reduce-only market order of the account's whole position in its
//! market, run through the same validation and execution as one sent alone, so spreads, the
//! close-spread ceiling, the profit fee, the emissions weight, stress and the house caps are that
//! order's. The entries run in list order, each seeing the state the one before left, and a
//! refused entry does not stop the rest. Authorisation and access screening run once, for the
//! whole request, which uses one nonce.

use std::collections::BTreeSet;

use crate::Fixed;
use crate::access::AccessEvidence;
use crate::auth::RequestScope;
use crate::command::{CloseAllRequest, CloseEntry, OrderKind, OrderRequest};
use crate::eip712::close_all_hash;
use crate::engine::Engine;
use crate::event::{Event, Reject};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Side};

impl Engine {
    /// Authorises and screens the request once, then closes the account's position in each named
    /// market as a reduce-only market order, in list order. An entry that cannot fill emits the
    /// rejection a single order would, with its list index as the `client_id`, and the others
    /// still run.
    /// Error cases:
    /// - before `V44`
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`), or access
    ///   screening refuses the account
    /// - no entries, more entries than listed markets, or a market named twice
    pub(crate) fn close_all(&mut self, req: &CloseAllRequest, evidence: &AccessEvidence, events: &mut Vec<Event>) {
        let account = req.account;
        let reject = |reason, events: &mut Vec<Event>| {
            events.push(Event::OrderRejected { account, client_id: 0, reason });
        };
        if self.protocol_version < ProtocolVersion::V44 {
            return reject(Reject::BatchUnsupported, events);
        }
        let authorised = self.authorise(account, req.auth.as_ref(), RequestScope::Trade, |domain, auth| {
            close_all_hash(domain, req, auth.nonce, auth.expires_at)
        });
        let request_hash = self
            .access_domain()
            .zip(req.auth.as_ref())
            .map(|(domain, auth)| close_all_hash(&domain, req, auth.nonce, auth.expires_at))
            .unwrap_or_default();
        if let Err(reason) = authorised
            .and_then(|()| {
                self.check_access(Some(account), None, request_hash, Some(evidence), false, true)
                    .map_err(|reason| Reject::Access { reason })
            })
            .and_then(|()| self.check_closes(&req.closes))
        {
            return reject(reason, events);
        }
        for (index, entry) in req.closes.iter().enumerate() {
            let order = self.closing_order(account, index, entry, req.max_slippage_bps);
            match self.validate_order(&order) {
                Ok(()) => self.place_validated(&order, events),
                Err(reason) => events.push(Event::OrderRejected { account, client_id: order.client_id, reason }),
            }
        }
    }

    /// Checks the list names at least one market, each at most once, and no more than are listed.
    /// Error cases:
    /// - no entries
    /// - more entries than listed markets
    /// - a market named twice
    fn check_closes(&self, closes: &[CloseEntry]) -> Result<(), Reject> {
        let invalid = |reason: &str| Err(Reject::InvalidOrder { reason: reason.into() });
        if closes.is_empty() {
            return invalid("no markets to close");
        }
        if closes.len() > self.markets.len() {
            return invalid("more closes than listed markets");
        }
        let mut named = BTreeSet::new();
        if closes.iter().any(|entry| !named.insert(entry.market)) {
            return invalid("market named twice");
        }
        Ok(())
    }

    /// The reduce-only market order that closes the account's whole position in `entry`'s market
    /// now. With no position it is a sell of the smallest quantity, which a single order would
    /// refuse as reduce-only with nothing to reduce, or for an earlier reason.
    fn closing_order(
        &self,
        account: AccountId,
        index: usize,
        entry: &CloseEntry,
        max_slippage_bps: Option<i64>,
    ) -> OrderRequest {
        let held = self.accounts.get(&account).map_or(Fixed::ZERO, |acct| acct.position(entry.market).qty);
        let (side, qty) = if held.is_zero() {
            (Side::Sell, Fixed::from_raw(1))
        } else if held.is_positive() {
            (Side::Sell, held)
        } else {
            (Side::Buy, held.abs())
        };
        OrderRequest {
            account,
            // Entry counts are bounded by the listed markets, so the index fits.
            client_id: index as u64,
            market: entry.market,
            side,
            qty,
            kind: OrderKind::Market { quoted: entry.quoted, max_slippage_bps },
            reduce_only: true,
            auth: None,
        }
    }
}
