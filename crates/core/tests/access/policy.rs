use crate::common::{BTC, Harness, acct, f};
use crate::support::*;
use engine_core::access::{
    AccessEvidence, AccessPolicy, AccessReject, EligibilityClass, Jurisdiction, MAX_EVIDENCE_LEAD_MS, SanctionsStatus,
    SourceFailure,
};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, WithdrawalRequest};
use engine_core::eip712::{self, B256, Signature};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};

/// The client actions the API screens before sequencing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Order,
    Withdrawal,
    Declaration,
}

#[test]
fn every_excluding_or_unknown_api_disposition_refuses_every_screened_client_action_and_burns_the_nonce() {
    let cases = [
        AccessReject::UnitedStates,
        AccessReject::SanctionedJurisdiction,
        AccessReject::JurisdictionUnknown,
        AccessReject::AccountSanctioned,
        AccessReject::AccountUnknown,
        AccessReject::SourceInvalid,
        AccessReject::EvidenceMismatch,
        AccessReject::EvidenceStale,
        AccessReject::SourceNotConfigured,
        AccessReject::SourceUnavailable,
        AccessReject::PeerUnknown,
    ];
    for expected in cases {
        for action in [Action::Order, Action::Withdrawal, Action::Declaration] {
            let (mut h, client) = setup();
            if action != Action::Declaration {
                attest(&mut h, &client);
                fund(&mut h, &client);
            }
            let nonce = h.engine.next_request_nonce(client.account);
            let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
            let declared = declaration(&h, &client, EligibilityClass::Professional);
            let mut evidence = match action {
                Action::Order => order_evidence(&h, &request),
                Action::Withdrawal => evidence(
                    &h,
                    eip712::withdrawal_hash(&client.domain, client.account, f("1"), nonce, client.expires_at),
                    Some(client.account),
                    None,
                ),
                Action::Declaration => declaration_evidence(&h, &declared),
            };
            let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
            match expected {
                AccessReject::UnitedStates => screening.jurisdiction = Jurisdiction::UnitedStates,
                AccessReject::SanctionedJurisdiction => screening.jurisdiction = Jurisdiction::Sanctioned,
                AccessReject::JurisdictionUnknown => screening.jurisdiction = Jurisdiction::Unknown,
                AccessReject::AccountSanctioned => screening.account_status = SanctionsStatus::Sanctioned,
                AccessReject::AccountUnknown => screening.account_status = SanctionsStatus::Unknown,
                AccessReject::EvidenceMismatch => screening.account = Some(AccountId([0xff; 20])),
                AccessReject::EvidenceStale => screening.observed_at -= 1_001,
                _ => {}
            }
            sign_screening(&h, screening);
            if expected == AccessReject::SourceInvalid {
                screening.signature = Signature::ZERO;
            }
            evidence = match expected {
                AccessReject::SourceNotConfigured => AccessEvidence::Failed { reason: SourceFailure::NotConfigured },
                AccessReject::SourceUnavailable => AccessEvidence::Failed { reason: SourceFailure::Unavailable },
                AccessReject::PeerUnknown => AccessEvidence::Failed { reason: SourceFailure::MissingPeer },
                _ => evidence,
            };
            let command = match action {
                Action::Order => Command::AccessOrder { request, evidence },
                Action::Withdrawal => {
                    let Command::Withdraw { account, amount, auth } = client.withdraw(nonce, "1") else {
                        unreachable!()
                    };
                    Command::AccessWithdraw {
                        request: WithdrawalRequest { account, amount, auth },
                        evidence,
                        payout_available: false,
                    }
                }
                Action::Declaration => Command::AttestEligibility { request: declared, evidence },
            };
            let events = h.cmd(command.clone());
            let reason = Reject::Access { reason: expected };
            let expected_event = match action {
                Action::Order => Event::OrderRejected { account: client.account, client_id: 1, reason },
                Action::Withdrawal => Event::WithdrawalRejected { account: client.account, amount: f("1"), reason },
                Action::Declaration => Event::EligibilityRejected { account: client.account, reason },
            };
            assert_eq!(events, [expected_event], "{expected:?} {action:?}");
            assert_eq!(h.engine.next_request_nonce(client.account), nonce + 1);
            if action == Action::Declaration {
                assert_eq!(h.engine.eligibility(client.account), None, "{expected:?}");
            } else {
                assert_eq!(h.engine.accounts()[&client.account].balance, f("100000"));
                assert!(h.engine.accounts()[&client.account].positions.is_empty());
            }
            let replayed = h.cmd(command);
            assert!(
                replayed.iter().any(|event| matches!(
                    event,
                    Event::OrderRejected { reason: Reject::InvalidNonce { expected }, .. }
                        | Event::WithdrawalRejected { reason: Reject::InvalidNonce { expected }, .. }
                        | Event::EligibilityRejected { reason: Reject::InvalidNonce { expected }, .. }
                        if *expected == nonce + 1
                )),
                "{replayed:?}"
            );
            if action == Action::Declaration {
                // A refusal records nothing, so a later declaration with verified evidence still can.
                attest(&mut h, &client);
            }
        }
    }
}

#[test]
fn future_or_action_mismatched_source_evidence_is_never_accepted() {
    for (changed, declaration_action) in (0..5).flat_map(|changed| [(changed, false), (changed, true)]) {
        let (mut h, client) = setup();
        if !declaration_action {
            attest(&mut h, &client);
            fund(&mut h, &client);
        }
        let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
        let declared = declaration(&h, &client, EligibilityClass::Professional);
        let (mut evidence, other) = if declaration_action {
            (declaration_evidence(&h, &declared), order_evidence(&h, &request))
        } else {
            (order_evidence(&h, &request), declaration_evidence(&h, &declared))
        };
        let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
        match changed {
            0 => screening.observed_at += MAX_EVIDENCE_LEAD_MS + 1,
            1 => screening.request_hash = B256::repeat_byte(1),
            2 => screening.payer = Some(client.account),
            3 => screening.source = client.account,
            // Evidence for the account's other signed action at the same nonce.
            4 => evidence = other,
            _ => unreachable!(),
        }
        if let AccessEvidence::Screened(screening) = &mut evidence {
            sign_screening(&h, screening);
        }
        let reason = Reject::Access {
            reason: if changed == 0 { AccessReject::EvidenceStale } else { AccessReject::EvidenceMismatch },
        };
        if declaration_action {
            assert_eq!(
                h.cmd(Command::AttestEligibility { request: declared, evidence }),
                [Event::EligibilityRejected { account: client.account, reason }],
                "case {changed}"
            );
            assert_eq!(h.engine.eligibility(client.account), None);
        } else {
            assert_eq!(
                h.cmd(Command::AccessOrder { request, evidence }),
                [Event::OrderRejected { account: client.account, client_id: 1, reason }],
                "case {changed}"
            );
        }
    }
}

#[test]
fn a_declaration_is_required_before_the_first_order_and_legacy_commands_cannot_skip_screening() {
    let (mut h, client) = setup();
    let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
    let evidence = order_evidence(&h, &request);
    assert_eq!(
        h.cmd(Command::AccessOrder { request, evidence }),
        [Event::OrderRejected {
            account: client.account,
            client_id: 1,
            reason: Reject::Access { reason: AccessReject::EligibilityRequired }
        }]
    );
    attest(&mut h, &client);
    fund(&mut h, &client);
    let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
    assert_eq!(
        h.cmd(Command::PlaceOrder(request)),
        [Event::OrderRejected {
            account: client.account,
            client_id: 1,
            reason: Reject::Access { reason: AccessReject::EvidenceMissing }
        }]
    );
    let command = client.withdraw(h.engine.next_request_nonce(client.account), "1");
    assert_eq!(
        h.cmd(command),
        [Event::WithdrawalRejected {
            account: client.account,
            amount: f("1"),
            reason: Reject::Access { reason: AccessReject::EvidenceMissing }
        }]
    );
}

#[test]
fn placement_screening_covers_resting_orders_for_life_including_orders_resting_from_before_v24() {
    // An order resting from before V24, placed under the unsigned rules in force then.
    let mut h = Harness::v8(ClientAuth::Unsigned);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    h.price(BTC, "100");
    let legacy = acct(1);
    h.deposit(legacy, "100000");
    let placed = h.order(legacy, BTC, Side::Buy, "10", OrderKind::Limit { price: f("90") });
    assert!(placed.iter().any(|event| matches!(event, Event::OrderAccepted { .. })), "{placed:?}");
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy {
        source: eip712::address(source().verifying_key()),
        max_age_ms: 1_000,
    }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V24 });
    // A V24 order screened at placement keeps resting past the freshness bound.
    let client = crate::common::Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client);
    let request = order(&h, &client, OrderKind::Limit { price: f("90") });
    let evidence = order_evidence(&h, &request);
    assert!(h.cmd(Command::AccessOrder { request, evidence }).iter().any(|e| matches!(e, Event::OrderAccepted { .. })));
    h.ts += 1_001;
    let events = h.price(BTC, "89");
    for account in [legacy, client.account] {
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Fill(fill) if fill.account == account && fill.qty == f("10"))),
            "{account:?}: {events:?}"
        );
        assert!(h.engine.accounts()[&account].orders.is_empty());
    }
    assert!(!events.iter().any(|event| matches!(event, Event::OrderCancelled { .. })), "{events:?}");
}

#[test]
fn evidence_stamped_up_to_the_lead_bound_after_the_sequenced_time_is_accepted() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    fund(&mut h, &client);
    let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
    let mut evidence = order_evidence(&h, &request);
    let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
    screening.observed_at += MAX_EVIDENCE_LEAD_MS;
    sign_screening(&h, screening);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.qty == f("10"))), "{events:?}");
}

#[test]
fn evidence_at_the_recorded_freshness_boundary_is_accepted() {
    let (mut h, client) = setup();
    attest(&mut h, &client);
    fund(&mut h, &client);
    let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
    let mut evidence = order_evidence(&h, &request);
    let AccessEvidence::Screened(screening) = &mut evidence else { unreachable!() };
    screening.observed_at -= 1_000;
    sign_screening(&h, screening);
    let events = h.cmd(Command::AccessOrder { request, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.qty == f("10"))), "{events:?}");
}

#[test]
fn every_typed_source_failure_refuses_an_authenticated_first_order_without_funding_or_declaring_it() {
    for (failure, reason) in [
        (SourceFailure::InvalidResponse, AccessReject::SourceInvalid),
        (SourceFailure::Unavailable, AccessReject::SourceUnavailable),
        (SourceFailure::NotConfigured, AccessReject::SourceNotConfigured),
        (SourceFailure::MissingPeer, AccessReject::PeerUnknown),
    ] {
        let (mut h, client) = setup();
        let request = order(&h, &client, OrderKind::Market { quoted: f("100"), max_slippage_bps: None });
        let events = h.cmd(Command::AccessOrder { request, evidence: AccessEvidence::Failed { reason: failure } });
        assert_eq!(
            events,
            [Event::OrderRejected { account: client.account, client_id: 1, reason: Reject::Access { reason } }]
        );
        assert_eq!(h.engine.next_request_nonce(client.account), 1);
        assert!(h.engine.accounts().is_empty());
        assert_eq!(h.engine.eligibility(client.account), None);
    }
}

#[test]
fn the_recorded_access_policy_cannot_be_replaced_or_recorded_invalid() {
    let (mut h, _) = setup();
    let recorded = h.engine.access_policy().cloned().unwrap();
    let invalid = "access policy needs a nonzero source and positive freshness bound";
    for (policy, reason) in [
        (AccessPolicy { source: AccountId([0x99; 20]), max_age_ms: 5_000 }, "the access policy is already recorded"),
        (AccessPolicy { source: AccountId([0; 20]), ..recorded.clone() }, invalid),
        (AccessPolicy { max_age_ms: 0, ..recorded.clone() }, invalid),
        (AccessPolicy { max_age_ms: -1, ..recorded.clone() }, invalid),
    ] {
        let events = h.cmd(Command::Admin(AdminCommand::SetAccessPolicy(policy)));
        assert_eq!(events, [Event::AdminRejected { reason: reason.into() }]);
        assert_eq!(h.engine.access_policy(), Some(&recorded));
    }
}
