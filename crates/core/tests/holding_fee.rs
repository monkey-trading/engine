//! `V55`'s holding-time profit fee, through `Engine::apply`: a realised gain pays the larger of the
//! base fee and the hold curve's fee at the hold of the slice it closes; the hold runs from the
//! position's open time, which an add moves to the size-weighted average, with the open time counted
//! as no older than the curve's last hold, and a flip restarts; and
//! activation starts every held position at the curve's last hold. The operator's bounds on the base
//! and the curve are in `holding_fee_bounds.rs`.

mod common;

use common::batch::{close_all_expiring, closes};
use common::close_spread::{reduce, trade};
use common::events::{fills, only_fill, rejection};
use common::session_keys::funded_at;
use common::spreads::funded;
use common::{AAPL, BTC, Client, ETH, Harness, f, ny};
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::{Event, Fill, Reject};
use engine_core::fixed::Rounding;
use engine_core::holding_fee::DEFAULT_PROFIT_FEE_CURVE;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};
use engine_core::{Engine, Fixed};

const V54: ProtocolVersion = ProtocolVersion::V54;
const V55: ProtocolVersion = ProtocolVersion::V55;

#[test]
fn a_gain_pays_the_curves_fee_at_its_hold_and_the_base_from_fifteen_minutes() {
    // (hold, rate): the whole gain to 10 s, the curve between its points rounded up to the bp, and the
    // 10% base from 15 minutes.
    let cases = [(0, 10_000), (10_000, 10_000), (10_007, 9_999), (60_000, 6_421), (600_000, 1_828), (900_000, 1_000)];
    for (hold, rate) in cases {
        let (h, close) = round_trip(V55, hold, "101000");
        assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(hold), Some(rate)), "{hold}");
        assert!(close.realised_pnl.is_positive(), "{close:?}");
        assert_eq!(close.profit_fee, close.realised_pnl.mul_bps_rounded(rate, Rounding::Up), "{hold}");
        h.assert_conserved(f("0"));
    }
}

#[test]
fn a_loss_pays_nothing_at_any_hold() {
    for hold in [0, 60_000, 900_000] {
        let (_, close) = round_trip(V55, hold, "99000");
        assert!(close.realised_pnl.is_negative());
        assert_eq!((close.profit_fee, close.hold_ms), (Fixed::ZERO, Some(hold)));
    }
}

#[test]
fn before_v55_every_gain_pays_the_base_and_a_fill_names_no_hold() {
    let (h, close) = round_trip(V54, 0, "101000");
    assert_eq!((close.hold_ms, close.profit_fee_bps), (None, None));
    assert_eq!(close.profit_fee, close.realised_pnl.mul_bps_rounded(1_000, Rounding::Up));
    assert!(h.engine.accounts().values().all(|acct| acct.opened_at.is_empty()));
}

#[test]
fn an_add_moves_the_open_time_to_the_size_weighted_average_rounded_towards_now() {
    let (mut h, client) = funded(V55, "1000000");
    let opened = h.ts;
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "2", "100000"));
    // Adding one to two after 10 minutes moves the open time a third of the way: 200 s on.
    h.advance(600_000);
    h.price(BTC, "100000");
    let add = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    assert_eq!((add.hold_ms, add.profit_fee_bps), (None, None), "an add closes nothing");
    assert_eq!(opened_at(&h, &client), Some(opened + 200_000));
    // Adding 1.5 to 3 a millisecond later moves it a third of the 400.001 s since: 133.333667 s,
    // rounded up, towards now.
    h.advance(1);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1.5", "100000"));
    assert_eq!(opened_at(&h, &client), Some(opened + 333_334));
    // A partial close leaves the open time, and pays at the hold from it.
    h.advance(99_999);
    h.price(BTC, "101000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "2", "101000")).clone();
    assert_eq!(close.hold_ms, Some(366_666));
    assert_eq!(opened_at(&h, &client), Some(opened + 333_334));
    // The whole close removes it.
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "2.5", "101000"));
    assert_eq!(opened_at(&h, &client), None);
}

#[test]
fn an_aged_position_lends_an_add_no_more_than_the_curves_length_of_age() {
    // A $1M BTC long held a day, then $9M added and closed at once. Averaged against the day, the
    // add's units would look 8,640 s old and close at the 10% base; counted from 15 minutes back,
    // the units held lend them 90 s, so the close pays the curve's 56.28% there.
    let (mut h, client) = funded(V55, "10000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    h.advance(24 * 60 * 60 * 1_000);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "90", "100000"));
    assert_eq!(opened_at(&h, &client), Some(h.ts - 90_000));
    h.price(BTC, "101000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "90", "101000")).clone();
    assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(90_000), Some(5_628)), "{close:?}");
    assert!(close.realised_pnl.is_positive());
    assert_eq!(close.profit_fee, close.realised_pnl.mul_bps_rounded(5_628, Rounding::Up));
}

#[test]
fn a_flip_closes_the_position_at_its_hold_and_opens_the_rest_now() {
    let (mut h, client) = funded(V55, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000"));
    h.advance(30_000);
    h.price(BTC, "99000");
    let flip = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "3", "99000")).clone();
    // The short's gain pays at its 30 s hold, 78.53%, and the long the rest opens starts now.
    assert_eq!((flip.hold_ms, flip.profit_fee_bps, flip.position), (Some(30_000), Some(7_853), f("2")));
    assert_eq!(flip.profit_fee, flip.realised_pnl.mul_bps_rounded(7_853, Rounding::Up));
    assert_eq!(opened_at(&h, &client), Some(h.ts));
}

#[test]
fn a_close_out_pays_the_fee_at_the_hold_of_the_gaining_position_it_closes() {
    // 100,000 holds 3 BTC ($300k, 30k margin) and 300 ETH ($600k, 60k margin).
    let (mut h, client, _) = funded_at(V55);
    h.price(ETH, "2000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "3", "100000"));
    only_fill(&trade(&mut h, &client, ETH, Side::Buy, "300", "2000"));
    // Five seconds on, BTC is up 1% and ETH down 15%: the close-out closes ETH at a loss, and the
    // account, still under half of BTC's margin, then closes BTC's gain at its 5 s hold.
    h.advance(5_000);
    h.price(BTC, "101000");
    let events = h.price(ETH, "1700");
    assert!(events.iter().any(|event| matches!(event, Event::CloseOut { .. })), "{events:?}");
    let closed = fills(&events);
    let [eth, btc] = closed[..] else { panic!("two close-out fills: {events:?}") };
    assert!(eth.liquidation && btc.liquidation);
    assert!(eth.realised_pnl.is_negative() && eth.profit_fee.is_zero());
    assert_eq!((eth.hold_ms, btc.hold_ms, btc.profit_fee_bps), (Some(5_000), Some(5_000), Some(10_000)));
    assert!(btc.realised_pnl.is_positive());
    assert_eq!(btc.profit_fee, btc.realised_pnl);
    h.assert_conserved(f("0"));
}

#[test]
fn the_margin_check_on_a_flip_counts_the_fee_at_the_closing_slices_hold() {
    // 100,000 holds 5 BTC, and BTC rises 10%: flipping to 10 short realises 50,000 and needs 110,000
    // of margin, which the gain covers at the 10% base but not at the 100% the first 10 s pay.
    for (version, hold, fills_it) in [(V55, 5_000, false), (V55, 900_000, true), (V54, 5_000, true)] {
        let (mut h, client, _) = funded_at(version);
        only_fill(&trade(&mut h, &client, BTC, Side::Buy, "5", "100000"));
        h.advance(hold);
        h.price(BTC, "110000");
        let events = trade(&mut h, &client, BTC, Side::Sell, "15", "110000");
        if fills_it {
            assert_eq!(only_fill(&events).position, f("-10"), "{version:?} {hold}");
        } else {
            assert!(matches!(rejection(&events), Reject::InsufficientMargin { .. }), "{events:?}");
        }
    }
}

#[test]
fn a_close_all_past_the_input_bound_pays_each_piece_at_the_positions_hold() {
    // 1.2 billion units at five hundredths of a cent, half opened a minute after the other, so the open
    // time is half a minute after the first; a close-all closes a piece of the input bound, and the
    // next the rest, each at the hold from that open time.
    let (mut h, client, _) = funded_at(V55);
    let opened = h.ts;
    h.price(BTC, "0.0005");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "600000000", "0.0005"));
    h.advance(60_000);
    h.price(BTC, "0.0005");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "600000000", "0.0005"));
    assert_eq!(opened_at(&h, &client), Some(opened + 30_000));
    h.advance(120_000);
    h.price(BTC, "0.0006");
    for qty in [Fixed::MAX_INPUT, f("200000000")] {
        let nonce = h.engine.next_request_nonce(client.account);
        let expires_at = h.ts + 60_000;
        let request =
            close_all_expiring(&h, &client, client.account, nonce, closes(&[BTC], "0.0006"), None, expires_at);
        let piece = only_fill(&h.cmd(request)).clone();
        // Held 150 s: 37% plus the 9 points to 46% the curve falls over its 84.549 s segment, pro rata.
        assert_eq!((piece.qty, piece.hold_ms, piece.profit_fee_bps), (qty, Some(150_000), Some(4_588)));
    }
    assert_eq!(opened_at(&h, &client), None);
}

#[test]
fn a_split_keeps_the_open_time_and_one_that_closes_the_position_removes_it() {
    // A two-for-one split doubles a unit; one new for two billion old takes it to none, paid in cash.
    for (new_units, old_units, kept) in [(2, 1, true), (1, 2_000_000_000, false)] {
        let (mut h, client, _) = funded_at(V55);
        h.price(AAPL, "80");
        let opened = h.ts;
        only_fill(&trade(&mut h, &client, AAPL, Side::Buy, "1", "80"));
        let split = CorporateAction {
            id: format!("split-{new_units}-{old_units}"),
            market: AAPL,
            previous_close: ny(2026, 10, 20, 16, 0),
            ex_open: ny(2026, 10, 21, 9, 30),
            adjustment: Adjustment::Split { new_units, old_units },
        };
        assert!(h.operate(AdminCommand::ScheduleCorporateAction(split)).contains(&Event::AdminApplied));
        h.advance_to(ny(2026, 10, 21, 9, 30));
        let events = h.cmd(Command::Tick);
        assert!(events.iter().any(|event| matches!(event, Event::CorporateSplit { .. })), "{events:?}");
        let acct = &h.engine.accounts()[&client.account];
        assert_eq!(acct.positions.contains_key(&AAPL), kept);
        assert_eq!(acct.opened_at.get(&AAPL).copied(), kept.then_some(opened));
    }
}

#[test]
fn activation_starts_each_held_position_at_the_curves_last_hold_and_the_default_curve() {
    let (mut h, client) = funded(V54, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "2", "100000"));
    assert!(h.engine.params().unwrap().profit_fee_curve.is_empty());
    // A root before the activation, so the one after it updates that tree rather than building one.
    h.engine.state_root();
    h.admin(AdminCommand::ActivateProtocol { version: V55 });
    // The activation's open times reach the incremental root as a tree rebuilt from the state has them.
    let root = h.engine.state_root();
    let mut rebuilt: Engine = serde_json::from_value(serde_json::to_value(&h.engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), root);
    assert_eq!(h.engine.params().unwrap().profit_fee_curve, DEFAULT_PROFIT_FEE_CURVE);
    assert_eq!(h.engine.operator_bounds().launch_curve.as_deref(), Some(&DEFAULT_PROFIT_FEE_CURVE[..]));
    assert_eq!(opened_at(&h, &client), Some(h.ts - 900_000));
    // A close straight after pays the base, at the curve's last hold.
    h.price(BTC, "101000");
    let close = only_fill(&reduce(&mut h, &client, BTC, Side::Sell, "1", "101000")).clone();
    assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(900_000), Some(1_000)));
}

#[test]
fn the_account_read_gives_the_rate_a_close_pays_now_and_when_it_next_falls() {
    let (mut h, client) = funded(V55, "1000000");
    let opened = h.ts;
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    let fee = |h: &Harness| h.engine.close_fee(client.account, BTC).unwrap();
    // (time since the open, rate, next fall): flat to 10 s, falling a bp every few ms after it, and
    // the base, which never falls, from 15 minutes.
    for (at, rate, falls) in [(0, 10_000, Some(10_007)), (60_000, 6_421, Some(60_005)), (900_000, 1_000, None)] {
        h.advance_to(opened + at);
        h.cmd(Command::Tick);
        let close = fee(&h);
        assert_eq!((close.opened_at, close.fee_bps, close.falls_at), (opened, rate, falls.map(|ms| opened + ms)));
    }
    assert_eq!(h.engine.close_fee(client.account, ETH), None);
}

#[test]
fn the_state_root_commits_each_open_time_and_the_position_leaf_keeps_its_hash() {
    let (mut h, client) = funded(V55, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    let root = h.engine.state_root();
    let state = serde_json::to_value(&h.engine).unwrap();
    // The same state rebuilt hashes to the same root; a moved open time alone changes it.
    let mut rebuilt: Engine = serde_json::from_value(state.clone()).unwrap();
    assert_eq!(rebuilt.state_root(), root);
    let account = client.account.to_string();
    let mut moved = state.clone();
    let opened = &mut moved["accounts"][&account]["opened_at"][BTC.to_string()];
    *opened = serde_json::json!(opened.as_i64().unwrap() - 1);
    let mut moved: Engine = serde_json::from_value(moved).unwrap();
    assert_ne!(moved.state_root(), root);
    // The exit's proofs hold beside the open-time leaves, with and without principal awaiting
    // finality, the position leaf hashing only its quantity and entry.
    for unfinalised in [None, Some("1000")] {
        let mut state = state.clone();
        if let Some(amount) = unfinalised {
            state["accounts"][&account]["unfinalised_deposits"] = serde_json::json!(amount);
        }
        let mut engine: Engine = serde_json::from_value(state).unwrap();
        let root = engine.state_root();
        let exit = engine.exit_proof(client.account).unwrap();
        let proof = exit.account.as_ref().unwrap();
        assert_eq!(exit.root.0, root);
        assert!(exit.terms.verify(exit.root) && proof.verify(exit.root), "{unfinalised:?}");
        assert_eq!(proof.leaf.unfinalised_deposits, unfinalised.map_or(Fixed::ZERO, f));
        assert_eq!(proof.positions.len(), 1);
    }
}

// ── Helpers ──

/// Opens a 1 BTC long at 100000 under `version`, lets `hold` pass, and closes it at `mid`.
fn round_trip(version: ProtocolVersion, hold: Millis, mid: &str) -> (Harness, Fill) {
    let (mut h, client) = funded(version, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    h.advance(hold);
    h.price(BTC, mid);
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", mid)).clone();
    (h, close)
}

/// `client`'s BTC open time.
fn opened_at(h: &Harness, client: &Client) -> Option<Millis> {
    h.engine.accounts()[&client.account].opened_at.get(&BTC).copied()
}
