//! Batch-order test harness: the access-screened `V44` setup and the signed, screened batch
//! commands the protocol tests sequence.

use engine_core::Engine;
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, CloseAllRequest, CloseEntry, Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis, Side};

use crate::common::close_spread::{attest, evidence, fund};
use crate::common::session_keys::funded_at;
use crate::common::{Client, Harness, f};

// ── Helpers ──

/// The access-screened `V44` harness: client 1 declared and holding 100000, BTC priced at 100000,
/// and a second client, seed 7, for a session key.
pub fn funded() -> (Harness, Client, Client) {
    funded_at(ProtocolVersion::V44)
}

/// `funded`, with MONKEY emissions initialised from now, so fills accumulate period results
/// (gross losses from `V43`).
pub fn funded_emitting() -> (Harness, Client, Client) {
    let (mut h, client, key) = funded();
    let schedule = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(1_000_000_000_000_000_000_000) };
    h.operator_admin(AdminCommand::InitEmissions(schedule));
    (h, client, key)
}

/// `client` declared and funded with `amount`.
pub fn onboard(h: &mut Harness, client: &Client, amount: &str) {
    attest(h, client);
    fund(h, client, amount);
}

/// A signed cancel-all for `account`, signed by `signer` with `nonce`.
pub fn cancel_all(signer: &Client, account: AccountId, nonce: u64) -> Command {
    let hash = eip712::cancel_all_hash(&signer.domain, account, nonce, signer.expires_at);
    let auth = RequestAuth { nonce, expires_at: signer.expires_at, signature: eip712::sign(&signer.key, &hash) };
    Command::CancelAll { account, auth: Some(auth) }
}

/// The entries closing `markets` at `quoted` each.
pub fn closes(markets: &[MarketId], quoted: &str) -> Vec<CloseEntry> {
    markets.iter().map(|&market| CloseEntry { market, quoted: f(quoted) }).collect()
}

/// A signed, screened close-all for `account` of `closes`, signed by `signer` with `nonce` and its
/// request expiry.
pub fn close_all(
    h: &Harness,
    signer: &Client,
    account: AccountId,
    nonce: u64,
    closes: Vec<CloseEntry>,
    max_slippage_bps: Option<i64>,
) -> Command {
    close_all_expiring(h, signer, account, nonce, closes, max_slippage_bps, signer.expires_at)
}

/// `close_all` with an explicit request expiry.
pub fn close_all_expiring(
    h: &Harness,
    signer: &Client,
    account: AccountId,
    nonce: u64,
    closes: Vec<CloseEntry>,
    max_slippage_bps: Option<i64>,
    expires_at: Millis,
) -> Command {
    let mut request = CloseAllRequest { account, closes, max_slippage_bps, auth: None };
    let hash = eip712::close_all_hash(&signer.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&signer.key, &hash) });
    let ev = evidence(h, hash, Some(account), None);
    Command::AccessCloseAll { request, evidence: ev }
}

/// A signed, screened market order for `client` of `qty` in `market`, quoted at `quoted`.
pub fn market_order(
    h: &mut Harness,
    client: &Client,
    market: MarketId,
    side: Side,
    qty: &str,
    quoted: &str,
) -> Vec<Event> {
    crate::common::close_spread::order(h, client, market, side, qty, quoted, false)
}

/// The same close one at a time: the signed, screened reduce-only market order for each entry's
/// whole position, as `engine`'s state holds it now, with the entry's index as its client id.
pub fn singles(
    h: &mut Harness,
    client: &Client,
    closes: &[CloseEntry],
    max_slippage_bps: Option<i64>,
) -> Vec<Vec<Event>> {
    closes
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let held = h.engine.accounts()[&client.account].position(entry.market).qty;
            let (side, qty) = if held.is_zero() {
                (Side::Sell, engine_core::Fixed::from_raw(1))
            } else if held.is_positive() {
                (Side::Sell, held)
            } else {
                (Side::Buy, held.abs())
            };
            let mut request = OrderRequest {
                account: client.account,
                client_id: index as u64,
                market: entry.market,
                side,
                qty,
                kind: OrderKind::Market { quoted: entry.quoted, max_slippage_bps },
                reduce_only: true,
                auth: None,
            };
            let nonce = h.engine.next_request_nonce(client.account);
            let expires_at = h.ts + 60_000;
            let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
            request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
            let ev = evidence(h, hash, Some(client.account), None);
            h.cmd(Command::AccessOrder { request, evidence: ev })
        })
        .collect()
}

/// A fresh engine that has replayed `h`'s log.
pub fn replayed(h: &Harness) -> Engine {
    let mut engine = Engine::new();
    for sequenced in &h.log {
        engine.apply(sequenced).expect("the log replays");
    }
    engine
}
