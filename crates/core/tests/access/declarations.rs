use crate::common::{CHAIN_ID, Client, VAULT};
use crate::support::*;
use engine_core::access::{AccessReject, EligibilityClass};
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::types::AccountId;

#[test]
fn each_named_eligibility_class_requires_a_signature_even_in_unsigned_dev_logs() {
    for class in [EligibilityClass::Professional, EligibilityClass::EligibleClient] {
        let (mut h, client) = setup();
        let request = declaration(&h, &client, class);
        assert!(h.engine.accounts().is_empty(), "declaration precedes funding");
        assert_eq!(declare(&mut h, request.clone()), [Event::EligibilityAttested { account: client.account, class }]);
        assert_eq!(h.engine.eligibility(client.account), Some(class));
        assert_eq!(h.engine.next_request_nonce(client.account), 1);
        assert_eq!(
            declare(&mut h, request),
            [Event::EligibilityRejected { account: client.account, reason: Reject::InvalidNonce { expected: 1 } }]
        );
        let again = declaration(&h, &client, class);
        assert_eq!(
            declare(&mut h, again),
            [Event::EligibilityRejected {
                account: client.account,
                reason: Reject::Access { reason: AccessReject::AlreadyAttested }
            }]
        );
        assert_eq!(h.engine.next_request_nonce(client.account), 2);
    }
}

#[test]
fn declaration_signatures_bind_the_exact_class_account_chain_vault_nonce_and_expiry() {
    for changed in 0..6 {
        let (mut h, client) = setup();
        let mut request = declaration(&h, &client, EligibilityClass::Professional);
        match changed {
            0 => request.class = EligibilityClass::EligibleClient,
            1 => request.account = Client::new(2).account,
            2 | 3 => {
                let elsewhere = Client {
                    domain: eip712::domain(
                        CHAIN_ID + u64::from(changed == 2),
                        if changed == 3 { AccountId([9; 20]) } else { VAULT },
                    ),
                    ..Client::new(1)
                };
                request = declaration(&h, &elsewhere, EligibilityClass::Professional);
            }
            4 => request.auth.nonce += 1,
            5 => request.auth.expires_at += 1,
            _ => unreachable!(),
        }
        assert_eq!(
            declare(&mut h, request.clone()),
            [Event::EligibilityRejected { account: request.account, reason: Reject::InvalidSignature }],
            "case {changed}"
        );
        assert_eq!(h.engine.eligibility(client.account), None);
        assert_eq!(h.engine.next_request_nonce(client.account), 0);
    }
}

#[test]
fn an_authentic_expired_declaration_burns_its_nonce_without_declaring_eligibility() {
    let (mut h, client) = setup();
    let mut request = declaration(&h, &client, EligibilityClass::Professional);
    request.auth.expires_at = h.ts - 1;
    request.auth.signature = eip712::sign(
        &client.key,
        &eip712::eligibility_hash(
            &client.domain,
            client.account,
            request.class,
            request.auth.nonce,
            request.auth.expires_at,
        ),
    );
    assert_eq!(
        declare(&mut h, request),
        [Event::EligibilityRejected { account: client.account, reason: Reject::Expired { expires_at: h.ts - 1 } }]
    );
    assert_eq!(h.engine.eligibility(client.account), None);
    assert_eq!(h.engine.next_request_nonce(client.account), 1);
}
