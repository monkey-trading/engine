//! Close-spread ceilings under `V39`: the registry's adoption, the governance path, the
//! pricing-site clamp and reversal refusal, and the listing guards. Ceilings are unequal test
//! values at or above the harness launch close charge (8bps: a 5bps stress minimum plus the 3bps
//! top toxicity add-on).

use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus,
    Screening,
};
use engine_core::auth::ClientAuth;
use engine_core::auth::RequestAuth;
use engine_core::close_spread::{CloseSpreadCeiling, MAX_CLOSE_SPREAD_BPS};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, Signature};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::market::QuoteCurrency;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Millis, Side};
use engine_core::Fixed;
use k256::ecdsa::SigningKey;

mod common;
use common::events::only_fill;
use common::{AAPL, BTC, CLOSE_SPREAD_CEILINGS, ETH, Client, Harness, acct, f, market};

/// The notice a ceiling adoption waits out: seven days.
const NOTICE_MS: Millis = 7 * 24 * 60 * 60 * 1000;

/// A ceiling for each of the harness's genesis markets.
fn ceilings() -> Vec<CloseSpreadCeiling> {
    CLOSE_SPREAD_CEILINGS
        .iter()
        .map(|(market, bps)| CloseSpreadCeiling { market: *market, max_close_spread_bps: *bps })
        .collect()
}

/// `V8` with the operator recorded, the genesis markets relisted with a wide slippage bound, and
/// the ceiling registry installed: what the admin-only tests need.
fn harness() -> Harness {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    widen_slippage(&mut h);
    h.operator_admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() });
    h
}

/// `harness` with access rules on and `V39` active, ready for a client to declare, fund, and
/// trade through the screened order commands.
fn access_harness() -> (Harness, Client) {
    let mut h = harness();
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    (h, Client::new(1))
}

/// Activates `V39` over a `harness`.
fn into_v39(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
}

/// Relists the genesis markets with a 100% slippage bound, so a test can quote the mid while its
/// fills pay spreads far wider than the default fixture's 20bps.
fn widen_slippage(h: &mut Harness) {
    for (id, symbol, class, schedule, margin) in [
        (BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1000),
        (ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1000),
        (AAPL, "AAPL", AssetClass::Equity, common::us_equities(), 2000),
    ] {
        let mut spec = market(id, symbol, class, schedule, margin);
        spec.max_slippage_bps = 10_000;
        h.operator_admin(AdminCommand::UpsertMarket(spec));
    }
}

/// The shared access-source key the fixtures sign screening evidence with.
fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

fn source_key() -> engine_core::types::AccountId {
    eip712::address(source().verifying_key())
}

/// Fresh, allowed and clear evidence for `hash`, as the API's screening source answers.
fn evidence(
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
    screening.signature = eip712::sign(&source(), &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}

/// Records the client's professional declaration, as its signed request with evidence.
fn attest(h: &mut Harness, client: &Client) {
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::eligibility_hash(&client.domain, client.account, EligibilityClass::Professional, nonce, expires_at);
    let request =
        EligibilityRequest { account: client.account, class: EligibilityClass::Professional, auth: RequestAuth {
            nonce,
            expires_at,
            signature: eip712::sign(&client.key, &hash),
        } };
    let ev = evidence(h, hash, Some(client.account), None);
    let events = h.cmd(Command::AttestEligibility { request, evidence: ev });
    assert_eq!(
        events,
        [Event::EligibilityAttested { account: client.account, class: EligibilityClass::Professional }]
    );
}

/// Funds the client through the screened deposit command, tracking the credit for
/// `Harness::assert_conserved`.
fn fund(h: &mut Harness, client: &Client, amount: &str) {
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
fn trade(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, quoted: &str) -> Vec<Event> {
    order(h, client, market, side, qty, quoted, false)
}

/// `trade` as a reduce-only order.
fn reduce(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, quoted: &str) -> Vec<Event> {
    order(h, client, market, side, qty, quoted, true)
}

fn order(
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
fn rest(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, kind: OrderKind) -> Vec<Event> {
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
fn funded(amount: &str) -> (Harness, Client) {
    let (mut h, client) = access_harness();
    attest(&mut h, &client);
    fund(&mut h, &client, amount);
    (h, client)
}

/// `common::params` with absolute divergence triggers converted, as `V13` activation leaves them.
fn share_params() -> engine_core::params::ProtocolParams {
    let mut params = common::params();
    params.share_divergence_triggers(common::stress::MAX_DIVERGENCE_BPS);
    params
}

fn install(h: &mut Harness, ceilings: Vec<CloseSpreadCeiling>) -> Vec<Event> {
    h.operate(AdminCommand::SetCloseSpreadCeilings { ceilings })
}

fn install_expect(h: &mut Harness, ceilings: Vec<CloseSpreadCeiling>) {
    let events = install(h, ceilings);
    assert!(events.contains(&Event::AdminApplied), "install rejected: {events:?}");
}

fn rejected(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::AdminRejected { reason }) => reason,
        other => panic!("expected an admin rejection, got {other:?}"),
    }
}

/// `base` with the entry for `market` replaced by one of `bps`.
fn with(base: &[CloseSpreadCeiling], market: MarketId, bps: i64) -> Vec<CloseSpreadCeiling> {
    let mut ceilings = base.to_vec();
    ceilings.retain(|ceiling| ceiling.market != market);
    ceilings.push(CloseSpreadCeiling { market, max_close_spread_bps: bps });
    ceilings
}

/// `base` with a second entry for `market`.
fn duplicating(base: &[CloseSpreadCeiling], market: MarketId, bps: i64) -> Vec<CloseSpreadCeiling> {
    let mut ceilings = base.to_vec();
    ceilings.push(CloseSpreadCeiling { market, max_close_spread_bps: bps });
    ceilings
}

#[test]
fn install_adopts_every_listed_market_once_and_refuses_a_flawed_registry() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let ok = ceilings();
    // A listed market with no entry, an entry for an unlisted market, a duplicate, an out-of-range
    // value, and a nonzero value below the launch close charge: each refuses, the registry stays
    // uninstalled.
    let cases: Vec<(Vec<CloseSpreadCeiling>, &str)> = vec![
        (ok[..ok.len() - 1].to_vec(), "listed without"),
        (with(&ok, 77, 20), "not listed"),
        (duplicating(&ok, ETH, 25), "twice"),
        (with(&ok, BTC, MAX_CLOSE_SPREAD_BPS + 1), "0..=9999"),
        (with(&ok, BTC, 7), "largest close charge"),
    ];
    for (ceilings, why) in cases {
        let events = install(&mut h, ceilings);
        assert!(rejected(&events).contains(why), "{why}: {events:?}");
        assert!(h.engine.close_spread_ceilings().is_none());
    }
    // Zero is a legal ceiling: free closes.
    install_expect(&mut h, with(&ok, BTC, 0));
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&BTC], 0);
    // A log records the registry once.
    let events = install(&mut h, ok);
    assert!(rejected(&events).contains("already installed"));
}

#[test]
fn the_registry_binds_from_v39_and_a_close_clamps_to_its_ceiling() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    install_expect(&mut h, ceilings());
    widen_slippage(&mut h);
    let client = Client::new(1);
    h.deposit(client.account, "1000000");
    h.price(BTC, "100000");
    // Pre-V39, a close pays the stress minimum, whatever the ceiling waiting for it.
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    let events = h.market_order(client.account, BTC, Side::Buy, "1", "100000");
    only_fill(&events);
    let events = h.market_order(client.account, BTC, Side::Sell, "1", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 5, "pre-V39 the close pays the stress minimum: {events:?}");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });

    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    into_v39(&mut h);
    attest(&mut h, &client);
    // Raise the stress minimum above BTC's 20bps ceiling: the open pays it, and the close clamps.
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "100000");
    let open = only_fill(&events);
    assert_eq!(open.spread_bps, 50, "an open pays what it prices: {events:?}");
    let events = trade(&mut h, &client, BTC, Side::Sell, "1", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 20, "V39 clamps the close to the ceiling: {events:?}");
    assert_eq!(fill.price, f("99800"), "{fill:?}");
    h.assert_conserved(f("0"));
}

#[test]
fn activating_v39_refuses_a_listed_market_with_no_ceiling() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 }));
    assert!(rejected(&events).contains("without a close-spread ceiling"), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V39);
}

#[test]
fn the_clamp_floors_the_adjustment_to_the_exact_bound() {
    let (mut h, client) = funded("1000000");
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    // A mid whose 20bps bound is fractional in raw units: the charged adjustment is the floor of
    // `index_raw * ceiling / 10_000`, never the unit-above value upward rounding gives the charge.
    let mid = f("0.654321987");
    h.price(BTC, "0.654321987");
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "0.654321987");
    only_fill(&events);
    let events = trade(&mut h, &client, BTC, Side::Sell, "1", "0.654321987");
    let fill = only_fill(&events);
    let bound = Fixed::from_raw(mid.raw() * 20 / 10_000);
    assert_eq!(fill.index, mid);
    assert_eq!(fill.price, mid - bound, "{fill:?}");
    // The exact bound holds: the adjustment never exceeds `index * ceiling / 10_000`.
    assert!((fill.index - fill.price).raw() * 10_000 <= mid.raw() * 20, "{fill:?}");
}

#[test]
fn a_reversal_over_the_ceiling_is_refused_and_the_pure_close_stays_open() {
    let (mut h, client) = funded("10000000");
    h.price(BTC, "100000");
    // A 100-unit long (10m notional) pays the top size tier's 5bps. Selling 150 reverses through
    // it: the blended charge is the opening slice's size cost per unit — 800 USD over 150 units,
    // 0.53bps of a 100000 mid, its truncated `spread_bps` 0 — well under the 20bps ceiling, so the
    // reversal passes and pays what it priced.
    let events = trade(&mut h, &client, BTC, Side::Buy, "100", "100000");
    only_fill(&events);
    let events = trade(&mut h, &client, BTC, Side::Sell, "150", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.position, f("-50"));
    assert_eq!(fill.price, f("99994.666666666"), "{fill:?}");
    let events = trade(&mut h, &client, BTC, Side::Buy, "200", "100000");
    only_fill(&events);
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, f("150"));
    // A 50bps stress minimum puts every close charge above the ceiling: the reversal is refused
    // and the position is untouched, and the reduce-only close still fills at the ceiling.
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    let events = trade(&mut h, &client, BTC, Side::Sell, "400", "100000");
    assert!(matches!(
        common::events::rejection(&events),
        Reject::CloseSpreadCeiling { ceiling_bps: 20 }
    ), "{events:?}");
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, f("150"));
    let events = reduce(&mut h, &client, BTC, Side::Sell, "150", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 20, "the pure close clamps: {events:?}");
    assert!(fill.position.is_zero());
    h.assert_conserved(f("0"));
}

#[test]
fn a_triggered_order_refused_as_a_reversal_fills_its_closing_slice() {
    let (mut h, client) = funded("10000000");
    h.price(BTC, "100000");
    let events = trade(&mut h, &client, BTC, Side::Buy, "100", "100000");
    only_fill(&events);
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    let events = rest(&mut h, &client, BTC, Side::Sell, "150", OrderKind::Stop { price: f("99999") });
    assert!(events.contains(&Event::OrderAccepted { account: client.account, client_id: 7, order_id: 1 }), "{events:?}");
    // The drop through the stop's price triggers it: the reversal is over BTC's 20bps ceiling, so
    // the engine fills the 100-unit closing slice at the clamped close and cancels the rest.
    let events = h.price(BTC, "99999");
    let fill = only_fill(&events);
    assert_eq!(fill.qty, f("100"), "{events:?}");
    assert_eq!(fill.position, Fixed::ZERO, "{fill:?}");
    assert_eq!(fill.price, f("99799.002"), "{fill:?}");
    assert!(
        events.contains(&Event::OrderCancelled { account: client.account, order_id: 1, reason: CancelReason::Unfillable }),
        "{events:?}"
    );
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, Fixed::ZERO);
    h.assert_conserved(f("0"));
}

#[test]
fn adoption_adds_a_not_yet_listed_market_and_lowering_only_ever_moves_entries() {
    let mut h = harness();
    into_v39(&mut h);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling { market: 7, max_close_spread_bps: 25, effective_at: due });
    // Listing waits for the adoption: no fallback ceiling exists.
    let gold = market(7, "XAU-USD", AssetClass::Commodity, Schedule::AlwaysOpen, 500);
    let events = h.operate(AdminCommand::UpsertMarket(gold.clone()));
    assert!(rejected(&events).contains("no adopted close-spread ceiling"), "{events:?}");
    h.advance_to(due);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 25);
    h.operator_admin(AdminCommand::UpsertMarket(gold));
    // Raising — or re-adopting the same value — is refused; lowering is not; the entry stays put
    // until the notice runs.
    let later = h.ts + NOTICE_MS;
    for bps in [26, 25] {
        let events = h.operate(AdminCommand::AdoptCloseSpreadCeiling { market: 7, max_close_spread_bps: bps, effective_at: later });
        assert!(rejected(&events).contains("can only be lowered"), "{events:?}");
    }
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling { market: 7, max_close_spread_bps: 18, effective_at: later });
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 25);
    h.advance_to(later);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 18);
    // An adoption effective inside the notice is refused.
    let events = h.operate(AdminCommand::AdoptCloseSpreadCeiling { market: 7, max_close_spread_bps: 10, effective_at: h.ts + 1 });
    assert!(rejected(&events).contains("no sooner"), "{events:?}");
}

#[test]
fn no_operator_command_raises_a_close_above_its_ceiling() {
    let (mut h, client) = funded("10000000");
    // The widest parameters the engine admits: a 100bps baseline stress minimum. AAPL's closes
    // still pay at most their 15bps ceiling; the open pays what it prices.
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 100;
    h.operator_admin(AdminCommand::SetParams(raised));
    h.price(AAPL, "230");
    let events = trade(&mut h, &client, AAPL, Side::Buy, "100", "230");
    let open = only_fill(&events);
    assert_eq!(open.spread_bps, 100, "an open pays what it prices: {events:?}");
    let events = trade(&mut h, &client, AAPL, Side::Sell, "100", "230");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 15, "{events:?}");
    assert_eq!(fill.price, f("229.655"), "{fill:?}");
    h.assert_conserved(f("0"));
}

#[test]
fn a_ceilinged_market_cannot_change_instrument_identity() {
    let mut h = harness();
    into_v39(&mut h);
    let mut other_class = market(ETH, "ETH-EUR", AssetClass::Fx, Schedule::AlwaysOpen, 1000);
    other_class.quote_currency = QuoteCurrency::Usd;
    let events = h.operate(AdminCommand::UpsertMarket(other_class));
    assert!(rejected(&events).contains("asset class"), "{events:?}");
    let mut other_currency = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1000);
    other_currency.quote_currency = QuoteCurrency::Eur { fx_market: 4 };
    let events = h.operate(AdminCommand::UpsertMarket(other_currency));
    assert!(rejected(&events).contains("quote currency"), "{events:?}");
    // A relist that keeps the instrument's identity applies.
    let mut relisted = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1500);
    relisted.symbol = "ETH2-USD".into();
    h.operator_admin(AdminCommand::UpsertMarket(relisted));
}

#[test]
fn a_zero_ceiling_means_free_closes() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    install_expect(&mut h, with(&ceilings(), BTC, 0));
    let client = Client::new(1);
    h.deposit(client.account, "10000000");
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    into_v39(&mut h);
    attest(&mut h, &client);
    h.price(BTC, "100000");
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "100000");
    only_fill(&events);
    // A reversal whose opening slice pays no size spread fills free.
    let events = trade(&mut h, &client, BTC, Side::Sell, "2", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 0, "{events:?}");
    assert_eq!(fill.price, f("100000"));
    // Back to flat, also free, then a long position big enough to pay a size spread on its next
    // reversal's opening slice.
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "100000");
    only_fill(&events);
    let events = trade(&mut h, &client, BTC, Side::Buy, "100", "100000");
    only_fill(&events);
    // A reversal whose opening slice pays anything is refused.
    let events = trade(&mut h, &client, BTC, Side::Sell, "200", "100000");
    assert!(matches!(
        common::events::rejection(&events),
        Reject::CloseSpreadCeiling { ceiling_bps: 0 }
    ), "{events:?}");
}

#[test]
fn pre_v39_closes_pay_what_they_priced_and_the_registry_is_inert() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    install_expect(&mut h, ceilings());
    let a = acct(1);
    h.deposit(a, "1000000");
    h.price(ETH, "4000");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(ETH), level: 1 });
    let events = h.market_order(a, ETH, Side::Buy, "1", "4000");
    only_fill(&events);
    let events = h.market_order(a, ETH, Side::Sell, "1", "4000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 5, "pre-V39 a close pays the stress minimum: {events:?}");
    // The registry is installed but inert: no ceiling applies before `V39`.
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
}

#[test]
fn the_registry_and_pending_adoption_round_trip_through_serialization() {
    let mut h = harness();
    into_v39(&mut h);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling { market: ETH, max_close_spread_bps: 10, effective_at: due });
    let encoded = serde_json::to_vec(&h.engine).expect("engine serialises");
    let mut decoded: engine_core::Engine = serde_json::from_slice(&encoded).expect("engine deserialises");
    assert_eq!(decoded.close_spread_ceilings(), h.engine.close_spread_ceilings());
    // The pending adoption applies at its time on the deserialized engine too.
    let seq = h.log.len() as u64;
    let events = decoded.apply(&engine_core::command::Sequenced { seq, ts: due, cmd: Command::Tick });
    assert!(events.is_ok());
    assert_eq!(decoded.close_spread_ceilings().unwrap()[&ETH], 10);
}
