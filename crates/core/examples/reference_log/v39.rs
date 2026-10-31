//! The session's `V39` part: close-spread ceilings cap what every close can pay.

use engine_core::access::{AccessDeposit, AccessEvidence, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus, Screening};
use engine_core::auth::RequestAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, Signature};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::Fixed;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, Side};
use k256::ecdsa::SigningKey;

use crate::common::{AAPL, BTC, Client, Harness, f, market};
use crate::v37::ELECTED_SEED;

/// The seed of the key this part trades through.
const CLIENT_SEED: u8 = 0x39;

/// Installs the ceiling registry over the markets the session lists, then activates `V39`. A
/// close whose charge rises above its ceiling clamps to it, floored to the exact bound; a reversal
/// over it is refused while the reduce-only close stays open, and a triggered stop fills its
/// closing slice and cancels the rest; a market lists only once its ceiling is adopted, an entry
/// only ever lowers, and an adopted market cannot change instrument.
pub(crate) fn session(h: &mut Harness) {
    let ceilings: Vec<CloseSpreadCeiling> = h
        .engine
        .markets()
        .values()
        .map(|market| {
            // The genesis harness's launch close charge is 8bps, so every value clears it.
            let bps = match market.spec.asset_class {
                AssetClass::Crypto => 30,
                AssetClass::Fx => 13,
                AssetClass::Index | AssetClass::Equity => 15,
                AssetClass::Commodity => 20,
                other => panic!("no close-spread ceiling for {other:?}"),
            };
            CloseSpreadCeiling { market: market.spec.id, max_close_spread_bps: bps }
        })
        .collect();
    assert_eq!(
        h.operate_as(ELECTED_SEED, AdminCommand::SetCloseSpreadCeilings { ceilings }).last(),
        Some(&Event::AdminApplied)
    );
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    // BTC's fills in this part pay spreads far above the fixture's 20bps slippage bound; the
    // relist changes no price rule or tier, only the client's guard, which the ceiling keeps.
    let mut btc = h.engine.markets()[&BTC].spec.clone();
    btc.max_slippage_bps = 10_000;
    assert_eq!(h.operate_as(ELECTED_SEED, AdminCommand::UpsertMarket(btc)).last(), Some(&Event::AdminApplied));
    // Raise the widest stress level's minimum above BTC's 30bps ceiling.
    let mut raised = h.engine.params().cloned().expect("parameters are set");
    let top = raised.stress_levels.len() - 1;
    raised.stress_levels[top].min_spread_bps = 100;
    assert_eq!(h.operate_as(ELECTED_SEED, AdminCommand::SetParams(raised)).last(), Some(&Event::AdminApplied));
    assert_eq!(
        h.operate_as(ELECTED_SEED, AdminCommand::SetStressFloor { market: Some(BTC), level: top }).last(),
        Some(&Event::AdminApplied)
    );

    // A declared, funded client trades BTC through the screened order command.
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(CLIENT_SEED) };
    declare(h, &client);
    fund_house(h, &client);
    fund(h, &client);
    h.price(BTC, "80000");
    let events = order(h, &client, Side::Buy, "0.01", OrderKind::Market { quoted: f("80000"), max_slippage_bps: None });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.spread_bps == 100)));
    // The close clamps to BTC's ceiling, and the clamp floors to the exact bound.
    let events = order(h, &client, Side::Sell, "0.01", OrderKind::Market { quoted: f("80000"), max_slippage_bps: None });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.spread_bps == 30 && fill.price == f("79760"))), "{events:?}");

    // A reversal over the ceiling is refused; the pure close stays open at the ceiling.
    let events = order(h, &client, Side::Buy, "0.01", OrderKind::Market { quoted: f("80000"), max_slippage_bps: None });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let events = order(h, &client, Side::Sell, "0.02", OrderKind::Market { quoted: f("80000"), max_slippage_bps: None });
    assert!(matches!(
        events[..],
        [Event::OrderRejected { reason: Reject::CloseSpreadCeiling { ceiling_bps: 30 }, .. }]
    ), "{events:?}");
    let events = reduce(h, &client, "0.01", "80000");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.spread_bps == 30)), "{events:?}");

    // A triggered stop refused as a reversal fills its closing slice and cancels the rest.
    let events = order(h, &client, Side::Buy, "0.01", OrderKind::Market { quoted: f("80000"), max_slippage_bps: None });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let events = order(h, &client, Side::Sell, "0.02", OrderKind::Stop { price: f("79999") });
    assert!(events.iter().any(|event| matches!(event, Event::OrderAccepted { .. })), "{events:?}");
    let events = h.price(BTC, "79999");
    let (fills, cancels) = events.iter().fold((0, 0), |(fills, cancels), event| match event {
        Event::Fill(fill) if fill.qty == f("0.01") && fill.price == f("79759.003") => (fills + 1, cancels),
        Event::OrderCancelled { reason: CancelReason::Unfillable, .. } => (fills, cancels + 1),
        _ => (fills, cancels),
    });
    assert_eq!((fills, cancels), (1, 1), "{events:?}");

    // A tiny index floors the adjustment to the exact bound: the charged spread is
    // `floor(index_raw * 30 / 10_000)`, never the unit-above value upward rounding gives.
    h.price(BTC, "0.654321987");
    let events = order(h, &client, Side::Buy, "0.01", OrderKind::Market { quoted: f("0.654321987"), max_slippage_bps: None });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let events = reduce(h, &client, "0.01", "0.654321987");
    let bound = f("0.654321987").raw() * 30 / 10_000;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Fill(fill) if fill.price == f("0.654321987") - Fixed::from_raw(bound))),
        "{events:?}"
    );

    // A new market lists only once its ceiling is adopted, through the seven-day notice; an entry
    // then only ever lowers.
    let adopted = h.ts + NOTICE_MS;
    assert_eq!(
        h.operate_as(
            ELECTED_SEED,
            AdminCommand::AdoptCloseSpreadCeiling { market: 39, max_close_spread_bps: 20, effective_at: adopted },
        )
        .last(),
        Some(&Event::AdminApplied)
    );
    let silver = market(39, "XAG-USD", AssetClass::Commodity, Schedule::AlwaysOpen, 1_000);
    let refused = h.operate_as(ELECTED_SEED, AdminCommand::UpsertMarket(silver.clone()));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { reason }) if reason.contains("no adopted close-spread ceiling")), "{refused:?}");
    h.advance_to(adopted);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().and_then(|ceilings| ceilings.get(&39).copied()), Some(20));
    assert_eq!(h.operate_as(ELECTED_SEED, AdminCommand::UpsertMarket(silver)).last(), Some(&Event::AdminApplied));
    let raised = h.operate_as(
        ELECTED_SEED,
        AdminCommand::AdoptCloseSpreadCeiling { market: 39, max_close_spread_bps: 21, effective_at: h.ts + NOTICE_MS },
    );
    assert!(matches!(raised.last(), Some(Event::AdminRejected { reason }) if reason.contains("can only be lowered")), "{raised:?}");

    // An adopted market cannot change instrument: AAPL's asset class stays.
    let mut etf = h.engine.markets()[&AAPL].spec.clone();
    etf.asset_class = AssetClass::Etf;
    let refused = h.operate_as(ELECTED_SEED, AdminCommand::UpsertMarket(etf));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { reason }) if reason.contains("asset class")), "{refused:?}");
}


/// The screening source the `V24` part recorded.
fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).expect("a valid scalar")
}

/// Fresh, allowed and clear evidence for `hash`, signed by the recorded source.
fn screened(h: &Harness, hash: eip712::B256, account: Option<AccountId>, payer: Option<AccountId>) -> AccessEvidence {
    let mut screening = Screening {
        source: eip712::address(source().verifying_key()),
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

/// Records the client's professional declaration.
fn declare(h: &mut Harness, client: &Client) {
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::eligibility_hash(&client.domain, client.account, EligibilityClass::Professional, nonce, expires_at);
    let request = EligibilityRequest {
        account: client.account,
        class: EligibilityClass::Professional,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) },
    };
    let evidence = screened(h, hash, Some(client.account), None);
    let events = h.cmd(Command::AttestEligibility { request, evidence });
    assert!(matches!(events[..], [Event::EligibilityAttested { .. }]), "{events:?}");
}

/// Restocks the house's capital, which the earlier parts paid out.
fn fund_house(h: &mut Harness, payer: &Client) {
    let deposit = AccessDeposit { account: None, payer: Some(payer.account), amount: f("2000000"), chain_ref: "0xhouse:39".into() };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), None, Some(payer.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. })), "{events:?}");
}

/// Funds the client with a screened deposit.
fn fund(h: &mut Harness, client: &Client) {
    let deposit = AccessDeposit {
        account: Some(client.account),
        payer: Some(client.account),
        amount: f("1000000"),
        chain_ref: "0xclient:39".into(),
    };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), Some(client.account), Some(client.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(matches!(events[..], [Event::Deposited { .. }]), "{events:?}");
}

/// The client's signed, screened order of `kind` for `qty`.
fn order(h: &mut Harness, client: &Client, side: Side, qty: &str, kind: OrderKind) -> Vec<Event> {
    let nonce = h.engine.next_request_nonce(client.account);
    let request = OrderRequest {
        account: client.account,
        client_id: 39,
        market: BTC,
        side,
        qty: f(qty),
        kind,
        reduce_only: false,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(nonce, request) else { unreachable!("a signed order") };
    let auth = request.auth.as_ref().unwrap();
    let hash = eip712::order_hash(&client.domain, &request, auth.nonce, auth.expires_at);
    let evidence = screened(h, hash, Some(client.account), None);
    h.cmd(Command::AccessOrder { request, evidence })
}

/// A signed, screened reduce-only close of the client's whole BTC position at `quoted`.
fn reduce(h: &mut Harness, client: &Client, qty: &str, quoted: &str) -> Vec<Event> {
    let nonce = h.engine.next_request_nonce(client.account);
    let request = OrderRequest {
        account: client.account,
        client_id: 39,
        market: BTC,
        side: Side::Sell,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(nonce, request) else { unreachable!("a signed order") };
    let auth = request.auth.as_ref().unwrap();
    let hash = eip712::order_hash(&client.domain, &request, auth.nonce, auth.expires_at);
    let evidence = screened(h, hash, Some(client.account), None);
    h.cmd(Command::AccessOrder { request, evidence })
}

