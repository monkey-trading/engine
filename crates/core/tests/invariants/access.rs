//! Explicit signed clients for the current-version state-tree experiment.

use std::collections::BTreeMap;

use engine_core::Engine;
use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus,
    Screening,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, WithdrawalRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;

use crate::common::{Client, Harness, acct};
use crate::session::Session;

pub(crate) struct Clients {
    keys: BTreeMap<AccountId, Client>,
    accounts: BTreeMap<AccountId, AccountId>,
}

impl Clients {
    /// Funds known keys under the historical rules, then upgrades and records their declarations.
    pub(crate) fn prepare(mut h: Harness, session: &Session) -> (Harness, Self) {
        let mut keys = BTreeMap::new();
        let mut accounts = BTreeMap::new();
        for seed in [1, 2, 3, 9] {
            let client = Client::new(seed);
            accounts.insert(acct(seed), client.account);
            keys.insert(client.account, client);
        }
        for (seed, amount) in (1..).zip(session.funding) {
            h.deposit(accounts[&acct(seed)], &amount.to_string());
        }
        h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
            source: eip712::address(source().verifying_key()),
            max_age_ms: 1000,
        }));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
        for client in keys.values() {
            let expires_at = h.ts + 60_000;
            let class = EligibilityClass::Professional;
            let hash = eip712::eligibility_hash(&client.domain, client.account, class, 0, expires_at);
            let request = EligibilityRequest {
                account: client.account,
                class,
                auth: RequestAuth { nonce: 0, expires_at, signature: eip712::sign(&client.key, &hash) },
            };
            let evidence = evidence(&h.engine, h.ts, hash, Some(client.account), None);
            let events = h.cmd(Command::AttestEligibility { request, evidence });
            assert!(
                events.iter().any(|event| matches!(event, engine_core::event::Event::EligibilityAttested { .. })),
                "{events:?}"
            );
        }
        (h, Self { keys, accounts })
    }

    /// Maps generated account labels to the actual signing keys before resolving relative sizes.
    pub(crate) fn account(&self, label: AccountId) -> AccountId {
        self.accounts[&label]
    }

    /// Signs an explicit generated action and supplies fresh, authenticated synthetic evidence.
    pub(crate) fn sign(&self, command: Command, engine: &Engine, now: Millis) -> Command {
        match command {
            Command::PlaceOrder(mut request) => {
                let client = &self.keys[&request.account];
                let nonce = engine.next_request_nonce(client.account);
                let expires_at = now + 60_000;
                let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
                request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
                let evidence = evidence(engine, now, hash, Some(client.account), None);
                Command::AccessOrder { request, evidence }
            }
            Command::Withdraw { account, amount, .. } => {
                let client = &self.keys[&account];
                let nonce = engine.next_request_nonce(account);
                let expires_at = now + 60_000;
                let hash = eip712::withdrawal_hash(&client.domain, account, amount, nonce, expires_at);
                let request = WithdrawalRequest {
                    account,
                    amount,
                    auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) }),
                };
                Command::AccessWithdraw {
                    request,
                    evidence: evidence(engine, now, hash, Some(account), None),
                    payout_available: true,
                }
            }
            Command::CancelOrder { account, order_id, .. } => {
                let client = &self.keys[&account];
                let nonce = engine.next_request_nonce(account);
                let expires_at = now + 60_000;
                let hash = eip712::cancel_hash(&client.domain, account, order_id, nonce, expires_at);
                Command::CancelOrder {
                    account,
                    order_id,
                    auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) }),
                }
            }
            Command::Deposit { account, amount, chain_ref } => {
                let deposit = AccessDeposit { account: Some(account), payer: Some(account), amount, chain_ref };
                let hash = eip712::deposit_screening_hash(&deposit);
                let evidence = evidence(engine, now, hash, deposit.account, deposit.payer);
                Command::AccessDeposit { deposit, evidence }
            }
            Command::HouseCapital { amount, chain_ref } => {
                let deposit = AccessDeposit { account: None, payer: Some(self.account(acct(1))), amount, chain_ref };
                let hash = eip712::deposit_screening_hash(&deposit);
                let evidence = evidence(engine, now, hash, None, deposit.payer);
                Command::AccessDeposit { deposit, evidence }
            }
            other => other,
        }
    }
}

/// The same published synthetic authority as the access protocol tests, never a live provider.
fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).unwrap()
}

fn evidence(
    engine: &Engine,
    now: Millis,
    request_hash: B256,
    account: Option<AccountId>,
    payer: Option<AccountId>,
) -> AccessEvidence {
    let mut screening = Screening {
        source: eip712::address(source().verifying_key()),
        request_hash,
        observed_at: now,
        account,
        payer,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    screening.signature =
        eip712::sign(&source(), &eip712::screening_hash(&engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}
