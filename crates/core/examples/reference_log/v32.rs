//! The session's `V32` part: a reversal that lands after the losses it unwinds becomes a per-account
//! emission debt, paid down by later loss weight before it earns again.

use alloy_primitives::B256;
use engine_core::access::{
    AccessEvidence, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus, Screening,
    receipt_access_deposit,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::{self, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};
use k256::ecdsa::SigningKey;

use crate::common::emissions::{allocations, next_period};
use crate::common::events::only_fill;
use crate::common::{Client, Harness, f, market};

/// The market this part trades.
const VLT: MarketId = 33;

/// `V32` activates. A trader loses half its receipt's principal in fills and earns its period's
/// allocation; the receipt is reversed in the next period, and the write-off, with no losses there to
/// absorb it, becomes an emission debt. The period after that pays the debt down without earning, and
/// the one after pays it off, the weight beyond it earning again.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V32 });
    h.operator_admin(AdminCommand::UpsertMarket(market(VLT, "VLT", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
    let source = SigningKey::from_slice(&[0x77; 32]).expect("a valid scalar");
    let trader = Client::new(80).account;
    declare(h, &source, trader);
    let weight_of = |events: &[Event]| {
        allocations(events).into_iter().find(|(account, ..)| *account == trader).map(|(_, _, weight, _)| weight)
    };

    let deposit = receipt(1, trader);
    credit(h, &source, &deposit);
    lose(h, &source, trader, "100", "90");
    assert_eq!(weight_of(&next_period(h)), Some(f("50")));

    let observed_at_seq = h.engine.recorded_deposit(&deposit.reference()).unwrap().updated_at_seq;
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: deposit, observed_at_seq })));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BadDebt { account, amount } if *account == trader && *amount == f("50"))),
        "{events:?}"
    );
    assert_eq!(weight_of(&next_period(h)), None);
    assert_eq!(h.engine.emission_debt(trader), f("50"), "the unabsorbed write-off is a debt");

    credit(h, &source, &receipt(2, trader));
    lose(h, &source, trader, "100", "94");
    assert_eq!(weight_of(&next_period(h)), None);
    assert_eq!(h.engine.emission_debt(trader), f("20"), "later loss weight pays the debt down first");

    credit(h, &source, &receipt(3, trader));
    lose(h, &source, trader, "100", "90");
    assert_eq!(weight_of(&next_period(h)), Some(f("30")));
    assert_eq!(h.engine.emission_debt(trader), f("0"));
}

/// Buys five VLT at `from`, lets the move window pass, and sells them at `to`.
fn lose(h: &mut Harness, source: &SigningKey, trader: AccountId, from: &str, to: &str) {
    for (side, price) in [(Side::Buy, from), (Side::Sell, to)] {
        h.advance(61_000);
        h.price(VLT, price);
        let client = Client { expires_at: h.ts + 60_000, ..Client::new(80) };
        let nonce = h.engine.next_request_nonce(trader);
        let request = OrderRequest {
            account: trader,
            client_id: 33,
            market: VLT,
            side,
            qty: f("5"),
            kind: OrderKind::Market { quoted: f(price), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        let Command::PlaceOrder(request) = client.order(nonce, request) else { unreachable!("a signed order") };
        let hash = eip712::order_hash(&client.domain, &request, nonce, client.expires_at);
        let evidence = evidence(h, source, hash, Some(trader), None);
        only_fill(&h.cmd(Command::AccessOrder { request, evidence }));
    }
}

/// Declares `trader` eligible from an allowed jurisdiction.
fn declare(h: &mut Harness, source: &SigningKey, trader: AccountId) {
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(80) };
    let nonce = h.engine.next_request_nonce(trader);
    let class = EligibilityClass::Professional;
    let hash = eip712::eligibility_hash(&client.domain, trader, class, nonce, client.expires_at);
    let auth = RequestAuth { nonce, expires_at: client.expires_at, signature: eip712::sign(&client.key, &hash) };
    let evidence = evidence(h, source, hash, Some(trader), None);
    let request = EligibilityRequest { account: trader, class, auth };
    let events = h.cmd(Command::AttestEligibility { request, evidence });
    assert!(matches!(events[..], [Event::EligibilityAttested { .. }]), "{events:?}");
}

/// Credits `receipt` on clear evidence, paid by its beneficiary.
fn credit(h: &mut Harness, source: &SigningKey, receipt: &DepositReceipt) {
    let DepositBeneficiary::Trader { account } = receipt.beneficiary else { unreachable!("trader receipts only") };
    let deposit = receipt_access_deposit(receipt, Some(account));
    let evidence = evidence(h, source, eip712::deposit_screening_hash(&deposit), deposit.account, Some(account));
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(account), evidence });
    assert!(events.iter().any(|e| matches!(e, Event::CustodyDepositChanged { .. })), "{events:?}");
}

fn receipt(id: u64, account: AccountId) -> DepositReceipt {
    DepositReceipt {
        block: 3_000_000 + id,
        block_hash: B256::repeat_byte(0x81 + id as u8),
        tx_hash: B256::repeat_byte(0x91 + id as u8),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 3_000_000 + id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f("100"),
    }
}

/// Fresh allowed, clear evidence for `request_hash`, signed by `source`.
fn evidence(
    h: &Harness,
    source: &SigningKey,
    request_hash: B256,
    account: Option<AccountId>,
    payer: Option<AccountId>,
) -> AccessEvidence {
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
    screening.signature = eip712::sign(source, &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}
