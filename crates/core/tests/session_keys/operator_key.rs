//! From `V75`, an address an account registered as its session key before the address became the
//! operator key in effect signs nothing as that account's session key while it is the operator's: the
//! operator's sequence is the admin channel's. `V74` let it trade for the account on that sequence.

use engine_core::command::AdminCommand;
use engine_core::event::{Event, Reject};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;

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
