//! Explicit signed inputs for current-protocol financial scenarios.

use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus,
    Screening,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{Command, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use k256::ecdsa::SigningKey;

pub fn key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).unwrap()
}

pub fn account(seed: u8) -> AccountId {
    eip712::address(key(seed).verifying_key())
}

pub fn policy() -> AccessPolicy {
    AccessPolicy { source: account(0x77), max_age_ms: 60_000 }
}

pub fn declaration(engine: &Engine, seed: u8) -> Command {
    let account = account(seed);
    let nonce = engine.next_request_nonce(account);
    let expires_at = engine.now() + 60_000;
    let class = EligibilityClass::Professional;
    let hash = eip712::eligibility_hash(&engine.access_domain().unwrap(), account, class, nonce, expires_at);
    let request = EligibilityRequest {
        account,
        class,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&key(seed), &hash) },
    };
    Command::AttestEligibility { request, evidence: proof(engine, hash, Some(account), None) }
}

pub fn funding(
    engine: &Engine,
    account: Option<AccountId>,
    payer: Option<AccountId>,
    amount: Fixed,
    reference: &str,
) -> Command {
    let deposit = AccessDeposit { account, payer, amount, chain_ref: reference.into() };
    let evidence = proof(engine, eip712::deposit_screening_hash(&deposit), account, payer);
    Command::AccessDeposit { deposit, evidence }
}

pub fn order(engine: &Engine, seed: u8, mut request: OrderRequest) -> Command {
    assert_eq!(request.account, account(seed));
    let nonce = engine.next_request_nonce(request.account);
    let expires_at = engine.now() + 60_000;
    let hash = eip712::order_hash(&engine.access_domain().unwrap(), &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&key(seed), &hash) });
    let evidence = proof(engine, hash, Some(request.account), None);
    Command::AccessOrder { request, evidence }
}

pub fn withdrawal(engine: &Engine, seed: u8, amount: Fixed, payout_available: bool) -> Command {
    let account = account(seed);
    let nonce = engine.next_request_nonce(account);
    let expires_at = engine.now() + 60_000;
    let hash = eip712::withdrawal_hash(&engine.access_domain().unwrap(), account, amount, nonce, expires_at);
    let request = WithdrawalRequest {
        account,
        amount,
        auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&key(seed), &hash) }),
    };
    let evidence = proof(engine, hash, Some(account), None);
    Command::AccessWithdraw { request, evidence, payout_available }
}

pub fn proof(
    engine: &Engine,
    request_hash: B256,
    account: Option<AccountId>,
    payer: Option<AccountId>,
) -> AccessEvidence {
    let mut screening = Screening {
        source: self::account(0x77),
        request_hash,
        observed_at: engine.now(),
        account,
        payer,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    screening.signature =
        eip712::sign(&key(0x77), &eip712::screening_hash(&engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}
