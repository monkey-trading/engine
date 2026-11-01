//! The session's `V42` part: MONKEY emission weight is an account's gross realised losses in the
//! period — a losing fill earns its own weight even when wins in the same period leave the account
//! net positive — and the `V32` emission-debt mechanics run on gross weight.

use alloy_primitives::B256;
use engine_core::access::{
    AccessDeposit, AccessEvidence, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus, Screening,
    receipt_access_deposit,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::{DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::{self, Signature};
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};
use k256::ecdsa::SigningKey;

use crate::common::emissions::{allocations, next_period};
use crate::common::events::only_fill;
use crate::common::{Client, Harness, f, market};
use crate::v37::ELECTED_SEED;

/// The seeds of the keys this part trades through.
const WINNER_SEED: u8 = 0x43;
const DEBTOR_SEED: u8 = 0x44;

/// A fresh market this part lists, so its trades price from no stress or toxicity history.
const GRS: MarketId = 44;

/// `V42` activates. A trader with prior gains takes a losing fill inside a period its win leaves
/// net positive, and the fill still earns its full gross weight. A second trader's write-off
/// becomes an emission debt that later gross loss weight pays down before any of it earns again.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V42 });
    // The earlier parts replaced the stress table with a single level whose 100bps minimum every
    // fill would pay, carrying spread noise into the gross weights; the launch table prices a
    // quiet market at no spread. The profit fee and close-out level stay as they are, so no
    // notice waits.
    let mut restored = h.engine.params().cloned().expect("parameters are set");
    let stress_levels = crate::common::params().stress_levels;
    let widest =
        h.engine.markets().values().map(|market| market.spec.price_rules.max_divergence_bps).max().unwrap_or(0);
    let mut with_levels = restored.clone();
    with_levels.stress_levels = stress_levels.clone();
    with_levels.share_divergence_triggers(widest);
    restored.stress_levels = with_levels.stress_levels;
    assert_eq!(h.operate_as(ELECTED_SEED, AdminCommand::SetParams(restored)).last(), Some(&Event::AdminApplied));
    // A `V40` market lists only once its ceiling is adopted, through the seven-day notice.
    let adopted = h.ts + NOTICE_MS;
    assert_eq!(
        h.operate_as(
            ELECTED_SEED,
            AdminCommand::AdoptCloseSpreadCeiling { market: GRS, max_close_spread_bps: 30, effective_at: adopted },
        )
        .last(),
        Some(&Event::AdminApplied)
    );
    h.advance_to(adopted);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().and_then(|ceilings| ceilings.get(&GRS).copied()), Some(30));
    let listed = market(GRS, "GRS-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    assert_eq!(h.operate_as(ELECTED_SEED, AdminCommand::UpsertMarket(listed)).last(), Some(&Event::AdminApplied));
    let source = SigningKey::from_slice(&[0x77; 32]).expect("a valid scalar");

    // The owner's scenario: 50 of loss on one fill and a 55 win on another close the period net
    // positive 5; the win does not offset, and the losing fill earns its 50 of gross weight.
    let winner = Client { expires_at: h.ts + 60_000, ..Client::new(WINNER_SEED) };
    declare(h, &source, &winner);
    fund(h, &source, &winner);
    h.price(GRS, "1000");
    round(h, &source, &winner, "1000", "950");
    round(h, &source, &winner, "950", "1005");
    let result = h.engine.period_results()[&winner.account];
    assert_eq!((result.realised, result.losses), (f("5"), f("50")), "{result:?}");
    // The losing fill earns despite the net-positive period.
    weight_of(h, &winner, "50");

    // The debtor funds its losses with receipts alone, so a reversal writes off what the losses
    // spent. 50 of losses earn; the next period's reversal becomes an emission debt.
    let debtor = Client { expires_at: h.ts + 60_000, ..Client::new(DEBTOR_SEED) };
    declare(h, &source, &debtor);
    let deposit = receipt(1, debtor.account, "1000");
    credit(h, &source, &deposit);
    round(h, &source, &debtor, "1000", "950");
    weight_of(h, &debtor, "50");
    let observed_at_seq = h.engine.recorded_deposit(&deposit.reference()).unwrap().updated_at_seq;
    let events = h.cmd(Command::CustodyDeposit(Box::new(DepositUpdate::Reverse { receipt: deposit, observed_at_seq })));
    assert!(
        events.iter().any(
            |e| matches!(e, Event::BadDebt { account, amount } if *account == debtor.account && *amount == f("50"))
        ),
        "{events:?}"
    );
    assert!(!allocated(h, &debtor));
    assert_eq!(h.engine.emission_debt(debtor.account), f("50"), "the unabsorbed write-off is a debt");

    // A 30 loss and a 20 win: the gross 30 pays the debt down (the net 10 would not cover it), and
    // nothing earns yet.
    credit(h, &source, &receipt(2, debtor.account, "1000"));
    round(h, &source, &debtor, "1000", "970");
    round(h, &source, &debtor, "970", "990");
    assert_eq!(h.engine.period_results()[&debtor.account].losses, f("30"));
    assert!(!allocated(h, &debtor));
    assert_eq!(h.engine.emission_debt(debtor.account), f("20"), "gross loss weight pays the debt first");

    // 25 of gross loss pays the rest and the remaining 5 earns.
    credit(h, &source, &receipt(3, debtor.account, "1000"));
    round(h, &source, &debtor, "1000", "975");
    weight_of(h, &debtor, "5");
    assert_eq!(h.engine.emission_debt(debtor.account), f("0"));
}

/// Finalises the open period, asserting the client's allocation carries `expected` weight.
fn weight_of(h: &mut Harness, client: &Client, expected: &str) {
    let events = next_period(h);
    let weight = allocations(&events)
        .into_iter()
        .find(|(account, ..)| *account == client.account)
        .map(|(_, _, weight, _)| weight);
    assert_eq!(weight, Some(f(expected)), "{events:?}");
}

/// Finalises the open period and returns whether the client was allocated a share.
fn allocated(h: &mut Harness, client: &Client) -> bool {
    next_period(h).iter().any(|e| matches!(e, Event::EmissionAllocated { account, .. } if *account == client.account))
}

/// Buys one GRS at `from`, lets the move window pass, and sells it at `to`.
fn round(h: &mut Harness, source: &SigningKey, client: &Client, from: &str, to: &str) {
    for (side, price) in [(Side::Buy, from), (Side::Sell, to)] {
        h.advance(61_000);
        h.price(GRS, price);
        // Each order signs a fresh request expiry: the rounds advance sequenced time past the
        // client key's own.
        let client = Client {
            key: client.key.clone(),
            account: client.account,
            domain: client.domain.clone(),
            expires_at: h.ts + 60_000,
        };
        let nonce = h.engine.next_request_nonce(client.account);
        let request = OrderRequest {
            account: client.account,
            client_id: 42,
            market: GRS,
            side,
            qty: f("1"),
            kind: OrderKind::Market { quoted: f(price), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        let Command::PlaceOrder(request) = client.order(nonce, request) else { unreachable!("a signed order") };
        let hash = eip712::order_hash(&client.domain, &request, nonce, client.expires_at);
        let evidence = evidence(h, source, hash, Some(client.account), None);
        only_fill(&h.cmd(Command::AccessOrder { request, evidence }));
    }
}

/// Records the client's professional declaration.
fn declare(h: &mut Harness, source: &SigningKey, client: &Client) {
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let class = EligibilityClass::Professional;
    let hash = eip712::eligibility_hash(&client.domain, client.account, class, nonce, expires_at);
    let auth = RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) };
    let evidence = evidence(h, source, hash, Some(client.account), None);
    let request = EligibilityRequest { account: client.account, class, auth };
    let events = h.cmd(Command::AttestEligibility { request, evidence });
    assert!(matches!(events[..], [Event::EligibilityAttested { .. }]), "{events:?}");
}

/// Funds the client with a screened deposit.
fn fund(h: &mut Harness, source: &SigningKey, client: &Client) {
    let deposit = AccessDeposit {
        account: Some(client.account),
        payer: Some(client.account),
        amount: f("10000"),
        chain_ref: "0xclient:43".into(),
    };
    let evidence =
        evidence(h, source, eip712::deposit_screening_hash(&deposit), Some(client.account), Some(client.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(matches!(events[..], [Event::Deposited { .. }]), "{events:?}");
}

/// Credits `receipt` on clear evidence, paid by its beneficiary.
fn credit(h: &mut Harness, source: &SigningKey, receipt: &DepositReceipt) {
    let DepositBeneficiary::Trader { account } = receipt.beneficiary else { unreachable!("trader receipts only") };
    let deposit = receipt_access_deposit(receipt, Some(account));
    let evidence = evidence(h, source, eip712::deposit_screening_hash(&deposit), deposit.account, Some(account));
    let events = h.cmd(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer: Some(account), evidence });
    assert!(events.iter().any(|e| matches!(e, Event::CustodyDepositChanged { .. })), "{events:?}");
}

fn receipt(id: u64, account: AccountId, amount: &str) -> DepositReceipt {
    DepositReceipt {
        block: 3_100_000 + id,
        block_hash: B256::repeat_byte(0x85 + id as u8),
        tx_hash: B256::repeat_byte(0x95 + id as u8),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 3_100_000 + id,
        beneficiary: DepositBeneficiary::Trader { account },
        amount: f(amount),
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
