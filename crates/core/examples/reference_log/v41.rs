//! The session's `V41` part: session keys trade without the wallet, and only within their scope.

use engine_core::access::{
    AccessDeposit, AccessEvidence, EligibilityClass, EligibilityRequest, Jurisdiction, SanctionsStatus, Screening,
};
use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, Signature};
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::session_keys::{MAX_SESSION_KEY_EXPIRY_MS, MAX_SESSION_KEYS_PER_ACCOUNT};
use engine_core::types::{AccountId, Side};
use k256::ecdsa::SigningKey;

use crate::common::{BTC, Client, Harness, f};

/// The seeds of the account key and the browser-held session key this part trades through.
const ACCOUNT_SEED: u8 = 0x3a;
const SESSION_SEED: u8 = 0x3b;

/// Activates `V41`. An account-signed registration authorises the session key to place and cancel
/// orders for the account on the key's own nonce sequence; a session-key withdrawal refuses as out
/// of scope, a revoked key refuses distinctly and a fresh registration revives it with its nonce
/// still spent, and a registration past the 24-hour cap refuses.
pub(crate) fn session(h: &mut Harness) {
    let account = Client { expires_at: h.ts + 60_000, ..Client::new(ACCOUNT_SEED) };
    let key = Client { expires_at: h.ts + 60_000, ..Client::new(SESSION_SEED) };
    // Before `V41` activates, a registration is unsupported and spends nothing.
    let refused = h.cmd(account.register_session_key(0, key.account, h.ts + 3_600_000));
    assert!(
        matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::SessionKeyUnsupported, .. }]),
        "{refused:?}"
    );
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V41 });

    declare(h, &account);
    fund_house(h, &account);
    fund(h, &account);
    h.price(BTC, "80000");

    // A registration expiring more than a day ahead refuses and uses the account's nonce, so it
    // cannot be replayed from the log once it would fit under the cap.
    let next = h.engine.next_request_nonce(account.account);
    let latest = h.ts + MAX_SESSION_KEY_EXPIRY_MS;
    let refused = h.cmd(account.register_session_key(next, key.account, latest + 1));
    assert!(
        matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::ExpiryTooFar { .. }, .. }]),
        "{refused:?}"
    );
    assert_eq!(h.engine.next_request_nonce(account.account), next + 1);
    // The account's own key, an expiry already past, no signature at all, and the operator's
    // address can never be a session key.
    let refused = h.cmd(account.register_session_key(next + 1, account.account, h.ts + 3_600_000));
    assert!(
        matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::InvalidSessionKey, .. }]),
        "{refused:?}"
    );
    let refused = h.cmd(account.register_session_key(next + 2, key.account, h.ts));
    assert!(matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::Expired { .. }, .. }]), "{refused:?}");
    let unsigned = Client { expires_at: h.ts + 60_000, ..Client::new(0x54) };
    let refused = h.cmd(Command::RegisterSessionKey {
        account: unsigned.account,
        session_key: key.account,
        expires_at: h.ts + 3_600_000,
        auth: None,
    });
    assert!(matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::MissingSignature, .. }]), "{refused:?}");
    let operator = h.engine.operator().expect("the log records its operator").address;
    let refused = h.cmd(account.register_session_key(next + 3, operator, h.ts + 3_600_000));
    assert!(
        matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::InvalidSessionKey, .. }]),
        "{refused:?}"
    );

    // One account signature registers the key; its orders and cancels then fill and rest on the
    // key's nonce sequence while the account's sequence stays untouched.
    let expires_at = h.ts + 3_600_000;
    let events = h.cmd(account.register_session_key(next + 4, key.account, expires_at));
    assert!(
        matches!(events[..], [Event::SessionKeyRegistered { session_key, .. }] if session_key == key.account),
        "{events:?}"
    );
    assert_eq!(h.engine.next_request_nonce(account.account), next + 5);
    let events = order(
        h,
        &account,
        &key,
        0,
        Side::Buy,
        "0.01",
        OrderKind::Market { quoted: f("80000"), max_slippage_bps: None },
    );
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    let events = order(h, &account, &key, 1, Side::Buy, "0.01", OrderKind::Limit { price: f("79000") });
    let order_id = match events[..] {
        [Event::OrderAccepted { order_id, .. }] => order_id,
        _ => panic!("expected acceptance, got {events:?}"),
    };
    let events = cancel(h, &account, &key, 2, order_id);
    assert!(events.iter().any(|event| matches!(event, Event::OrderCancelled { .. })), "{events:?}");
    assert_eq!(h.engine.next_request_nonce(key.account), 3);
    assert_eq!(h.engine.next_request_nonce(account.account), next + 5);

    // The key can never move funds: the withdrawal refuses as out of scope, using the key's nonce.
    let events = withdraw(h, &account, &key, 3, "100");
    assert!(matches!(&events[..], [Event::WithdrawalRejected { reason: Reject::SessionKeyScope, .. }]), "{events:?}");
    assert_eq!(h.engine.next_request_nonce(key.account), 4);

    // An account-signed revocation kills the key: its order refuses distinctly, and a fresh
    // registration revives it with the nonce still spent, so the captured order cannot replay.
    let events = h.cmd(account.revoke_session_key(next + 5, key.account));
    assert!(matches!(events[..], [Event::SessionKeyRevoked { .. }]), "{events:?}");
    let events = order(
        h,
        &account,
        &key,
        4,
        Side::Buy,
        "0.01",
        OrderKind::Market { quoted: f("80000"), max_slippage_bps: None },
    );
    assert_eq!(rejection(&events), &Reject::SessionKeyRevoked, "{events:?}");
    let events = h.cmd(account.register_session_key(next + 6, key.account, h.ts + 3_600_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    let events = order(
        h,
        &account,
        &key,
        4,
        Side::Buy,
        "0.01",
        OrderKind::Market { quoted: f("80000"), max_slippage_bps: None },
    );
    assert_eq!(rejection(&events), &Reject::InvalidNonce { expected: 5 }, "{events:?}");
    let events = order(
        h,
        &account,
        &key,
        5,
        Side::Sell,
        "0.01",
        OrderKind::Market { quoted: f("80000"), max_slippage_bps: None },
    );
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");

    // A key's signature cannot manage keys: its registration of a stranger refuses as an invalid
    // signature, and revoking a key the account never registered refuses as an invalid key.
    let stranger = Client { expires_at: h.ts + 60_000, ..Client::new(0x52) };
    let hash = eip712::session_key_registration_hash(
        &key.domain,
        account.account,
        stranger.account,
        h.ts + 3_600_000,
        0,
        key.expires_at,
    );
    let auth = RequestAuth { nonce: 0, expires_at: key.expires_at, signature: eip712::sign(&key.key, &hash) };
    let refused = h.cmd(Command::RegisterSessionKey {
        account: account.account,
        session_key: stranger.account,
        expires_at: h.ts + 3_600_000,
        auth: Some(auth),
    });
    assert!(matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::InvalidSignature, .. }]), "{refused:?}");
    let refused = h.cmd(account.revoke_session_key(next + 7, stranger.account));
    assert!(
        matches!(refused[..], [Event::SessionKeyRejected { reason: Reject::InvalidSessionKey, .. }]),
        "{refused:?}"
    );
    // The key is registered for this account only: an order for another account it signs refuses
    // as an invalid signature and spends nothing.
    let other = Client { expires_at: h.ts + 60_000, ..Client::new(0x71) };
    declare(h, &other);
    let deposit = AccessDeposit {
        account: Some(other.account),
        payer: Some(other.account),
        amount: f("1000000"),
        chain_ref: "0xclient:3b".into(),
    };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), Some(other.account), Some(other.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(matches!(events[..], [Event::Deposited { .. }]), "{events:?}");
    let events =
        order(h, &other, &key, 6, Side::Buy, "0.01", OrderKind::Market { quoted: f("80000"), max_slippage_bps: None });
    assert_eq!(rejection(&events), &Reject::InvalidSignature, "{events:?}");
    assert_eq!(h.engine.next_request_nonce(key.account), 7);

    // A second key shares the account's leaf; a key whose expiry has passed refuses distinctly
    // until a fresh registration prunes it and revives it.
    let second = Client { expires_at: h.ts + 60_000, ..Client::new(SESSION_SEED + 1) };
    let events = h.cmd(account.register_session_key(next + 8, second.account, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    h.advance(60_001);
    h.price(BTC, "80000");
    let events = order_at(h, &account, &second, 0, Side::Buy, "0.01", h.ts + 60_000);
    assert_eq!(rejection(&events), &Reject::SessionKeyExpired { expires_at: h.ts - 1 }, "{events:?}");
    let events = h.cmd(register_at(h, &account, &second, h.ts + 3_600_000, h.ts + 60_000));
    assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    let events = order_at(h, &account, &second, 1, Side::Buy, "0.01", h.ts + 60_000);
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");

    // The cap: fill the account to MAX_SESSION_KEYS_PER_ACCOUNT live keys, refuse an eleventh,
    // and renew a held key at the cap, which the exemption lets replace itself.
    let live = h
        .engine
        .session_key(account.account, key.account)
        .into_iter()
        .chain(h.engine.session_key(account.account, second.account))
        .count();
    for seed in 0x3d..0x3d + (MAX_SESSION_KEYS_PER_ACCOUNT - live) as u8 {
        let extra = Client { expires_at: h.ts + 60_000, ..Client::new(seed) };
        let events = h.cmd(register_at(h, &account, &extra, h.ts + 3_600_000, h.ts + 60_000));
        assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    }
    let eleventh = Client { expires_at: h.ts + 60_000, ..Client::new(0x50) };
    let events = h.cmd(register_at(h, &account, &eleventh, h.ts + 3_600_000, h.ts + 60_000));
    assert!(
        matches!(events[..], [Event::SessionKeyRejected { reason: Reject::TooManySessionKeys { .. }, .. }]),
        "{events:?}"
    );
    let events = h.cmd(register_at(h, &account, &key, h.ts + 3_600_000, h.ts + 60_000));
    assert!(
        matches!(events[..], [Event::SessionKeyRegistered { session_key, .. }] if session_key == key.account),
        "{events:?}"
    );
}

/// The screening source the `V24` part recorded.
fn source() -> SigningKey {
    SigningKey::from_slice(&[0x77; 32]).expect("a valid scalar")
}

/// Fresh, allowed and clear evidence for `hash`, signed by the recorded source.
fn screened(h: &Harness, hash: eip712::B256, account: Option<AccountId>, payer: Option<AccountId>) -> AccessEvidence {
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
    screening.signature =
        eip712::sign(&source(), &eip712::screening_hash(&h.engine.access_domain().unwrap(), &screening));
    AccessEvidence::Screened(screening)
}

/// Records the client's professional declaration.
fn declare(h: &mut Harness, client: &Client) {
    let nonce = h.engine.next_request_nonce(client.account);
    let expires_at = h.ts + 60_000;
    let hash =
        eip712::eligibility_hash(&client.domain, client.account, EligibilityClass::Professional, nonce, expires_at);
    let request = EligibilityRequest {
        account: client.account,
        class: EligibilityClass::Professional,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(&client.key, &hash) },
    };
    let evidence = screened(h, hash, Some(client.account), None);
    let events = h.cmd(Command::AttestEligibility { request, evidence });
    assert!(matches!(events[..], [Event::EligibilityAttested { .. }]), "{events:?}");
}

/// Restocks the house's capital, which the earlier parts paid out.
fn fund_house(h: &mut Harness, payer: &Client) {
    let deposit = AccessDeposit {
        account: None,
        payer: Some(payer.account),
        amount: f("2000000"),
        chain_ref: "0xhouse:3a".into(),
    };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), None, Some(payer.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. })), "{events:?}");
}

/// Funds the client with a screened deposit.
fn fund(h: &mut Harness, client: &Client) {
    let deposit = AccessDeposit {
        account: Some(client.account),
        payer: Some(client.account),
        amount: f("1000000"),
        chain_ref: "0xclient:3a".into(),
    };
    let evidence = screened(h, eip712::deposit_screening_hash(&deposit), Some(client.account), Some(client.account));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence });
    assert!(matches!(events[..], [Event::Deposited { .. }]), "{events:?}");
}

/// The key's signed, screened order of `kind` for `qty` on the account's behalf.
fn order(
    h: &mut Harness,
    account: &Client,
    key: &Client,
    nonce: u64,
    side: Side,
    qty: &str,
    kind: OrderKind,
) -> Vec<Event> {
    let mut request = OrderRequest {
        account: account.account,
        client_id: 40,
        market: BTC,
        side,
        qty: f(qty),
        kind,
        reduce_only: false,
        auth: None,
    };
    let hash = eip712::order_hash(&key.domain, &request, nonce, key.expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at: key.expires_at, signature: eip712::sign(&key.key, &hash) });
    let evidence = screened(h, hash, Some(request.account), None);
    h.cmd(Command::AccessOrder { request, evidence })
}

/// `order` with a request expiry of `expires_at` instead of the key client's.
fn order_at(
    h: &mut Harness,
    account: &Client,
    key: &Client,
    nonce: u64,
    side: Side,
    qty: &str,
    expires_at: i64,
) -> Vec<Event> {
    let kind = OrderKind::Market { quoted: f("80000"), max_slippage_bps: None };
    let mut request = OrderRequest {
        account: account.account,
        client_id: 40,
        market: BTC,
        side,
        qty: f(qty),
        kind,
        reduce_only: false,
        auth: None,
    };
    let hash = eip712::order_hash(&key.domain, &request, nonce, expires_at);
    request.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&key.key, &hash) });
    let evidence = screened(h, hash, Some(request.account), None);
    h.cmd(Command::AccessOrder { request, evidence })
}

/// `account`'s registration of `key` signed with an explicit request expiry.
fn register_at(h: &Harness, account: &Client, key: &Client, session_expires_at: i64, expires_at: i64) -> Command {
    let nonce = h.engine.next_request_nonce(account.account);
    let hash = eip712::session_key_registration_hash(
        &account.domain,
        account.account,
        key.account,
        session_expires_at,
        nonce,
        expires_at,
    );
    Command::RegisterSessionKey {
        account: account.account,
        session_key: key.account,
        expires_at: session_expires_at,
        auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&account.key, &hash) }),
    }
}

/// The key's signed cancel of `order_id` on the account's behalf, as the node sequences cancels.
fn cancel(h: &mut Harness, account: &Client, key: &Client, nonce: u64, order_id: u64) -> Vec<Event> {
    let hash = eip712::cancel_hash(&key.domain, account.account, order_id, nonce, key.expires_at);
    let auth = RequestAuth { nonce, expires_at: key.expires_at, signature: eip712::sign(&key.key, &hash) };
    h.cmd(Command::CancelOrder { account: account.account, order_id, auth: Some(auth) })
}

/// The key's signed withdrawal on the account's behalf, as the client posts it.
fn withdraw(h: &mut Harness, account: &Client, key: &Client, nonce: u64, amount: &str) -> Vec<Event> {
    let hash = eip712::withdrawal_hash(&key.domain, account.account, f(amount), nonce, key.expires_at);
    let auth = RequestAuth { nonce, expires_at: key.expires_at, signature: eip712::sign(&key.key, &hash) };
    h.cmd(Command::Withdraw { account: account.account, amount: f(amount), auth: Some(auth) })
}

/// The order rejection among `events`; panics if there is none.
fn rejection(events: &[Event]) -> &Reject {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a rejection, got {events:?}"))
}
