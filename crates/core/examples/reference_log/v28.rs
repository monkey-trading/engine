//! V28 reserves unfunded realised client losses from staking liquidity.

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

const LIQUID: MarketId = 280;
const STALE: MarketId = 281;

/// A real partial liquidation leaves cash negative while its other position has no fresh quote.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V28 });
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(81) };
    let mut parameters = h.engine.params().unwrap().clone();
    parameters.profit_fee_bps = 0;
    parameters.house_total_cap_floor = f("1000000000");
    for level in &mut parameters.stress_levels {
        level.min_spread_bps = 0;
    }
    h.operator_admin(AdminCommand::SetParams(parameters));
    for id in [LIQUID, STALE] {
        let mut spec = market(id, "V28-reserve", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
        spec.house_cap_floor = f("1000000000");
        h.operator_admin(AdminCommand::UpsertMarket(spec));
    }
    fund(h, None, client.account, "5000", "0xv28:house");
    let reference = "0xv28:trader";
    fund(h, Some(client.account), client.account, "1000", reference);
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
    h.price(LIQUID, "200000");
    h.price(STALE, "100");
    buy(h, &client, LIQUID, "0.01", "200000");
    buy(h, &client, STALE, "1", "100");
    h.advance(2_000);
    let events = h.price(LIQUID, "1");
    assert!(
        events.iter().any(|event| matches!(event, Event::CloseOut { account, .. } if *account == client.account)),
        "{events:?}"
    );
    assert!(h.engine.accounts()[&client.account].position(LIQUID).qty.is_zero());
    assert_eq!(h.engine.accounts()[&client.account].position(STALE).qty, f("1"));
    let debt = -h.engine.accounts()[&client.account].balance;
    assert!(debt.is_positive());
    let cash = h.engine.house_cash();
    let staking = h.engine.staking().unwrap();
    let cap = cash.raw().saturating_sub(debt.raw()).saturating_sub(staking.config.liquidity_floor.raw()).max(0) as u128
        / 1_000;
    let before = staking.distributed;
    let end = staking.config.epoch_end(staking.epoch);
    h.advance_to(end);
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::DistributionEpochClosed { .. })), "{events:?}");
    assert!(h.engine.staking().unwrap().distributed.0 - before.0 <= cap, "unfunded client loss is not claimable");
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
fn buy(h: &mut Harness, client: &Client, market: MarketId, qty: &str, quoted: &str) {
    let request = OrderRequest {
        account: client.account,
        client_id: 28,
        market,
        side: Side::Buy,
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
