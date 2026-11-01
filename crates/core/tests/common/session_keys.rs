//! Session-key test harness: the access-screened `V41` setup, registrations, and screened
//! session-key orders the protocol tests sequence.

use engine_core::access::AccessPolicy;
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis, Side};

use crate::common::close_spread::{attest, ceilings, evidence, fund, source_key};
use crate::common::{BTC, CHAIN_ID, Client, Harness, VAULT, f};

pub const HOUR_MS: Millis = 60 * 60 * 1000;

// ── Helpers ──

/// The `V41` harness under access rules: signed requests, the operator recorded, the ceiling
/// registry the `V40` activation check wants, a screening source whose evidence stays fresh across
/// an hour of sequenced time, client 1 declared and holding 100000, and BTC priced at 100000.
pub fn funded() -> (Harness, Client, Client) {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() }));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V41 });
    let client = Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client, "100000");
    h.price(BTC, "100000");
    (h, client, Client::new(7))
}

/// `client`'s signed registration of `key` until `session_expires_at`, using the account's next
/// nonce and the client's request expiry.
pub fn register(h: &Harness, client: &Client, key: &Client, session_expires_at: Millis) -> Command {
    register_expiring(h, client, key, session_expires_at, client.expires_at)
}

/// `register` with an explicit request expiry, for registrations sequenced after the client's
/// fixed request expiry has passed.
pub fn register_expiring(
    h: &Harness,
    client: &Client,
    key: &Client,
    session_expires_at: Millis,
    expires_at: Millis,
) -> Command {
    let nonce = h.engine.next_request_nonce(client.account);
    let hash = eip712::session_key_registration_hash(
        &client.domain,
        client.account,
        key.account,
        session_expires_at,
        nonce,
        expires_at,
    );
    Command::RegisterSessionKey {
        account: client.account,
        session_key: key.account,
        expires_at: session_expires_at,
        auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) }),
    }
}

/// A 0.5 BTC market buy for `account`, quoted at 100000, unsigned.
pub fn buy(account: AccountId) -> OrderRequest {
    OrderRequest {
        account,
        client_id: 7,
        market: BTC,
        side: Side::Buy,
        qty: f("0.5"),
        kind: OrderKind::Market { quoted: f("100000"), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    }
}

/// A resting 0.4 BTC limit buy at 90000 for `account`, unsigned.
pub fn limit(account: AccountId) -> OrderRequest {
    OrderRequest { kind: OrderKind::Limit { price: f("90000") }, qty: f("0.4"), ..buy(account) }
}

/// `req` signed by `key` for `account` with `nonce` and the key client's request expiry, screened
/// like the account's own orders, and sequenced.
pub fn trade(h: &mut Harness, key: &Client, nonce: u64, req: OrderRequest) -> Vec<Event> {
    h.cmd(trade_cmd(h, key, nonce, req))
}

/// `trade` with a request expiry of `expires_at` instead of the key client's.
pub fn expiring_trade(h: &mut Harness, key: &Client, nonce: u64, req: OrderRequest, expires_at: Millis) -> Vec<Event> {
    h.cmd(trade_cmd_expiring(h, key, nonce, req, expires_at))
}

/// The screened `AccessOrder` `trade` sequences.
pub fn trade_cmd(h: &Harness, key: &Client, nonce: u64, req: OrderRequest) -> Command {
    trade_cmd_expiring(h, key, nonce, req, key.expires_at)
}

/// `trade_cmd` with an explicit request expiry.
pub fn trade_cmd_expiring(h: &Harness, key: &Client, nonce: u64, mut req: OrderRequest, expires_at: Millis) -> Command {
    let hash = eip712::order_hash(&key.domain, &req, nonce, expires_at);
    req.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&key.key, &hash) });
    let hash = eip712::order_hash(&key.domain, &req, nonce, expires_at);
    let ev = evidence(h, hash, Some(req.account), None);
    Command::AccessOrder { request: req, evidence: ev }
}

/// `req` re-addressed to `account` and signed by `key` with `nonce` and request expiry
/// `expires_at`, screened like the account's own orders, and sequenced.
pub fn expiring_trade_for(
    h: &mut Harness,
    key: &Client,
    account: AccountId,
    nonce: u64,
    mut req: OrderRequest,
    expires_at: Millis,
) -> Vec<Event> {
    req.account = account;
    let hash = eip712::order_hash(&key.domain, &req, nonce, expires_at);
    req.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&key.key, &hash) });
    let hash = eip712::order_hash(&key.domain, &req, nonce, expires_at);
    let ev = evidence(h, hash, Some(req.account), None);
    h.cmd(Command::AccessOrder { request: req, evidence: ev })
}
