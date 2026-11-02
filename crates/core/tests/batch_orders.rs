//! Batch orders from `V44`: one signed request cancels every resting order, or closes whole
//! positions, as one sequenced command on one nonce.

mod common;

use common::batch::{cancel_all, close_all, close_all_expiring, closes, funded, market_order, onboard, replayed};
use common::close_spread::rest;
use common::events::{fills, rejection};
use common::session_keys::{funded_at, register};
use common::{BTC, Client, ETH, Harness, UNLISTED, f};
use engine_core::access::AccessReject;
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;
use engine_core::command::{AdminCommand, CloseAllRequest, CloseEntry, Command, OrderKind};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Side};

/// One resting limit buy of 0.1 in `market` far below the price, so it rests.
fn rest_limit(h: &mut Harness, client: &Client, market: u32, price: &str) -> u64 {
    match rest(h, client, market, Side::Buy, "0.1", OrderKind::Limit { price: f(price) })[..] {
        [Event::OrderAccepted { order_id, .. }] => order_id,
        ref other => panic!("expected a resting order, got {other:?}"),
    }
}

/// Opens a long in BTC and a short in ETH for the client.
fn open_positions(h: &mut Harness, client: &Client) {
    h.price(ETH, "3000");
    assert_eq!(fills(&market_order(h, client, BTC, Side::Buy, "0.5", "100000")).len(), 1);
    assert_eq!(fills(&market_order(h, client, ETH, Side::Sell, "5", "3000")).len(), 1);
}

fn nonce(h: &Harness, account: AccountId) -> u64 {
    h.engine.next_request_nonce(account)
}

#[test]
fn cancel_all_cancels_every_resting_order_with_a_single_cancels_event_on_one_nonce() {
    let (mut h, client, _) = funded();
    h.price(ETH, "3000");
    let ids = [
        rest_limit(&mut h, &client, BTC, "90000"),
        rest_limit(&mut h, &client, ETH, "2500"),
        rest_limit(&mut h, &client, BTC, "80000"),
    ];
    let before = nonce(&h, client.account);
    let events = h.cmd(cancel_all(&client, client.account, before));
    let cancelled =
        |order_id| Event::OrderCancelled { account: client.account, order_id, reason: CancelReason::Client };
    assert_eq!(events, ids.map(cancelled), "ascending order id, the event a single cancel emits");
    assert!(h.engine.accounts()[&client.account].orders.is_empty());
    assert!(h.engine.markets()[&BTC].resting.is_empty() && h.engine.markets()[&ETH].resting.is_empty());
    assert_eq!(nonce(&h, client.account), before + 1, "three cancels, one nonce");
}

#[test]
fn cancel_all_leaves_other_accounts_orders_and_cancelled_ids_stay_cancelled() {
    let (mut h, client, other) = funded();
    onboard(&mut h, &other, "100000");
    let mine = rest_limit(&mut h, &client, BTC, "90000");
    let theirs = rest_limit(&mut h, &other, BTC, "90000");
    h.cmd(cancel_all(&client, client.account, nonce(&h, client.account)));
    assert!(h.engine.accounts()[&other.account].orders.contains_key(&theirs));
    let again = h.cmd(client.cancel(nonce(&h, client.account), mine));
    assert_eq!(rejection(&again), &Reject::UnknownOrder);
}

#[test]
fn cancel_all_with_nothing_resting_is_a_no_op_that_still_uses_its_nonce() {
    let (mut h, client, _) = funded();
    let before = nonce(&h, client.account);
    assert_eq!(h.cmd(cancel_all(&client, client.account, before)), []);
    assert_eq!(nonce(&h, client.account), before + 1);
}

#[test]
fn a_signed_batch_cannot_be_replayed_or_signed_for_another_account() {
    let (mut h, client, other) = funded();
    rest_limit(&mut h, &client, BTC, "90000");
    let n = nonce(&h, client.account);
    let cmd = cancel_all(&client, client.account, n);
    assert_eq!(h.cmd(cmd.clone()).len(), 1);
    assert_eq!(rejection(&h.cmd(cmd)), &Reject::InvalidNonce { expected: n + 1 });

    onboard(&mut h, &other, "1000");
    let order = rest_limit(&mut h, &other, BTC, "90000");
    // The client's signature over a cancel-all naming another account recovers to the client.
    let forged = cancel_all(&client, other.account, nonce(&h, client.account));
    assert_eq!(rejection(&h.cmd(forged)), &Reject::InvalidSignature);
    assert!(h.engine.accounts()[&other.account].orders.contains_key(&order));

    let close = close_all(&h, &client, client.account, nonce(&h, client.account), closes(&[BTC], "100000"), None);
    h.cmd(close.clone());
    assert!(matches!(rejection(&h.cmd(close)), Reject::InvalidNonce { .. }));
}

#[test]
fn close_all_closes_each_named_position_as_a_reduce_only_market_order() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    let before = nonce(&h, client.account);
    // Quotes 1% adverse to each closing side. The client's bound of 10000 bps is capped at the market's own.
    let adverse = vec![CloseEntry { market: ETH, quoted: f("2970") }, CloseEntry { market: BTC, quoted: f("101000") }];
    let events = h.cmd(close_all(&h, &client, client.account, before, adverse, Some(10_000)));
    let slipped: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            Event::OrderRejected { client_id, reason: Reject::Slippage { .. }, .. } => Some(*client_id),
            _ => None,
        })
        .collect();
    assert_eq!(slipped, [0, 1], "the market's own bound clamps the client's: {events:?}");
    assert!(fills(&events).is_empty());

    let cmd = close_all(&h, &client, client.account, nonce(&h, client.account), closes(&[ETH], "3000"), None);
    let events = h.cmd(cmd);
    assert!(
        matches!(&events[..], [Event::OrderAccepted { client_id: 0, .. }, Event::Fill(fill)] if fill.market == ETH)
    );
    assert!(!h.engine.accounts()[&client.account].positions.contains_key(&ETH));
    assert!(h.engine.accounts()[&client.account].positions.contains_key(&BTC));
    assert_eq!(nonce(&h, client.account), before + 2, "two requests, one nonce each");
}

#[test]
fn an_entry_that_cannot_fill_does_not_stop_the_others() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    let entries = vec![
        closes(&[BTC], "110000").remove(0),   // adverse to a sell: slippage
        closes(&[UNLISTED], "100").remove(0), // unlisted
        closes(&[ETH], "3000").remove(0),     // fills
    ];
    let events = h.cmd(close_all(&h, &client, client.account, nonce(&h, client.account), entries, None));
    let reasons: Vec<(u64, &Reject)> = events
        .iter()
        .filter_map(|e| match e {
            Event::OrderRejected { client_id, reason, .. } => Some((*client_id, reason)),
            _ => None,
        })
        .collect();
    assert!(matches!(reasons[0], (0, Reject::Slippage { .. })), "{events:?}");
    assert_eq!(reasons[1], (1, &Reject::UnknownMarket));
    assert_eq!(fills(&events).len(), 1);
    assert_eq!(fills(&events)[0].market, ETH);
    assert!(h.engine.accounts()[&client.account].positions.contains_key(&BTC), "the slipped entry stays open");
}

#[test]
fn a_close_never_flips_a_position() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    let events =
        h.cmd(close_all(&h, &client, client.account, nonce(&h, client.account), closes(&[BTC, ETH], "100000"), None));
    for fill in fills(&events) {
        assert!(fill.position.is_zero(), "a close ends flat: {fill:?}");
    }
    assert_eq!(fills(&events).len(), 2, "{events:?}");
    assert!(h.engine.accounts()[&client.account].positions.is_empty());
    // Closing again finds nothing to reduce.
    let events =
        h.cmd(close_all(&h, &client, client.account, nonce(&h, client.account), closes(&[BTC], "100000"), None));
    assert_eq!(rejection(&events), &Reject::ReduceOnly);
    assert!(!h.engine.accounts()[&client.account].positions.contains_key(&BTC));
}

#[test]
fn a_list_naming_a_market_twice_or_more_markets_than_are_listed_is_refused_whole() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    let before = nonce(&h, client.account);
    let events = h.cmd(close_all(&h, &client, client.account, before, closes(&[BTC, ETH, BTC], "100000"), None));
    assert!(matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
    let many = closes(&[BTC, ETH, 3, 4, 5, 6, 7, 8], "1");
    let events = h.cmd(close_all(&h, &client, client.account, before + 1, many, None));
    assert!(matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
    assert_eq!(h.engine.accounts()[&client.account].positions.len(), 2, "nothing closed");
    assert_eq!(nonce(&h, client.account), before + 2, "each refused request used its nonce");
}

#[test]
fn an_empty_list_is_refused_and_an_account_without_positions_gets_a_rejection_per_entry() {
    let (mut h, client, _) = funded();
    h.price(ETH, "3000");
    let before = nonce(&h, client.account);
    // Screened and signed, an empty list still emits, so every close-all leaves an event.
    let events = h.cmd(close_all(&h, &client, client.account, before, vec![], None));
    assert!(matches!(&events[..], [Event::OrderRejected { reason: Reject::InvalidOrder { .. }, .. }]), "{events:?}");
    assert_eq!(nonce(&h, client.account), before + 1);
    let events = h.cmd(close_all(&h, &client, client.account, before + 1, closes(&[BTC, ETH], "100000"), None));
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| matches!(e, Event::OrderRejected { reason: Reject::ReduceOnly, .. })), "{events:?}");
}

#[test]
fn a_batch_past_its_expiry_or_too_far_ahead_is_refused_and_uses_its_nonce() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    rest_limit(&mut h, &client, BTC, "90000");
    let before = nonce(&h, client.account);
    let (past, ahead) = (h.ts - 1, h.ts + MAX_EXPIRY_AHEAD_MS + 1);
    for (offset, (expires_at, reason)) in [
        (past, Reject::Expired { expires_at: past }),
        (ahead, Reject::ExpiryTooFar { latest: h.ts + MAX_EXPIRY_AHEAD_MS }),
    ]
    .into_iter()
    .enumerate()
    {
        let signer = Client { expires_at, ..Client::new(1) };
        let cancel = cancel_all(&signer, client.account, before + 2 * offset as u64);
        assert_eq!(rejection(&h.cmd(cancel)), &reason);
        let n = before + 2 * offset as u64 + 1;
        let close = close_all_expiring(&h, &client, client.account, n, closes(&[BTC], "100000"), None, expires_at);
        assert_eq!(rejection(&h.cmd(close)), &reason);
    }
    assert_eq!(nonce(&h, client.account), before + 4, "each refusal used its nonce");
    assert_eq!(h.engine.accounts()[&client.account].orders.len(), 1, "nothing was cancelled");
    assert_eq!(h.engine.accounts()[&client.account].positions.len(), 2, "nothing was closed");
}

#[test]
fn the_signature_covers_the_whole_list_in_order_and_the_slippage_bound() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    let n = nonce(&h, client.account);
    let Command::AccessCloseAll { request, evidence } =
        close_all(&h, &client, client.account, n, closes(&[BTC, ETH], "100000"), None)
    else {
        unreachable!("a screened close-all")
    };
    type Tamper = fn(&mut CloseAllRequest);
    let tampers: [Tamper; 4] = [
        |r| r.closes.reverse(),
        |r| r.closes.truncate(1),
        |r| r.closes[0].quoted = f("1"),
        |r| r.max_slippage_bps = Some(1),
    ];
    for tamper in tampers {
        let mut altered = request.clone();
        tamper(&mut altered);
        let events = h.cmd(Command::AccessCloseAll { request: altered, evidence: evidence.clone() });
        assert_eq!(rejection(&events), &Reject::InvalidSignature, "{events:?}");
    }
    assert_eq!(h.engine.accounts()[&client.account].positions.len(), 2);
}

#[test]
fn close_all_is_screened_as_a_single_order_is() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    let n = nonce(&h, client.account);
    let Command::AccessCloseAll { evidence, .. } =
        close_all(&h, &client, client.account, n, closes(&[BTC], "100000"), None)
    else {
        unreachable!("a screened close-all")
    };
    // Evidence for another request (here the same close at another quote) does not carry over.
    let Command::AccessCloseAll { request: next, .. } =
        close_all(&h, &client, client.account, n, closes(&[BTC], "100001"), None)
    else {
        unreachable!("a screened close-all")
    };
    let events = h.cmd(Command::AccessCloseAll { request: next, evidence });
    assert_eq!(rejection(&events), &Reject::Access { reason: AccessReject::EvidenceMismatch });
    // An account that never declared cannot close, exactly as it cannot place an order.
    h.advance(1);
    let undeclared = Client::new(9);
    let events = h.cmd(close_all(&h, &undeclared, undeclared.account, 0, closes(&[BTC], "100000"), None));
    assert_eq!(rejection(&events), &Reject::Access { reason: AccessReject::EligibilityRequired });
}

#[test]
fn a_registered_session_key_signs_either_form_on_its_own_nonce_and_never_after_revocation() {
    let (mut h, client, key) = funded();
    open_positions(&mut h, &client);
    rest_limit(&mut h, &client, BTC, "90000");
    let expires_at = h.ts + common::session_keys::HOUR_MS;
    let cmd = register(&h, &client, &key, expires_at);
    h.cmd(cmd);
    let account_nonce = nonce(&h, client.account);

    let events = h.cmd(cancel_all(&key, client.account, 0));
    assert!(matches!(&events[..], [Event::OrderCancelled { .. }]), "{events:?}");
    let events = h.cmd(close_all(&h, &key, client.account, 1, closes(&[BTC, ETH], "100000"), None));
    assert!(!fills(&events).is_empty(), "{events:?}");
    assert_eq!(nonce(&h, key.account), 2);
    assert_eq!(nonce(&h, client.account), account_nonce, "the account's sequence never moved");

    let cmd = client.revoke_session_key(account_nonce, key.account);
    h.cmd(cmd);
    assert_eq!(rejection(&h.cmd(cancel_all(&key, client.account, 2))), &Reject::SessionKeyRevoked);
    let events = h.cmd(close_all(&h, &key, client.account, 3, closes(&[BTC], "100000"), None));
    assert_eq!(rejection(&events), &Reject::SessionKeyRevoked);
}

#[test]
fn before_v44_both_forms_are_refused_and_use_no_nonce_even_from_a_session_key() {
    let (mut h, client, key) = funded_at(ProtocolVersion::V43);
    open_positions(&mut h, &client);
    let cmd = register(&h, &client, &key, h.ts + common::session_keys::HOUR_MS);
    h.cmd(cmd);
    let (account_nonce, key_nonce) = (nonce(&h, client.account), nonce(&h, key.account));
    for signer in [&client, &key] {
        let n = nonce(&h, signer.account);
        assert_eq!(rejection(&h.cmd(cancel_all(signer, client.account, n))), &Reject::BatchUnsupported);
        let events = h.cmd(close_all(&h, signer, client.account, n, closes(&[BTC], "100000"), None));
        assert_eq!(rejection(&events), &Reject::BatchUnsupported);
    }
    assert_eq!((nonce(&h, client.account), nonce(&h, key.account)), (account_nonce, key_nonce));
    assert_eq!(h.engine.accounts()[&client.account].positions.len(), 2);
    // Activation turns them on.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V44 });
    let n = nonce(&h, client.account);
    let events = h.cmd(close_all(&h, &client, client.account, n, closes(&[BTC], "100000"), None));
    assert_eq!(fills(&events).len(), 1);
}

#[test]
fn a_log_with_batches_replays_to_its_roots() {
    let (mut h, client, _) = funded();
    open_positions(&mut h, &client);
    rest_limit(&mut h, &client, BTC, "90000");
    rest_limit(&mut h, &client, ETH, "2500");
    h.cmd(cancel_all(&client, client.account, nonce(&h, client.account)));
    h.cmd(close_all(&h, &client, client.account, nonce(&h, client.account), closes(&[BTC, ETH], "100000"), None));
    let mut replay = replayed(&h);
    assert_eq!(replay.state_root(), h.engine.state_root());
    assert!(replay.accounts()[&client.account].positions.is_empty());
    assert!(replay.accounts()[&client.account].orders.is_empty());
}
