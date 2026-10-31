//! Shared harness for the close-spread ceiling tests: the `V39` harnesses, the access-screened
//! client helpers, and the registry fixture builders.

use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus,
    Screening,
};
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Millis, Side};
use k256::ecdsa::SigningKey;

use crate::common::stress;
use crate::common::{AAPL, BTC, CLOSE_SPREAD_CEILINGS, Client, ETH, Harness, f, market, params, us_equities};

/// The notice a ceiling adoption waits out: seven days.
pub const NOTICE_MS: Millis = 7 * 24 * 60 * 60 * 1000;

/// A ceiling for each of the harness's genesis markets.
pub fn ceilings() -> Vec<CloseSpreadCeiling> {
    CLOSE_SPREAD_CEILINGS
        .iter()
        .map(|(market, bps)| CloseSpreadCeiling { market: *market, max_close_spread_bps: *bps })
        .collect()
}

/// `V8` with the operator recorded, the genesis markets relisted with a wide slippage bound, and
/// the ceiling registry installed: what the admin-only tests need.
pub fn harness() -> Harness {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    widen_slippage(&mut h);
    h.operator_admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() });
    h
}

/// `harness` with access rules on and `V39` active, ready for a client to declare, fund, and
/// trade through the screened order commands.
pub fn access_harness() -> (Harness, Client) {
    let mut h = harness();
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    (h, Client::new(1))
}

/// Activates `V39` over a `harness`.
pub fn into_v39(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
}

/// Relists the genesis markets with a 100% slippage bound, so a test can quote the mid while its
/// fills pay spreads far wider than the default fixture's 20bps.
pub fn widen_slippage(h: &mut Harness) {
    for (id, symbol, class, schedule, margin) in [
        (BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1000),
        (ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1000),
        (AAPL, "AAPL", AssetClass::Equity, us_equities(), 2000),
    ] {
        let mut spec = market(id, symbol, class, schedule, margin);
        spec.max_slippage_bps = 10_000;
        h.operator_admin(AdminCommand::UpsertMarket(spec));
    }
}

/// The shared access-source key the fixtures sign screening evidence with.
pub fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

pub fn source_key() -> engine_core::types::AccountId {
    eip712::address(source().verifying_key())
}

/// Fresh, allowed and clear evidence for `hash`, as the API's screening source answers.
pub fn evidence(
    h: &Harness,
    hash: eip712::B256,
    account: Option<engine_core::types::AccountId>,
    payer: Option<engine_core::types::AccountId>,
) -> AccessEvidence {
    let mut screening = Screening {
        source: source_key(),
        request_hash: hash,
        observed_at: h.ts,
        account,
        payer,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    screening.signature =
        eip712::sign(&source(), &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}

/// Records the client's professional declaration, as its signed request with evidence.
pub fn attest(h: &mut Harness, client: &Client) {
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash =
        eip712::eligibility_hash(&client.domain, client.account, EligibilityClass::Professional, nonce, expires_at);
    let request = EligibilityRequest {
        account: client.account,
        class: EligibilityClass::Professional,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) },
    };
    let ev = evidence(h, hash, Some(client.account), None);
    let events = h.cmd(Command::AttestEligibility { request, evidence: ev });
    assert_eq!(events, [Event::EligibilityAttested { account: client.account, class: EligibilityClass::Professional }]);
}

/// Funds the client through the screened deposit command, tracking the credit for
/// `Harness::assert_conserved`.
pub fn fund(h: &mut Harness, client: &Client, amount: &str) {
    let deposit = AccessDeposit {
        account: Some(client.account),
        payer: Some(client.account),
        amount: f(amount),
        chain_ref: format!("0xfund:{}", h.log.len()),
    };
    let ev = evidence(h, eip712::deposit_screening_hash(&deposit), Some(client.account), Some(client.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence: ev });
    assert_eq!(events, [Event::Deposited { account: client.account, amount: f(amount) }]);
    h.deposits += f(amount);
}

/// The client's signed, screened market order for `qty` at the house's slippage bound.
pub fn trade(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, quoted: &str) -> Vec<Event> {
    order(h, client, market, side, qty, quoted, false)
}

/// `trade` as a reduce-only order.
pub fn reduce(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, quoted: &str) -> Vec<Event> {
    order(h, client, market, side, qty, quoted, true)
}

pub fn order(
    h: &mut Harness,
    client: &Client,
    market: MarketId,
    side: Side,
    qty: &str,
    quoted: &str,
    reduce_only: bool,
) -> Vec<Event> {
    let mut request = OrderRequest {
        account: client.account,
        client_id: 7,
        market,
        side,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only,
        auth: None,
    };
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    let ev = evidence(h, hash, Some(client.account), None);
    h.cmd(Command::AccessOrder { request, evidence: ev })
}

/// Places a signed, screened resting order of `kind`.
pub fn rest(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, kind: OrderKind) -> Vec<Event> {
    let mut request = OrderRequest {
        account: client.account,
        client_id: 7,
        market,
        side,
        qty: f(qty),
        kind,
        reduce_only: false,
        auth: None,
    };
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    let ev = evidence(h, hash, Some(client.account), None);
    h.cmd(Command::AccessOrder { request, evidence: ev })
}

/// An access harness with its client declared and funded, ready to trade.
pub fn funded(amount: &str) -> (Harness, Client) {
    let (mut h, client) = access_harness();
    attest(&mut h, &client);
    fund(&mut h, &client, amount);
    (h, client)
}

/// `common::params` with absolute divergence triggers converted, as `V13` activation leaves them.
pub fn share_params() -> engine_core::params::ProtocolParams {
    let mut params = params();
    params.share_divergence_triggers(stress::MAX_DIVERGENCE_BPS);
    params
}

pub fn install(h: &mut Harness, ceilings: Vec<CloseSpreadCeiling>) -> Vec<Event> {
    // The node alone sequences the install; a log records it once.
    h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings }))
}

pub fn install_expect(h: &mut Harness, ceilings: Vec<CloseSpreadCeiling>) {
    let events = install(h, ceilings);
    assert!(events.contains(&Event::AdminApplied), "install rejected: {events:?}");
}

pub fn rejected(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::AdminRejected { reason }) => reason,
        other => panic!("expected an admin rejection, got {other:?}"),
    }
}

/// `base` with the entry for `market` replaced by one of `bps`.
pub fn with(base: &[CloseSpreadCeiling], market: MarketId, bps: i64) -> Vec<CloseSpreadCeiling> {
    let mut ceilings = base.to_vec();
    ceilings.retain(|ceiling| ceiling.market != market);
    ceilings.push(CloseSpreadCeiling { market, max_close_spread_bps: bps });
    ceilings
}

/// `base` with a second entry for `market`.
pub fn duplicating(base: &[CloseSpreadCeiling], market: MarketId, bps: i64) -> Vec<CloseSpreadCeiling> {
    let mut ceilings = base.to_vec();
    ceilings.push(CloseSpreadCeiling { market, max_close_spread_bps: bps });
    ceilings
}
