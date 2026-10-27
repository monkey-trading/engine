use crate::common::{BTC, Client, Harness, f};
use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus,
    Screening,
};
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};
use k256::ecdsa::SigningKey;

pub fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

pub fn setup() -> (Harness, Client) {
    activate_access(Harness::v8(ClientAuth::Unsigned))
}

/// A genuine unfunded log leaves the reserved genesis reference available for admission tests.
pub fn setup_with_uncredited_genesis() -> (Harness, Client) {
    let mut h = Harness::zero_house(ProtocolVersion::V4);
    h.admin(AdminCommand::SetOperator(crate::common::operator()));
    activate_access(h)
}

/// Records the same source authority and activates access rules on either fixture.
fn activate_access(mut h: Harness) -> (Harness, Client) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
        source: eip712::address(source().verifying_key()),
        max_age_ms: 1_000,
    }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 });
    h.price(BTC, "100");
    (h, Client::new(1))
}

pub fn declaration(h: &Harness, client: &Client, class: EligibilityClass) -> EligibilityRequest {
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash = eip712::eligibility_hash(&client.domain, client.account, class, nonce, expires_at);
    EligibilityRequest {
        account: client.account,
        class,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) },
    }
}

/// Fresh, allowed and clear evidence for `request`, as the API screens a declaration.
pub fn declaration_evidence(h: &Harness, request: &EligibilityRequest) -> AccessEvidence {
    let domain = h.engine.access_domain().unwrap();
    let auth = &request.auth;
    let hash = eip712::eligibility_hash(&domain, request.account, request.class, auth.nonce, auth.expires_at);
    evidence(h, hash, Some(request.account), None)
}

/// Sequences `request` with fresh, allowed and clear evidence.
pub fn declare(h: &mut Harness, request: EligibilityRequest) -> Vec<Event> {
    let evidence = declaration_evidence(h, &request);
    h.cmd(Command::AttestEligibility { request, evidence })
}

pub fn attest(h: &mut Harness, client: &Client) {
    let request = declaration(h, client, EligibilityClass::Professional);
    assert_eq!(
        declare(h, request),
        [Event::EligibilityAttested { account: client.account, class: EligibilityClass::Professional }]
    );
}

pub fn order(h: &Harness, client: &Client, kind: OrderKind) -> OrderRequest {
    let request = OrderRequest {
        account: client.account,
        client_id: 1,
        market: BTC,
        side: Side::Buy,
        qty: f("10"),
        kind,
        reduce_only: false,
        auth: None,
    };
    let Command::PlaceOrder(request) = client.order(h.engine.next_request_nonce(client.account), request) else {
        unreachable!()
    };
    request
}

pub fn order_evidence(h: &Harness, request: &OrderRequest) -> AccessEvidence {
    let auth = request.auth.as_ref().unwrap();
    let hash = eip712::order_hash(&h.engine.access_domain().unwrap(), request, auth.nonce, auth.expires_at);
    evidence(h, hash, Some(request.account), None)
}

pub fn evidence(h: &Harness, hash: B256, account: Option<AccountId>, payer: Option<AccountId>) -> AccessEvidence {
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
    sign_screening(h, &mut screening);
    AccessEvidence::Screened(screening)
}

pub fn sign_screening(h: &Harness, screening: &mut Screening) {
    screening.signature =
        eip712::sign(&source(), &eip712::screening_hash(&h.engine.access_domain().unwrap(), screening));
}

pub fn deposit(
    h: &Harness,
    account: Option<AccountId>,
    payer: Option<AccountId>,
    reference: &str,
) -> (AccessDeposit, AccessEvidence) {
    let deposit = AccessDeposit { account, payer, amount: f("100000"), chain_ref: reference.into() };
    let evidence = evidence(h, eip712::deposit_screening_hash(&deposit), account, payer);
    (deposit, evidence)
}

pub fn fund(h: &mut Harness, client: &Client) {
    let (deposit, evidence) = deposit(h, Some(client.account), Some(client.account), "0xfunded:1");
    assert_eq!(
        h.cmd(Command::AccessDeposit { deposit, evidence }),
        [Event::Deposited { account: client.account, amount: f("100000") }]
    );
}
