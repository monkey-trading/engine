//! Cancelling resting orders: one on a client's request, or from `V44` all of an account's on one.

use crate::account::RestingOrder;
use crate::auth::{RequestAuth, RequestScope};
use crate::eip712::{cancel_all_hash, cancel_hash};
use crate::engine::Engine;
use crate::event::{CancelReason, Event, Reject};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, OrderId};

impl Engine {
    /// Authorises the request, then cancels a resting order on the client's request.
    pub(crate) fn cancel_order(
        &mut self,
        account: AccountId,
        order_id: OrderId,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        if let Err(reason) = self.authorise(account, auth, RequestScope::Trade, |domain, auth| {
            cancel_hash(domain, account, order_id, auth.nonce, auth.expires_at)
        }) {
            events.push(Event::OrderRejected { account, client_id: 0, reason });
        } else if self.remove_order(account, order_id).is_some() {
            events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Client });
        } else {
            events.push(Event::OrderRejected { account, client_id: 0, reason: Reject::UnknownOrder });
        }
    }

    /// Authorises the request, then cancels every resting order the account holds, in order id
    /// order, each with the event a single cancel emits. An account with none emits nothing.
    /// Error cases:
    /// - before `V44`
    /// - the signature, nonce, or request expiry fails (see `Engine::authorise`)
    pub(crate) fn cancel_all(&mut self, account: AccountId, auth: Option<&RequestAuth>, events: &mut Vec<Event>) {
        let reject = |reason, events: &mut Vec<Event>| {
            events.push(Event::OrderRejected { account, client_id: 0, reason });
        };
        if self.protocol_version < ProtocolVersion::V44 {
            return reject(Reject::BatchUnsupported, events);
        }
        if let Err(reason) = self.authorise(account, auth, RequestScope::Trade, |domain, auth| {
            cancel_all_hash(domain, account, auth.nonce, auth.expires_at)
        }) {
            return reject(reason, events);
        }
        let held: Vec<OrderId> =
            self.accounts.get(&account).map(|acct| acct.orders.keys().copied().collect()).unwrap_or_default();
        for order_id in held {
            self.remove_order(account, order_id);
            events.push(Event::OrderCancelled { account, order_id, reason: CancelReason::Client });
        }
    }

    /// Removes a resting order from its account and its market's book.
    pub(crate) fn remove_order(&mut self, account: AccountId, order_id: OrderId) -> Option<RestingOrder> {
        let order = self.accounts.get_mut(&account)?.orders.remove(&order_id)?;
        self.index_order(account, &order, false);
        if let Some(market) = self.markets.get_mut(&order.market) {
            market.resting.remove(&order_id);
        }
        self.record_order(account, order_id);
        Some(order)
    }
}
