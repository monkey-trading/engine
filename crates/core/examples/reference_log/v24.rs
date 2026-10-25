//! Screened signed eligibility, orders and withdrawals, and custody holds and retries for V24.

use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, AccessReject, EligibilityClass, EligibilityRequest, Jurisdiction,
    SanctionsStatus, Screening, SourceFailure,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use k256::ecdsa::SigningKey;

use crate::common::{BTC, Client, Harness, f};

/// The freshness bound the session records for source evidence.
const MAX_AGE_MS: i64 = 30_000;

/// Records the screening authority and activates V24. A legacy order is refused for missing
/// evidence; a client declares eligibility, once, from an allowed jurisdiction; deposits credit, are
/// held for a confirmed sanction or a missing payer, or wait pending for a declaration, a
/// classification, or a working source and credit once retried, a declaration from the US leaving
/// one pending until an allowed one is recorded; screened orders fill or are refused by jurisdiction, and a resting order
/// fills on its placement screening after the freshness bound has passed; withdrawals are approved,
/// refused for an unavailable payout, or refused for a sanctioned account; and a cancel needs no
/// screening.
pub(crate) fn session(h: &mut Harness) {
    let source = SigningKey::from_slice(&[0x77; 32]).expect("a valid scalar");
    let policy = AccessPolicy { source: eip712::address(source.verifying_key()), max_age_ms: MAX_AGE_MS };
    h.admin(AdminCommand::SetAccessPolicy(policy));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 });
    let s = Session { source };
    h.price(BTC, "80000");

    // A signed order from an account funded before V24 carries no screening evidence.
    let legacy = h.signer(1);
    let events = h.market_order(legacy, BTC, Side::Buy, "0.01", "80000");
    assert!(matches!(
        events[..],
        [Event::OrderRejected { reason: Reject::Access { reason: AccessReject::EvidenceMissing }, .. }]
    ));

    // Declarations: accepted once, then refused as a repeat, and refused when another key signs.
    let v = Client { expires_at: h.ts + 60_000, ..Client::new(20) };
    let w = Client { expires_at: h.ts + 60_000, ..Client::new(21) };
    let allowed = Jurisdiction::Allowed;
    assert!(matches!(s.declare(h, &v, &v, allowed)[..], [Event::EligibilityAttested { .. }]));
    assert!(matches!(s.declare(h, &v, &v, allowed)[..], [Event::EligibilityRejected { .. }]));
    assert!(matches!(
        s.declare(h, &w, &v, allowed)[..],
        [Event::EligibilityRejected { reason: Reject::InvalidSignature, .. }]
    ));

    // Deposits: clear funding credits; a sanctioned payer is held, and a held reference stays held
    // when redelivered clear; a payer-less deposit is held; clear house funding credits; an
    // unclassified payer leaves a deposit pending; and an undeclared beneficiary's deposit waits
    // pending through a declaration refused from the US until an allowed one and a retry credit it.
    let clear = SanctionsStatus::Clear;
    s.deposit(h, Some(v.account), v.account, "0xaccess:1", clear, "deposited");
    s.deposit(h, Some(v.account), w.account, "0xaccess:3", SanctionsStatus::Sanctioned, "deposit_held");
    s.deposit(h, Some(v.account), w.account, "0xaccess:3", clear, "deposit_held");
    let payerless =
        AccessDeposit { account: Some(v.account), payer: None, amount: f("1000"), chain_ref: "0xaccess:6".into() };
    let unavailable = AccessEvidence::Failed { reason: SourceFailure::Unavailable };
    let events = h.cmd(Command::AccessDeposit { deposit: payerless, evidence: unavailable.clone() });
    assert!(matches!(events[..], [Event::DepositHeld { reason: AccessReject::PayerMissing, .. }]));
    s.deposit(h, None, v.account, "0xaccess:4", clear, "house_capital_moved");
    s.deposit(h, Some(v.account), v.account, "0xaccess:7", SanctionsStatus::Unknown, "deposit_pending");
    let undeclared = AccessDeposit {
        account: Some(w.account),
        payer: Some(w.account),
        amount: f("1000"),
        chain_ref: "0xaccess:2".into(),
    };
    let evidence = s.deposit_evidence(h, &undeclared, clear);
    h.cmd(Command::AccessDeposit { deposit: undeclared.clone(), evidence });
    let w = Client { expires_at: h.ts + 60_000, ..w };
    assert!(matches!(
        s.declare(h, &w, &w, Jurisdiction::UnitedStates)[..],
        [Event::EligibilityRejected { reason: Reject::Access { reason: AccessReject::UnitedStates }, .. }]
    ));
    assert!(matches!(s.declare(h, &w, &w, allowed)[..], [Event::EligibilityAttested { .. }]));
    let evidence = s.deposit_evidence(h, &undeclared, clear);
    let events = h.cmd(Command::RetryDepositScreening { chain_ref: undeclared.chain_ref, evidence });
    assert!(matches!(events[..], [Event::Deposited { .. }]));
    // An unavailable source leaves a deposit pending; a redelivery and a failed retry keep it so, and
    // a verified retry credits it once.
    let pending = AccessDeposit {
        account: Some(v.account),
        payer: Some(v.account),
        amount: f("500"),
        chain_ref: "0xaccess:5".into(),
    };
    h.cmd(Command::AccessDeposit { deposit: pending.clone(), evidence: unavailable.clone() });
    h.cmd(Command::AccessDeposit { deposit: pending.clone(), evidence: unavailable.clone() });
    h.cmd(Command::RetryDepositScreening { chain_ref: pending.chain_ref.clone(), evidence: unavailable });
    let evidence = s.deposit_evidence(h, &pending, clear);
    let events =
        h.cmd(Command::RetryDepositScreening { chain_ref: pending.chain_ref.clone(), evidence: evidence.clone() });
    assert!(matches!(events[..], [Event::Deposited { .. }]));
    h.cmd(Command::RetryDepositScreening { chain_ref: pending.chain_ref, evidence });

    // Orders: a screened order fills, a US peer is refused, and a resting limit triggered after the
    // freshness bound fills on its placement screening.
    let events =
        s.order(h, &v, OrderKind::Market { quoted: f("80000"), max_slippage_bps: None }, Jurisdiction::Allowed);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))));
    let events =
        s.order(h, &v, OrderKind::Market { quoted: f("80000"), max_slippage_bps: None }, Jurisdiction::UnitedStates);
    assert!(matches!(
        events[..],
        [Event::OrderRejected { reason: Reject::Access { reason: AccessReject::UnitedStates }, .. }]
    ));
    s.order(h, &v, OrderKind::Limit { price: f("79000") }, Jurisdiction::Allowed);
    h.advance(MAX_AGE_MS + 1);
    let events = h.price(BTC, "78900");
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.account == v.account)), "{events:?}");

    // Withdrawals: approved with a payout path, refused without one, and refused for a sanctioned
    // account; then a resting order cancels without screening.
    let v = Client { expires_at: h.ts + 60_000, ..v };
    assert!(matches!(s.withdraw(h, &v, true, clear)[..], [Event::WithdrawalApproved { .. }]));
    assert!(matches!(
        s.withdraw(h, &v, false, clear)[..],
        [Event::WithdrawalRejected { reason: Reject::PayoutUnavailable, .. }]
    ));
    let events = s.withdraw(h, &v, true, SanctionsStatus::Sanctioned);
    assert!(matches!(
        events[..],
        [Event::WithdrawalRejected { reason: Reject::Access { reason: AccessReject::AccountSanctioned }, .. }]
    ));
    let events = s.order(h, &v, OrderKind::Limit { price: f("70000") }, Jurisdiction::Allowed);
    let Some(order_id) = events.iter().find_map(|event| match event {
        Event::OrderAccepted { order_id, .. } => Some(*order_id),
        _ => None,
    }) else {
        panic!("{events:?}")
    };
    let nonce = h.engine.next_request_nonce(v.account);
    assert!(matches!(
        h.cmd(v.cancel(nonce, order_id))[..],
        [Event::OrderCancelled { reason: CancelReason::Client, .. }]
    ));
}

/// The session's screening authority.
struct Session {
    source: SigningKey,
}

impl Session {
    /// A declaration for `client`'s account signed by `signer`, observed from `jurisdiction`.
    fn declare(&self, h: &mut Harness, client: &Client, signer: &Client, jurisdiction: Jurisdiction) -> Vec<Event> {
        let nonce = h.engine.next_request_nonce(client.account);
        let class = EligibilityClass::Professional;
        let hash = eip712::eligibility_hash(&client.domain, client.account, class, nonce, client.expires_at);
        let auth = RequestAuth { nonce, expires_at: client.expires_at, signature: eip712::sign(&signer.key, &hash) };
        let screening = Screening { jurisdiction, ..self.screening(h, hash, Some(client.account), None) };
        let evidence = self.sign(h, screening);
        h.cmd(Command::AttestEligibility {
            request: EligibilityRequest { account: client.account, class, auth },
            evidence,
        })
    }

    /// Sequences 1000 of funding from `payer` with `status` for the payer, checking its event type.
    fn deposit(
        &self,
        h: &mut Harness,
        account: Option<AccountId>,
        payer: AccountId,
        chain_ref: &str,
        status: SanctionsStatus,
        expected: &str,
    ) {
        let deposit = AccessDeposit { account, payer: Some(payer), amount: f("1000"), chain_ref: chain_ref.into() };
        let evidence = self.deposit_evidence(h, &deposit, status);
        let events = h.cmd(Command::AccessDeposit { deposit, evidence });
        assert_eq!(serde_json::to_value(&events[0]).unwrap()["type"], expected, "{events:?}");
    }

    /// Fresh signed evidence for `deposit`, with `status` for its payer.
    fn deposit_evidence(&self, h: &Harness, deposit: &AccessDeposit, status: SanctionsStatus) -> AccessEvidence {
        let mut screening = self.screening(h, eip712::deposit_screening_hash(deposit), deposit.account, deposit.payer);
        screening.payer_status = status;
        self.sign(h, screening)
    }

    /// A screened 0.01 BTC buy of `kind` from `client`, observed from `jurisdiction`.
    fn order(&self, h: &mut Harness, client: &Client, kind: OrderKind, jurisdiction: Jurisdiction) -> Vec<Event> {
        let nonce = h.engine.next_request_nonce(client.account);
        let request = OrderRequest {
            account: client.account,
            client_id: 20,
            market: BTC,
            side: Side::Buy,
            qty: f("0.01"),
            kind,
            reduce_only: false,
            auth: None,
        };
        let Command::PlaceOrder(request) = client.order(nonce, request) else { unreachable!("a signed order") };
        let hash = eip712::order_hash(&client.domain, &request, nonce, client.expires_at);
        let screening = Screening { jurisdiction, ..self.screening(h, hash, Some(client.account), None) };
        h.cmd(Command::AccessOrder { request, evidence: self.sign(h, screening) })
    }

    /// A screened withdrawal of 1 from `client`, with `status` for the account.
    fn withdraw(&self, h: &mut Harness, client: &Client, payout: bool, status: SanctionsStatus) -> Vec<Event> {
        let nonce = h.engine.next_request_nonce(client.account);
        let Command::Withdraw { account, amount, auth } = client.withdraw(nonce, "1") else { unreachable!() };
        let hash = eip712::withdrawal_hash(&client.domain, account, amount, nonce, client.expires_at);
        let screening = Screening { account_status: status, ..self.screening(h, hash, Some(account), None) };
        let request = WithdrawalRequest { account, amount, auth };
        h.cmd(Command::AccessWithdraw { request, evidence: self.sign(h, screening), payout_available: payout })
    }

    /// An allowed, clear observation of `account` and `payer` made now, still unsigned.
    fn screening(
        &self,
        h: &Harness,
        request_hash: B256,
        account: Option<AccountId>,
        payer: Option<AccountId>,
    ) -> Screening {
        Screening {
            source: eip712::address(self.source.verifying_key()),
            request_hash,
            observed_at: h.ts,
            account,
            payer,
            jurisdiction: Jurisdiction::Allowed,
            account_status: SanctionsStatus::Clear,
            payer_status: SanctionsStatus::Clear,
            signature: Signature::ZERO,
        }
    }

    /// `screening` signed by the authority under the recorded deployment domain.
    fn sign(&self, h: &Harness, mut screening: Screening) -> AccessEvidence {
        let domain = h.engine.access_domain().expect("the signed harness records its domain");
        screening.signature = eip712::sign(&self.source, &eip712::screening_hash(&domain, &screening));
        AccessEvidence::Screened(screening)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_reference_session_reaches_every_access_path_without_writing_a_fixture() {
        let mut h = Harness::v17();
        session(&mut h);
        assert_eq!(h.engine.protocol_version(), ProtocolVersion::V24);
        assert_eq!(h.engine.held_deposits().len(), 2);
        assert_eq!(h.engine.pending_deposits().len(), 1, "the unclassified payer's deposit waits");
    }
}
