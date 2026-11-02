//! The session's `V44` part: one signed request cancels every resting order, or closes whole
//! positions, as one command on one nonce.

use engine_core::access::AccessDeposit;
use engine_core::command::{AdminCommand, CloseEntry, Command, OrderKind};
use engine_core::eip712;
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

use crate::common::batch::{cancel_all, close_all, closes};
use crate::common::close_spread::{attest, evidence, fund, rest, trade};
use crate::common::session_keys::{HOUR_MS, register};
use crate::common::{BTC, Client, ETH, Harness, UNLISTED, f};

/// The seeds of the account key and the browser-held session key this part trades through.
const ACCOUNT_SEED: u8 = 0x4b;
const SESSION_SEED: u8 = 0x4c;

/// A resting limit buy of 0.1 in `market` at `price` (2 in ETH, whose minimum notional is higher at
/// its price), whose id it returns.
fn rest_limit(h: &mut Harness, client: &Client, market: u32, price: &str) -> u64 {
    let qty = if market == ETH { "2" } else { "0.1" };
    match rest(h, client, market, Side::Buy, qty, OrderKind::Limit { price: f(price) })[..] {
        [Event::OrderAccepted { order_id, .. }] => order_id,
        ref other => panic!("expected a resting order, got {other:?}"),
    }
}

/// Before `V44` both forms refuse for the account and a registered session key alike, spending no
/// nonce. Activating it, an account holding resting orders and positions in two markets cancels
/// every order on one signature; a close-all fills the entries that can, rejects a slipped, an
/// unlisted and a flat entry by their list index, and a replay of it, a repeated market and an
/// empty list each refuse. A registered session key signs both forms on its own
/// nonce until the account revokes it.
pub(crate) fn session(h: &mut Harness) {
    let account = Client { expires_at: h.ts + 60_000, ..Client::new(ACCOUNT_SEED) };
    let key = Client { expires_at: h.ts + 60_000, ..Client::new(SESSION_SEED) };
    attest(h, &account);
    fund(h, &account, "1000000");
    let house = AccessDeposit {
        account: None,
        payer: Some(account.account),
        amount: f("2000000"),
        chain_ref: "0xhouse:43".into(),
    };
    let ev = evidence(h, eip712::deposit_screening_hash(&house), None, Some(account.account));
    let events = h.cmd(Command::AccessDeposit { deposit: house, evidence: ev });
    assert!(events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. })), "{events:?}");
    h.price(BTC, "80000");
    h.price(ETH, "2500");
    let expires_at = h.ts + HOUR_MS;
    let events = h.cmd(register(h, &account, &key, expires_at));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");

    let refused = |events: &[Event]| matches!(events, [Event::OrderRejected { reason: Reject::BatchUnsupported, .. }]);
    let next = h.engine.next_request_nonce(account.account);
    assert!(refused(&h.cmd(cancel_all(&account, account.account, next))));
    assert!(refused(&h.cmd(close_all(h, &account, account.account, next, closes(&[BTC], "80000"), None))));
    assert!(refused(&h.cmd(cancel_all(&key, account.account, 0))));
    assert!(refused(&h.cmd(close_all(h, &key, account.account, 0, closes(&[BTC], "80000"), None))));
    assert_eq!(h.engine.next_request_nonce(account.account), next);
    assert_eq!(h.engine.next_request_nonce(key.account), 0);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V44 });

    // Positions in two markets and three resting orders; one cancel-all clears the orders.
    for (market, side, qty, quoted) in [(BTC, Side::Buy, "0.1", "81000"), (ETH, Side::Sell, "2", "2400")] {
        let events = trade(h, &account, market, side, qty, quoted);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    let ids = [
        rest_limit(h, &account, BTC, "70000"),
        rest_limit(h, &account, ETH, "2000"),
        rest_limit(h, &account, BTC, "60000"),
    ];
    let nonce = h.engine.next_request_nonce(account.account);
    let events = h.cmd(cancel_all(&account, account.account, nonce));
    let cancelled =
        |order_id| Event::OrderCancelled { account: account.account, order_id, reason: CancelReason::Client };
    assert_eq!(events, ids.map(cancelled), "{events:?}");
    assert_eq!(h.engine.next_request_nonce(account.account), nonce + 1);
    assert!(h.engine.accounts()[&account.account].orders.is_empty());
    let events = h.cmd(cancel_all(&account, account.account, nonce + 1));
    assert!(events.is_empty(), "{events:?}");

    // A close-all: the BTC entry quotes far above a sell's price and slips, an unlisted market
    // is refused, ETH fills. The slipped position stays open, and the replay is refused.
    let entries = vec![
        CloseEntry { market: BTC, quoted: f("90000") },
        CloseEntry { market: UNLISTED, quoted: f("1") },
        CloseEntry { market: ETH, quoted: f("2600") },
    ];
    let nonce = h.engine.next_request_nonce(account.account);
    let cmd = close_all(h, &account, account.account, nonce, entries, Some(50));
    let events = h.cmd(cmd.clone());
    assert_eq!(events.len(), 4, "{events:?}");
    assert!(
        matches!(&events[0], Event::OrderRejected { client_id: 0, reason: Reject::Slippage { .. }, .. }),
        "{events:?}"
    );
    assert!(
        matches!(&events[1], Event::OrderRejected { client_id: 1, reason: Reject::UnknownMarket, .. }),
        "{events:?}"
    );
    assert!(matches!(&events[3], Event::Fill(fill) if fill.market == ETH && fill.position.is_zero()), "{events:?}");
    assert!(h.engine.accounts()[&account.account].positions.contains_key(&BTC));
    let events = h.cmd(cmd);
    assert!(matches!(events[..], [Event::OrderRejected { reason: Reject::InvalidNonce { .. }, .. }]), "{events:?}");

    // A repeated market refuses the whole list, so does an empty one, and a flat market's entry
    // refuses as reduce-only.
    let nonce = h.engine.next_request_nonce(account.account);
    let events = h.cmd(close_all(h, &account, account.account, nonce, closes(&[BTC, BTC], "80000"), None));
    assert!(matches!(events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
    let events = h.cmd(close_all(h, &account, account.account, nonce + 1, vec![], None));
    assert!(matches!(events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
    let events = h.cmd(close_all(h, &account, account.account, nonce + 2, closes(&[ETH], "2500"), None));
    assert!(
        matches!(events[..], [Event::OrderRejected { client_id: 0, reason: Reject::ReduceOnly, .. }]),
        "{events:?}"
    );

    // The registered key signs both forms on its own nonce sequence; the account's never moves.
    let resting = rest_limit(h, &account, BTC, "70000");
    let account_nonce = h.engine.next_request_nonce(account.account);
    let events = h.cmd(cancel_all(&key, account.account, 0));
    assert_eq!(events, [cancelled(resting)]);
    let events = h.cmd(close_all(h, &key, account.account, 1, closes(&[BTC], "80000"), None));
    assert!(
        matches!(&events[..], [Event::OrderAccepted { .. }, Event::Fill(fill)] if fill.position.is_zero()),
        "{events:?}"
    );
    assert_eq!(h.engine.next_request_nonce(key.account), 2);
    assert_eq!(h.engine.next_request_nonce(account.account), account_nonce);

    // Revoked, the key's batches refuse distinctly and spend its nonce.
    let events = h.cmd(account.revoke_session_key(account_nonce, key.account));
    assert!(matches!(events[..], [Event::SessionKeyRevoked { .. }]), "{events:?}");
    let events = h.cmd(cancel_all(&key, account.account, 2));
    assert!(matches!(events[..], [Event::OrderRejected { reason: Reject::SessionKeyRevoked, .. }]), "{events:?}");
    assert_eq!(h.engine.next_request_nonce(key.account), 3);
}
