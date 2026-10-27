//! V29 discounts uncollectible marked equity before staking distributions.

use engine_core::access::{
    AccessDeposit, AccessEvidence, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus, Screening,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};
use k256::ecdsa::SigningKey;

use crate::common::{Client, Harness, f, market};

const FX: MarketId = 290;
const SHARE: MarketId = 291;

/// A closed euro share retains its mark while its FX quote makes a positive-cash long insolvent.
pub(crate) fn session(h: &mut Harness) {
    let mut parameters = h.engine.params().unwrap().clone();
    parameters.profit_fee_bps = 0;
    parameters.stress_levels.truncate(1);
    parameters.toxicity.tiers.clear();
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.operator_admin(AdminCommand::UpsertMarket(market(FX, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)));
    let mut share = market(SHARE, "V29-share", AssetClass::Equity, Schedule::AlwaysOpen, 1_000);
    share.quote_currency = engine_core::market::QuoteCurrency::Eur { fx_market: FX };
    share.house_cap_floor = f("1000000000");
    share.financing.markup_bps = 0;
    h.operator_admin(AdminCommand::UpsertMarket(share.clone()));
    let losing = Client { expires_at: h.ts + 60_000, ..Client::new(82) };
    let winning = Client { expires_at: h.ts + 60_000, ..Client::new(83) };
    fund(h, None, losing.account, "5000", "0xv29:house");
    for (client, amount, reference) in [(&losing, "906", "0xv29:long"), (&winning, "1000", "0xv29:short")] {
        fund(h, Some(client.account), client.account, amount, reference);
        let class = EligibilityClass::Professional;
        let nonce = h.engine.next_request_nonce(client.account);
        let hash = eip712::eligibility_hash(&client.domain, client.account, class, nonce, client.expires_at);
        let request = EligibilityRequest {
            account: client.account,
            class,
            auth: RequestAuth { nonce, expires_at: client.expires_at, signature: eip712::sign(&client.key, &hash) },
        };
        let evidence = screened(h, hash, Some(client.account), None);
        let events = h.cmd(Command::AttestEligibility { request, evidence });
        assert!(events.iter().any(|event| matches!(event, Event::EligibilityAttested { .. })), "{events:?}");
        let deposit = h.engine.pending_deposits()[reference].clone();
        let evidence = screened(h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
        let events = h.cmd(Command::RetryDepositScreening { chain_ref: reference.into(), evidence });
        assert!(events.iter().any(|event| matches!(event, Event::Deposited { .. })), "{events:?}");
    }
    h.price(FX, "1");
    h.price(SHARE, "1000");
    order(h, &losing, SHARE, Side::Buy, "1", "1000");
    order(h, &winning, SHARE, Side::Sell, "1", "1000");
    h.price(SHARE, "100");
    assert_eq!(h.engine.equity_and_margin(&h.engine.accounts()[&losing.account], None).0, f("6"));
    share.schedule = Schedule::Weekly {
        tz: chrono_tz::UTC,
        windows: vec![engine_core::session::Window {
            open: "Mon 00:00".parse().unwrap(),
            close: "Sun 23:59".parse().unwrap(),
        }],
        closed_dates: vec![chrono::DateTime::from_timestamp_millis(h.ts).unwrap().date_naive()],
        closures: vec![],
        special_sessions: vec![],
    };
    h.operator_admin(AdminCommand::UpsertMarket(share));
    h.price(FX, "1.01");
    assert_eq!(h.engine.accounts()[&losing.account].balance, f("906"));
    assert_eq!(h.engine.equity_and_margin(&h.engine.accounts()[&losing.account], None).0, f("-3"));
    assert!(!h.engine.markets()[&SHARE].open);
    // Existing session positions can make the funded-cash cap bind. A solvent client's gain
    // makes NAV the active cap so this log distinguishes the new marked-equity rule.
    let mut gain = market(292, "V29-gain", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    gain.house_cap_floor = f("1000000000");
    h.operator_admin(AdminCommand::UpsertMarket(gain));
    h.price(292, "1");
    order(h, &winning, 292, Side::Buy, "1", "1");
    let gain = (h.engine.house_nav() - h.engine.house_cash()).max(f("0")) + f("1000");
    h.price(292, &(f("1") + gain).to_string());
    let deficit: i128 = h
        .engine
        .accounts()
        .values()
        .map(|account| h.engine.equity_and_margin(account, None).0.raw().saturating_neg().max(0))
        .sum();
    let nav = h.engine.house_nav();
    let cash = h.engine.house_cash();
    let reserved: i128 = h
        .engine
        .accounts()
        .values()
        .map(|account| account.unfinalised_deposits.raw() + account.balance.raw().saturating_neg().max(0))
        .sum();
    let expected = (nav.raw() - deficit)
        .min(cash.raw() - reserved)
        .saturating_sub(h.engine.staking().unwrap().config.liquidity_floor.raw())
        .max(0) as u128
        / 1_000;
    assert!(
        nav.raw() - deficit < cash.raw() - reserved,
        "collectible NAV must bind: nav={nav} deficit={deficit} cash={cash} reserved={reserved}"
    );
    let staking = h.engine.staking().unwrap();
    let before = staking.distributed;
    h.advance_to(staking.config.epoch_end(staking.epoch));
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V29 }));
    assert!(events.iter().any(|event| matches!(event, Event::AdminApplied)), "{events:?}");
    assert!(events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { .. })), "{events:?}");
    assert_eq!(h.engine.staking().unwrap().distributed.0 - before.0, expected);
}

/// Supplies signed source evidence to funding, declarations and orders.
fn screened(h: &Harness, request_hash: B256, account: Option<AccountId>, payer: Option<AccountId>) -> AccessEvidence {
    let source = SigningKey::from_slice(&[0x77; 32]).unwrap();
    let mut screening = Screening {
        source: eip712::address(source.verifying_key()),
        request_hash,
        observed_at: h.ts,
        account,
        payer,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    screening.signature =
        eip712::sign(&source, &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}

/// Funds the house or records a new client's pending deposit before its declaration.
fn fund(h: &mut Harness, account: Option<AccountId>, payer: AccountId, amount: &str, reference: &str) {
    let deposit = AccessDeposit { account, payer: Some(payer), amount: f(amount), chain_ref: reference.into() };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), account, Some(payer));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(
        events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. } | Event::DepositPending { .. })),
        "{events:?}"
    );
}

/// Opens a signed, screened position in the session's new isolated market.
fn order(h: &mut Harness, client: &Client, market: MarketId, side: Side, qty: &str, quoted: &str) {
    let request = OrderRequest {
        account: client.account,
        client_id: 29,
        market,
        side,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let nonce = h.engine.next_request_nonce(client.account);
    let Command::PlaceOrder(request) = client.order(nonce, request) else { unreachable!() };
    let auth = request.auth.as_ref().unwrap();
    let hash = eip712::order_hash(&client.domain, &request, auth.nonce, auth.expires_at);
    let evidence = screened(h, hash, Some(client.account), None);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
}
