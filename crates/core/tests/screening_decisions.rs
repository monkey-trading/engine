//! `V85`'s screening decisions, through `Engine::apply`. A client request's evidence is the source's
//! decision for its account, with no `request_hash`, so one decision backs each of the account's
//! requests (a declaration, orders, a close-all, levels, a withdrawal, and a payout reissue's check)
//! while its `observed_at` is within the recorded bound at the sequenced time; a decision naming the
//! request itself still backs it. A decision naming another request, one for another account, one past
//! its bound or stamped too far ahead, one from another source key, and one with the request it was
//! signed for taken out are refused. A refused decision refuses each request while it is fresh, a
//! deposit's evidence still names its deposit, and before `V85` a decision backs only the request it
//! names.

mod common;

use engine_core::access::{
    AccessDeposit, AccessEvidence, AccessPolicy, AccessReject, EligibilityClass, EligibilityRequest, Jurisdiction,
    MAX_EVIDENCE_LEAD_MS, SanctionsStatus, Screening,
};
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::{
    AdminCommand, CloseAllRequest, CloseEntry, Command, OrderKind, OrderRequest, PositionLevelsRequest,
    WithdrawalRequest,
};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis, Side};
use k256::ecdsa::SigningKey;

use crate::common::close_spread::{ceilings, fund, source, source_key};
use crate::common::{BTC, CHAIN_ID, Client, Harness, VAULT, f};

/// The freshness bound the log records.
const MAX_AGE_MS: Millis = 30_000;

/// The screened client requests, each signed at the account's next nonce.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Order,
    Levels,
    CloseAll,
    Withdrawal,
}

/// A refusal case: what it is, its evidence given the harness, the account and the digest of the
/// request it is sequenced with, and the refusal it meets.
type Case<'a> = (&'a str, Box<dyn Fn(&Harness, AccountId, B256) -> AccessEvidence + 'a>, AccessReject);

/// A signed client request, waiting for its evidence.
enum Signed {
    Declaration(EligibilityRequest),
    Order(OrderRequest),
    Levels(PositionLevelsRequest),
    CloseAll(CloseAllRequest),
    Withdrawal(WithdrawalRequest),
}

#[test]
fn one_decision_backs_each_of_the_accounts_requests_while_it_is_fresh() {
    let mut h = screened_at(ProtocolVersion::V85);
    let client = Client::new(1);
    let decision = clear(&h, client.account);
    let (declaration, _) = declaration(&h, &client);
    assert_eq!(
        h.cmd(declaration.with(decision.clone())),
        [Event::EligibilityAttested { account: client.account, class: EligibilityClass::Professional }]
    );
    fund(&mut h, &client, "100000");
    // Each request, at its own nonce and a second apart, sequenced with the one decision; then a
    // withdrawal at the decision's last millisecond of freshness.
    for action in [Action::Order, Action::Levels, Action::CloseAll] {
        h.advance(1_000);
        h.price(BTC, "100000");
        let events = h.cmd(sign(&h, &client, action).0.with(decision.clone()));
        assert_accepted(action, &events);
    }
    let observed_at = h.ts - 3_000;
    h.advance_to(observed_at + MAX_AGE_MS);
    let events = h.cmd(sign(&h, &client, Action::Withdrawal).0.with(decision.clone()));
    assert_accepted(Action::Withdrawal, &events);
    assert!(h.engine.client_screening_refusal(client.account, B256::ZERO, &decision, h.ts).is_none());

    // A millisecond later the same decision is stale for every request, and the reissue check.
    h.advance(1);
    for action in [Action::Order, Action::Withdrawal] {
        let events = h.cmd(sign(&h, &client, action).0.with(decision.clone()));
        assert_eq!(refusal(&events), Some(AccessReject::EvidenceStale), "{action:?}: {events:?}");
    }
    let stale = h.engine.client_screening_refusal(client.account, B256::ZERO, &decision, h.ts);
    assert_eq!(stale, Some(AccessReject::EvidenceStale));
}

#[test]
fn a_client_request_refuses_every_decision_but_a_fresh_signed_one_for_its_account() {
    let earlier_key = SigningKey::from_slice(&[0x78; 32]).unwrap();
    let earlier_source = eip712::address(earlier_key.verifying_key());
    let other = Client::new(2);
    // Each case's evidence, given the digest of the request it is sequenced with.
    let cases: [Case; 7] = [
        (
            "another request's digest",
            Box::new(|h: &Harness, account: AccountId, hash: B256| {
                let other = B256::from(hash.0.map(|byte| byte ^ 0xff));
                screened(h, Screening { request_hash: Some(other), ..decision_for(h, account) })
            }),
            AccessReject::EvidenceMismatch,
        ),
        (
            "the request taken out of a decision signed for it",
            Box::new(|h: &Harness, account: AccountId, hash: B256| {
                let named = signed(h, Screening { request_hash: Some(hash), ..decision_for(h, account) });
                AccessEvidence::Screened(Screening { request_hash: None, ..named })
            }),
            AccessReject::SourceInvalid,
        ),
        (
            "another account's decision",
            Box::new(|h: &Harness, _: AccountId, _: B256| clear(h, other.account)),
            AccessReject::EvidenceMismatch,
        ),
        (
            "a payer named, as a deposit's decision does",
            Box::new(|h: &Harness, account: AccountId, _: B256| {
                screened(h, Screening { payer: Some(account), ..decision_for(h, account) })
            }),
            AccessReject::EvidenceMismatch,
        ),
        (
            "stamped past the lead the sequenced time allows",
            Box::new(|h: &Harness, account: AccountId, _: B256| {
                screened(h, Screening { observed_at: h.ts + MAX_EVIDENCE_LEAD_MS + 1, ..decision_for(h, account) })
            }),
            AccessReject::EvidenceStale,
        ),
        (
            "an earlier source key under its own address",
            Box::new(|h: &Harness, account: AccountId, _: B256| {
                signed_by(h, &earlier_key, Screening { source: earlier_source, ..decision_for(h, account) })
            }),
            AccessReject::EvidenceMismatch,
        ),
        (
            "an earlier source key under the recorded address",
            Box::new(|h: &Harness, account: AccountId, _: B256| signed_by(h, &earlier_key, decision_for(h, account))),
            AccessReject::SourceInvalid,
        ),
    ];
    for action in [Action::Order, Action::Levels, Action::CloseAll, Action::Withdrawal] {
        let (mut h, client) = declared(ProtocolVersion::V85);
        for (case, evidence, reason) in &cases {
            let (request, hash) = sign(&h, &client, action);
            let evidence = evidence(&h, client.account, hash);
            let refused = h.engine.client_screening_refusal(client.account, hash, &evidence, h.ts);
            assert_eq!(refused, Some(*reason), "{action:?}, {case}");
            let nonce = h.engine.next_request_nonce(client.account);
            let events = h.cmd(request.with(evidence));
            assert_eq!(refusal(&events), Some(*reason), "{action:?}, {case}: {events:?}");
            assert_eq!(h.engine.next_request_nonce(client.account), nonce + 1, "a refusal uses its nonce");
        }
        // The request with its account's fresh decision passes screening, and so does one with a decision
        // naming the request itself, as before `V85`.
        let events = h.cmd(sign(&h, &client, action).0.with(clear(&h, client.account)));
        assert_accepted(action, &events);
        if action == Action::Order {
            let (request, hash) = sign(&h, &client, action);
            let named = screened(&h, Screening { request_hash: Some(hash), ..decision_for(&h, client.account) });
            assert!(h.engine.client_screening_refusal(client.account, hash, &named, h.ts).is_none());
            assert_accepted(action, &h.cmd(request.with(named)));
        }
    }
}

#[test]
fn a_refused_decision_refuses_each_request_it_backs() {
    let (mut h, client) = declared(ProtocolVersion::V85);
    let decision = decision_for(&h, client.account);
    for (refused, reason) in [
        (Screening { jurisdiction: Jurisdiction::UnitedStates, ..decision.clone() }, AccessReject::UnitedStates),
        (
            Screening { jurisdiction: Jurisdiction::Sanctioned, ..decision.clone() },
            AccessReject::SanctionedJurisdiction,
        ),
        (
            Screening { account_status: SanctionsStatus::Sanctioned, ..decision.clone() },
            AccessReject::AccountSanctioned,
        ),
        (Screening { account_status: SanctionsStatus::Unknown, ..decision.clone() }, AccessReject::AccountUnknown),
    ] {
        let refused = screened(&h, refused);
        for action in [Action::Order, Action::Withdrawal, Action::Order] {
            let events = h.cmd(sign(&h, &client, action).0.with(refused.clone()));
            assert_eq!(refusal(&events), Some(reason), "{action:?}: {events:?}");
        }
    }
    // A refusal leaves nothing behind: the account's next clear decision backs its next request.
    let events = h.cmd(sign(&h, &client, Action::Order).0.with(clear(&h, client.account)));
    assert_accepted(Action::Order, &events);
}

#[test]
fn a_deposit_still_needs_a_decision_naming_its_input() {
    let (mut h, client) = declared(ProtocolVersion::V85);
    let deposit = AccessDeposit {
        account: Some(client.account),
        payer: Some(client.account),
        amount: f("1000"),
        chain_ref: "0xdeposit:1".into(),
    };
    let unbound = screened(&h, Screening { payer: Some(client.account), ..decision_for(&h, client.account) });
    let events = h.cmd(Command::AccessDeposit { deposit: deposit.clone(), evidence: unbound });
    let pending = Event::DepositPending {
        chain_ref: deposit.chain_ref.clone(),
        account: deposit.account,
        payer: deposit.payer,
        amount: deposit.amount,
        reason: AccessReject::EvidenceMismatch,
    };
    assert_eq!(events, [pending]);
    let named = Screening {
        request_hash: Some(eip712::deposit_screening_hash(&deposit)),
        payer: Some(client.account),
        ..decision_for(&h, client.account)
    };
    let events = h.cmd(Command::RetryDepositScreening { chain_ref: deposit.chain_ref, evidence: screened(&h, named) });
    assert_eq!(events, [Event::Deposited { account: client.account, amount: f("1000") }]);
}

#[test]
fn before_v85_a_decision_backs_only_the_request_it_names() {
    let (mut h, client) = declared(ProtocolVersion::V84);
    let (request, hash) = sign(&h, &client, Action::Order);
    let named = screened(&h, Screening { request_hash: Some(hash), ..decision_for(&h, client.account) });
    let events = h.cmd(request.with(named.clone()));
    assert_accepted(Action::Order, &events);
    // The same decision for the account's next request, and a decision naming none.
    for evidence in [named, clear(&h, client.account)] {
        let events = h.cmd(sign(&h, &client, Action::Order).0.with(evidence.clone()));
        assert_eq!(refusal(&events), Some(AccessReject::EvidenceMismatch), "{events:?}");
        let refused = h.engine.client_screening_refusal(client.account, B256::ZERO, &evidence, h.ts);
        assert_eq!(refused, Some(AccessReject::EvidenceMismatch));
    }
}

// ── Helpers ──

impl Signed {
    /// The request as the command that carries `evidence`.
    fn with(self, evidence: AccessEvidence) -> Command {
        match self {
            Signed::Declaration(request) => Command::AttestEligibility { request, evidence },
            Signed::Order(request) => Command::AccessOrder { request, evidence },
            Signed::Levels(request) => Command::AccessSetPositionLevels { request, evidence },
            Signed::CloseAll(request) => Command::AccessCloseAll { request, evidence },
            Signed::Withdrawal(request) => Command::AccessWithdraw { request, evidence, payout_available: true },
        }
    }
}

/// A signed, operator-recorded log under `version` with access screening on, ceilings set, and BTC
/// priced at 100000.
fn screened_at(version: ProtocolVersion) -> Harness {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() }));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: MAX_AGE_MS }));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.price(BTC, "100000");
    h
}

/// `screened_at(version)` with client 1 declared, funded with 100000 and holding 0.1 BTC long.
fn declared(version: ProtocolVersion) -> (Harness, Client) {
    let mut h = screened_at(version);
    let client = Client::new(1);
    let (declaration, hash) = declaration(&h, &client);
    let evidence = if version.binds_screening_to_request() {
        screened(&h, Screening { request_hash: Some(hash), ..decision_for(&h, client.account) })
    } else {
        clear(&h, client.account)
    };
    assert!(matches!(h.cmd(declaration.with(evidence))[..], [Event::EligibilityAttested { .. }]));
    fund(&mut h, &client, "100000");
    let (order, hash) = sign(&h, &client, Action::Order);
    let evidence = screened(&h, Screening { request_hash: Some(hash), ..decision_for(&h, client.account) });
    let evidence = if version.binds_screening_to_request() { evidence } else { clear(&h, client.account) };
    assert_accepted(Action::Order, &h.cmd(order.with(evidence)));
    (h, client)
}

/// The client's professional declaration at its next nonce, and the digest it signs.
fn declaration(h: &Harness, client: &Client) -> (Signed, B256) {
    let (nonce, expires_at) = (h.engine.next_request_nonce(client.account), h.ts + 60_000);
    let class = EligibilityClass::Professional;
    let hash = eip712::eligibility_hash(&client.domain, client.account, class, nonce, expires_at);
    let auth = RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) };
    (Signed::Declaration(EligibilityRequest { account: client.account, class, auth }), hash)
}

/// The client's `action` at its next nonce, and the digest it signs: a 0.1 BTC market buy, levels
/// around 100000 on its BTC position, a close-all of BTC, or a withdrawal of 10.
fn sign(h: &Harness, client: &Client, action: Action) -> (Signed, B256) {
    let (nonce, expires_at) = (h.engine.next_request_nonce(client.account), h.ts + 60_000);
    let auth = |hash| Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) });
    let account = client.account;
    match action {
        Action::Order => {
            let mut request = OrderRequest {
                account,
                client_id: 7,
                market: BTC,
                side: Side::Buy,
                qty: f("0.1"),
                kind: OrderKind::Market { quoted: f("100000"), max_slippage_bps: None },
                reduce_only: false,
                auth: None,
                take_profit: None,
                stop_loss: None,
            };
            let hash = eip712::order_hash(&client.domain, &request, nonce, expires_at);
            request.auth = auth(hash);
            (Signed::Order(request), hash)
        }
        Action::Levels => {
            let (take_profit, stop_loss) = (Some(f("150000")), Some(f("50000")));
            let mut request = PositionLevelsRequest { account, market: BTC, take_profit, stop_loss, auth: None };
            let hash = eip712::position_levels_hash(&client.domain, &request, nonce, expires_at);
            request.auth = auth(hash);
            (Signed::Levels(request), hash)
        }
        Action::CloseAll => {
            let closes = vec![CloseEntry { market: BTC, quoted: f("100000") }];
            let mut request = CloseAllRequest { account, closes, max_slippage_bps: Some(10_000), auth: None };
            let hash = eip712::close_all_hash(&client.domain, &request, nonce, expires_at);
            request.auth = auth(hash);
            (Signed::CloseAll(request), hash)
        }
        Action::Withdrawal => {
            let hash = eip712::withdrawal_hash(&client.domain, account, f("10"), nonce, expires_at);
            (Signed::Withdrawal(WithdrawalRequest { account, amount: f("10"), auth: auth(hash) }), hash)
        }
    }
}

/// The source's allowed and clear decision for `account`, observed now and naming no request,
/// unsigned.
fn decision_for(h: &Harness, account: AccountId) -> Screening {
    Screening {
        source: source_key(),
        request_hash: None,
        observed_at: h.ts,
        account: Some(account),
        payer: None,
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    }
}

/// `decision_for(account)`, signed by the recorded source.
fn clear(h: &Harness, account: AccountId) -> AccessEvidence {
    screened(h, decision_for(h, account))
}

/// `screening` signed by the recorded source.
fn screened(h: &Harness, screening: Screening) -> AccessEvidence {
    signed_by(h, &source(), screening)
}

/// `screening` signed by the recorded source, as a decision.
fn signed(h: &Harness, screening: Screening) -> Screening {
    let AccessEvidence::Screened(screening) = screened(h, screening) else { unreachable!() };
    screening
}

/// `screening` signed by `key` under the log's domain.
fn signed_by(h: &Harness, key: &SigningKey, mut screening: Screening) -> AccessEvidence {
    screening.signature = eip712::sign(key, &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}

/// The access refusal `events` hold, if any.
fn refusal(events: &[Event]) -> Option<AccessReject> {
    events.iter().find_map(|event| match event {
        Event::OrderRejected { reason: Reject::Access { reason }, .. }
        | Event::WithdrawalRejected { reason: Reject::Access { reason }, .. }
        | Event::EligibilityRejected { reason: Reject::Access { reason }, .. } => Some(*reason),
        _ => None,
    })
}

/// Fails unless `events` hold what `action` does once screening passes: a fill for an order or a
/// close-all, the levels set, or the withdrawal approved.
fn assert_accepted(action: Action, events: &[Event]) {
    let done = events.iter().any(|event| match action {
        Action::Order | Action::CloseAll => matches!(event, Event::Fill(_)),
        Action::Levels => matches!(event, Event::PositionLevelsSet { .. }),
        Action::Withdrawal => matches!(event, Event::WithdrawalApproved { .. }),
    });
    assert!(done, "{action:?}: {events:?}");
}
