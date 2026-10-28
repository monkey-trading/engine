//! The session's `V34` part: a withdrawal request is at least 10 USDC.

use engine_core::access::SanctionsStatus;
use engine_core::command::AdminCommand;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::withdrawal::MIN_WITHDRAWAL;
use k256::ecdsa::SigningKey;

use crate::common::{Client, Harness};
use crate::v24::Session;

/// `V34` activates. A screened request below 10 USDC is refused and reserves nothing, a request
/// of exactly 10 USDC is queued and approved, and so is the next one.
pub(crate) fn session(h: &mut Harness) {
    let s = Session { source: SigningKey::from_slice(&[0x77; 32]).expect("a valid scalar") };
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V34 });
    let client = Client { expires_at: h.ts + 60_000, ..Client::new(20) };
    let balance = h.balance(client.account);
    assert!(balance > MIN_WITHDRAWAL, "the client holds more than the minimum");
    for dust in ["0.000001", "9.999999"] {
        let events = s.withdraw(h, &client, dust, true, SanctionsStatus::Clear);
        assert!(
            matches!(&events[..], [Event::WithdrawalRejected { reason: Reject::BelowMinimumWithdrawal { .. }, .. }]),
            "{dust}: {events:?}"
        );
    }
    assert_eq!(h.balance(client.account), balance, "a refused request reserves nothing");
    for _ in 0..2 {
        let events = s.withdraw(h, &client, "10", true, SanctionsStatus::Clear);
        assert!(
            matches!(&events[..], [Event::WithdrawalQueued { .. }, Event::WithdrawalApproved { .. }, ..]),
            "{events:?}"
        );
    }
    assert_eq!(h.balance(client.account), balance - MIN_WITHDRAWAL - MIN_WITHDRAWAL);
}
