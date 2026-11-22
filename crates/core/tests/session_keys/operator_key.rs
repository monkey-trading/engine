//! From `V75`, an address an account registered as its session key before the address became the
//! operator key in effect signs nothing as that account's session key while it is the operator's: the
//! operator's sequence is the admin channel's. `V74` let it trade for the account on that sequence. From
//! `V88` the operator key in effect signs no client request for its own address either, a declaration
//! included.

use engine_core::access::{EligibilityClass, EligibilityRequest};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;

use crate::common::close_spread::evidence;
use crate::common::events::{fills, rejection};
use crate::common::session_keys::{HOUR_MS, buy, expiring_trade, funded_at, idle_until, register_expiring};
use crate::common::{BTC, Client, Harness};

/// The key the operator rotates to.
const NEXT_OPERATOR: u8 = 0x78;

/// Rotates the operator to `NEXT_OPERATOR`, has `client` register that address as its session key an
/// hour before the rotation takes effect, and sequences the first command after it does.
fn rotate_onto_a_registered_session_key(h: &mut Harness, client: &Client) {
    let next = Client::new(NEXT_OPERATOR);
    h.operator_admin(AdminCommand::RotateOperator { address: next.account });
    let effective_at = h.ts + NOTICE_MS;
    idle_until(h, effective_at - HOUR_MS);
    assert_ne!(h.engine.operator().unwrap().address, next.account, "the rotation is still pending");
    let expires_at: Millis = effective_at + 2 * HOUR_MS;
    let events = h.cmd(register_expiring(h, client, &next, expires_at, h.ts + 60_000));
    assert_eq!(
        events,
        [Event::SessionKeyRegistered { account: client.account, session_key: next.account, expires_at }],
        "the next operator's address registers as a session key while the rotation waits"
    );
    idle_until(h, effective_at + 60_000);
    assert_eq!(h.engine.operator().unwrap().address, next.account, "the rotation took effect");
    assert!(h.ts < expires_at, "the registration is unexpired");
}

#[test]
fn a_session_key_that_became_the_operators_key_signs_nothing_for_the_account() {
    for version in [ProtocolVersion::V74, ProtocolVersion::V75] {
        let (mut h, client, _) = funded_at(version);
        rotate_onto_a_registered_session_key(&mut h, &client);
        let operator = Client { expires_at: h.ts + 60_000, ..Client::new(NEXT_OPERATOR) };
        let admin_nonce = h.engine.next_request_nonce(operator.account);
        let signed_before = operator.admin(admin_nonce, AdminCommand::SetStressFloor { market: None, level: 0 });
        h.price(BTC, "100000");
        let expiry = h.ts + 60_000;
        let events = expiring_trade(&mut h, &operator, admin_nonce, buy(client.account), expiry);
        if version == ProtocolVersion::V75 {
            // Refused as no key of the account's, and the admin sequence is untouched.
            assert_eq!(rejection(&events), &Reject::InvalidSignature, "{events:?}");
            assert!(fills(&events).is_empty());
            assert_eq!(h.engine.next_request_nonce(operator.account), admin_nonce);
            let events = h.cmd(signed_before);
            assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
        } else {
            // The operator's key filled an order for the account on the admin sequence, so the admin
            // command signed with the nonce that was next before it is refused.
            assert_eq!(fills(&events).len(), 1, "{events:?}");
            assert_eq!(h.engine.next_request_nonce(operator.account), admin_nonce + 1);
            let events = h.cmd(signed_before);
            assert!(matches!(&events[..], [Event::AdminRejected { reason }] if reason.contains("invalid nonce")));
        }
    }
}

/// From `V88` the operator key in effect signs no client request for its own address either: rotated onto
/// a funded client's address, it neither trades there nor spends the admin sequence. Before `V88` the
/// order filled on that sequence and the admin command signed before it was refused.
#[test]
fn the_operator_key_signs_no_client_request_for_its_own_address() {
    for version in [ProtocolVersion::V87, ProtocolVersion::V88] {
        let (mut h, client, _) = funded_at(version);
        h.operator_admin(AdminCommand::RotateOperator { address: client.account });
        let effective = h.ts + NOTICE_MS + 60_000;
        idle_until(&mut h, effective);
        assert_eq!(h.engine.operator().unwrap().address, client.account, "the rotation took effect");
        // `funded_at`'s client.
        let key = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
        assert_eq!(key.account, client.account);
        let nonce = h.engine.next_request_nonce(key.account);
        let signed_before = key.admin(nonce, AdminCommand::SetStressFloor { market: None, level: 0 });
        h.price(BTC, "100000");
        let expiry = h.ts + 60_000;
        let order = expiring_trade(&mut h, &key, nonce, buy(client.account), expiry);
        let admin = h.cmd(signed_before);
        if version >= ProtocolVersion::V88 {
            assert!(fills(&order).is_empty(), "{version:?}: {order:?}");
            assert_eq!(rejection(&order), &Reject::InvalidSignature, "{order:?}");
            assert_eq!(admin.last(), Some(&Event::AdminApplied), "the admin sequence is left alone");
        } else {
            assert_eq!(fills(&order).len(), 1, "{version:?}: {order:?}");
        }
    }
}

/// From `V88` nor does it sign a declaration for its own address, which checks its signature apart from the
/// other client requests: refused, it spends no nonce, so the admin command signed before still applies.
#[test]
fn the_operator_key_declares_nothing_for_its_own_address() {
    for version in [ProtocolVersion::V87, ProtocolVersion::V88] {
        let (mut h, _, _) = funded_at(version);
        let next = Client::new(NEXT_OPERATOR);
        h.operator_admin(AdminCommand::RotateOperator { address: next.account });
        let effective = h.ts + NOTICE_MS + 60_000;
        idle_until(&mut h, effective);
        assert_eq!(h.engine.operator().unwrap().address, next.account, "the rotation took effect");
        let nonce = h.engine.next_request_nonce(next.account);
        let expires_at = h.ts + 60_000;
        let signed_before = Client { expires_at, ..Client::new(NEXT_OPERATOR) }
            .admin(nonce, AdminCommand::SetStressFloor { market: None, level: 0 });
        let class = EligibilityClass::Professional;
        let hash = eip712::eligibility_hash(&next.domain, next.account, class, nonce, expires_at);
        let auth = RequestAuth { nonce, expires_at, signature: eip712::sign(&next.key, &hash) };
        let request = EligibilityRequest { account: next.account, class, auth };
        let declared =
            h.cmd(Command::AttestEligibility { request, evidence: evidence(&h, hash, Some(next.account), None) });
        let admin = h.cmd(signed_before);
        if version >= ProtocolVersion::V88 {
            let refused = Event::EligibilityRejected { account: next.account, reason: Reject::InvalidSignature };
            assert_eq!(declared, [refused], "{version:?}");
            assert_eq!(admin.last(), Some(&Event::AdminApplied), "the admin sequence is left alone");
        } else {
            assert_eq!(declared, [Event::EligibilityAttested { account: next.account, class }], "{version:?}");
            assert_ne!(admin.last(), Some(&Event::AdminApplied), "{version:?}: the declaration spent the nonce");
        }
    }
}
