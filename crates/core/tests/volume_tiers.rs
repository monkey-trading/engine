//! `V59`'s volume tiers, through `Engine::apply`: an account trades at the best price only up to its
//! tier's share of each market's first spread bound; opening notional counts once it has been held
//! 60 s, on the day it matures, a close takes the pending notional newest first and counts nothing for
//! it, a close of matured notional counts once the round-trip horizon has passed, a round trip counts
//! nothing on either fill, and only counted volume reaches a tier. The operator's bounds on the table
//! are in `volume_tier_bounds.rs`.

mod common;

use common::close_spread::{rest, trade};
use common::events::{fills, only_fill};
use common::session_keys::funded_at;
use common::{AAPL, BTC, Client, Harness, f, ny};
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::{Event, Fill};
use engine_core::fixed::Rounding;
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::HORIZON_MS;
use engine_core::types::{Millis, Side};
use engine_core::volume_tier::{DAY_MS, MIN_HOLD_MS, VolumeStanding, day_of, launch_volume_tiers};
use engine_core::{Engine, Fixed};

const V58: ProtocolVersion = ProtocolVersion::V58;
const V59: ProtocolVersion = ProtocolVersion::V59;

#[test]
fn a_fresh_account_trades_a_quarter_of_the_first_band_at_the_best_price() {
    // BTC's bands read 1bp to $1M, 2bp to $5M and 5bp above. Tier 0's first band ends at $250k, so a
    // $600k buy pays 1bp on $250k and 2bp on $350k: $95, a unit's share rounded up.
    let (mut h, client, _) = funded_at(V59);
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "6", "100000")).clone();
    assert_eq!(buy.price, f("100015.833333334"));
    let standing = standing(&h, &client);
    assert_eq!((standing.tier, standing.best_price_bounds[&BTC]), (0, f("250000")));
    // Before `V59` the same buy pays 1bp on all of it, and the account read names no volume.
    let (mut h, client, _) = funded_at(V58);
    assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Buy, "6", "100000")).price, f("100010"));
    assert_eq!(h.engine.volume_standing(client.account), None);
}

#[test]
fn opening_notional_counts_once_held_sixty_seconds_on_the_day_it_matures() {
    let (mut h, client, _) = funded_at(V59);
    // Opened 30 s before UTC midnight, the buy matures on the next day.
    let midnight = (day_of(h.ts) + 1) * DAY_MS;
    h.advance_to(midnight - 30_000);
    h.price(BTC, "100000");
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    let notional = buy.price;
    tick_at(&mut h, midnight + 29_999);
    assert_eq!(volumes(&h, &client), (Fixed::ZERO, notional));
    tick_at(&mut h, midnight + 30_000);
    assert_eq!(volumes(&h, &client), (notional, Fixed::ZERO));
    // The next fill counts the lot into its book, on the day it matured.
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "0.001", "100000"));
    let book = h.engine.account_volume(client.account).unwrap();
    assert_eq!((book.days.len(), book.days[0].day, book.days[0].volume), (1, day_of(midnight), notional));
}

#[test]
fn a_close_inside_sixty_seconds_takes_the_newest_pending_notional_and_counts_nothing_for_it() {
    let (mut h, client, _) = funded_at(V59);
    let first = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    let opened = h.ts;
    h.advance(30_000);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    h.advance(10_000);
    h.price(BTC, "100000");
    // The sell takes the second buy whole and half the first, and leaves half the first pending.
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1.5", "100000"));
    let half = first.price.mul_div(f("0.5"), f("1"), Rounding::Down);
    assert_eq!(volumes(&h, &client), (Fixed::ZERO, half));
    tick_at(&mut h, opened + MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (half, Fixed::ZERO));
    tick_at(&mut h, opened + 10 * MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (half, Fixed::ZERO));
}

#[test]
fn a_close_of_matured_notional_counts_on_its_day_once_the_round_trip_horizon_has_passed() {
    let (mut h, client, _) = funded_at(V59);
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    h.price(BTC, "100000");
    let sell = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000")).clone();
    let closed = h.ts;
    assert_eq!(volumes(&h, &client), (buy.price, sell.price));
    tick_at(&mut h, closed + HORIZON_MS - 1);
    assert_eq!(volumes(&h, &client), (buy.price, sell.price));
    tick_at(&mut h, closed + HORIZON_MS);
    assert_eq!(volumes(&h, &client), (buy.price + sell.price, Fixed::ZERO));
}

#[test]
fn a_round_trip_at_one_index_counts_nothing_on_either_fill() {
    // An open closed at the same index a second later is pending, so neither fill counts.
    let (mut h, client, _) = funded_at(V59);
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    h.advance(1_000);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000"));
    tick_after(&mut h, 10 * MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (Fixed::ZERO, Fixed::ZERO));

    // A matured close reopened at its index a second later is taken back: neither fill counts, and the
    // reopened position is matured again, so closing it counts.
    let (mut h, client, _) = funded_at(V59);
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000"));
    h.advance(1_000);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    assert_eq!(volumes(&h, &client), (buy.price, Fixed::ZERO));
    tick_after(&mut h, 10 * MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (buy.price, Fixed::ZERO));
    h.price(BTC, "100000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000")).clone();
    tick_after(&mut h, HORIZON_MS);
    assert_eq!(volumes(&h, &client), (buy.price + close.price, Fixed::ZERO));
}

#[test]
fn a_reopening_at_another_index_is_no_round_trip_and_both_fills_count() {
    let (mut h, client, _) = funded_at(V59);
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    h.price(BTC, "100000");
    let sell = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000")).clone();
    h.advance(1_000);
    h.price(BTC, "100100");
    let again = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100100")).clone();
    tick_after(&mut h, MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (buy.price + sell.price + again.price, Fixed::ZERO));
}

#[test]
fn a_day_counts_at_most_half_of_tier_ones_threshold_so_it_takes_two_days_and_pending_grants_nothing() {
    let (mut h, client, _) = funded_at(V59);
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "6", "100000"));
    // A day later its $600k has counted, capped at $500k towards tier 1.
    h.advance(DAY_MS);
    h.price(BTC, "100000");
    assert_eq!(standing(&h, &client).progress[1].capped, f("500000"));
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "6", "100000"));
    // The close's $600k waits out the horizon, and the account prices at tier 0 meanwhile.
    let quoted = |h: &Harness| h.engine.quote(client.account, BTC, Side::Buy, f("6")).unwrap().price;
    assert_eq!((standing(&h, &client).tier, quoted(&h)), (0, f("100015.833333334")));
    tick_after(&mut h, HORIZON_MS);
    let reached = standing(&h, &client);
    assert_eq!((reached.tier, reached.progress[1].capped, reached.progress[1].days), (1, f("1000000"), 2));
    assert_eq!(reached.best_price_bounds[&BTC], f("500000"));
    // Tier 1's first band ends at $500k: $50 at 1bp and $20 at 2bp on a $600k buy.
    h.price(BTC, "100000");
    assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Buy, "6", "100000")).price, f("100011.666666667"));
}

#[test]
fn counted_volume_leaves_the_window_after_thirty_days() {
    let (mut h, client, _) = funded_at(V59);
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    let matured = h.ts + MIN_HOLD_MS;
    let last = (day_of(matured) + 30) * DAY_MS - 1;
    tick_at(&mut h, last);
    assert_eq!(volumes(&h, &client).0, buy.price);
    tick_at(&mut h, last + 1);
    assert_eq!(volumes(&h, &client).0, Fixed::ZERO);
    // A change to the book drops the days before the window.
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "0.5", "100000"));
    assert!(h.engine.account_volume(client.account).unwrap().days.is_empty());
}

#[test]
fn a_close_out_counts_what_it_closes_at_once() {
    let (mut h, client, _) = funded_at(V59);
    let buy = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "9", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    let events = h.price(BTC, "90000");
    let closed: Vec<Fill> = fills(&events).into_iter().cloned().collect();
    let [out] = &closed[..] else { panic!("one close-out fill: {events:?}") };
    assert!(out.liquidation && out.position.is_zero(), "{out:?}");
    let opened = buy.price.mul_rounded(f("9"), Rounding::Down);
    let counted = out.price.mul_rounded(f("9"), Rounding::Down);
    assert_eq!(volumes(&h, &client), (opened + counted, Fixed::ZERO));
}

#[test]
fn a_resting_order_that_triggers_counts_as_its_fill_does() {
    let (mut h, client, _) = funded_at(V59);
    let events = rest(&mut h, &client, BTC, Side::Buy, "1", OrderKind::Limit { price: f("99000") });
    assert!(events.iter().any(|event| matches!(event, Event::OrderAccepted { .. })), "{events:?}");
    let events = h.price(BTC, "98000");
    let fill = only_fill(&events).clone();
    tick_after(&mut h, MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (fill.price, Fixed::ZERO));
}

#[test]
fn activation_starts_every_account_at_tier_zero_and_holds_each_position_pending_for_a_minute() {
    // A long held 15 minutes takes four times as much again just before the activation: no record says
    // which units were held a minute, so the whole position is pending for the activation's first minute.
    let (mut h, client, _) = funded_at(V58);
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "0.2", "100000"));
    h.advance(900_000);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "0.8", "100000"));
    assert!(h.engine.params().unwrap().volume_tiers.is_empty());
    h.engine.state_root();
    h.admin(AdminCommand::ActivateProtocol { version: V59 });
    let activated = h.ts;
    let root = h.engine.state_root();
    let mut rebuilt: Engine = serde_json::from_value(serde_json::to_value(&h.engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), root);
    assert_eq!(h.engine.params().unwrap().volume_tiers, launch_volume_tiers());
    assert_eq!(h.engine.operator_bounds().launch_volume_tiers, Some(launch_volume_tiers()));
    assert_eq!((volumes(&h, &client), standing(&h, &client).tier), ((Fixed::ZERO, Fixed::ZERO), 0));
    // Half closed at once counts nothing, and the half closed after the minute counts.
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "0.5", "100000"));
    tick_at(&mut h, activated + MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (Fixed::ZERO, Fixed::ZERO));
    h.price(BTC, "100000");
    let sell = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "0.5", "100000")).clone();
    tick_after(&mut h, HORIZON_MS);
    assert_eq!(volumes(&h, &client), (sell.price.mul(f("0.5")), Fixed::ZERO));
    // From `V59` parameters name a table.
    let mut params = h.engine.params().unwrap().clone();
    params.volume_tiers.clear();
    let refused = h.operate(AdminCommand::SetParams(params));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { reason }) if reason.contains("volume_tiers")));
}

#[test]
fn a_reopening_at_another_index_uses_up_the_close_it_takes_so_a_later_round_trip_cannot_take_it_back() {
    // From a matured long: sell at A, buy at B, sell at A, buy at A, all within the horizon. The
    // buy at B takes the first close's leg; the buy at A is a round trip with the second sell, which
    // closed the buy at B's pending lot, so it takes back nothing and is pending itself.
    let (mut h, client, _) = funded_at(V59);
    let open = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    let mut fills = Vec::new();
    for (side, mid) in [(Side::Sell, "100000"), (Side::Buy, "100100"), (Side::Sell, "100000"), (Side::Buy, "100000")] {
        h.price(BTC, mid);
        fills.push(only_fill(&trade(&mut h, &client, BTC, side, "1", mid)).clone());
        h.advance(500);
    }
    // A close now takes the last buy's pending lot and counts nothing; the first close counts.
    h.price(BTC, "100200");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100200"));
    tick_after(&mut h, MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (open.price + fills[0].price, Fixed::ZERO));
    assert!(h.engine.account_volume(client.account).unwrap().opening.is_empty());

    // From a matured long: sell at A, buy at B, then buy at A. The buy at B used up the sell's leg, so
    // the buy at A is no round trip: the sell counts, and both buys are pending, closing as such.
    let (mut h, client, _) = funded_at(V59);
    let open = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    let mut fills = Vec::new();
    for (side, mid) in [(Side::Sell, "100000"), (Side::Buy, "100100"), (Side::Buy, "100000")] {
        h.price(BTC, mid);
        fills.push(only_fill(&trade(&mut h, &client, BTC, side, "1", mid)).clone());
        h.advance(500);
    }
    h.price(BTC, "100200");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "2", "100200"));
    tick_after(&mut h, MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (open.price + fills[0].price, Fixed::ZERO));
}

#[test]
fn a_round_trip_through_a_flip_counts_nothing_and_leaves_the_long_matured() {
    // A matured long of 1 flips to a short of 1 and back at one index: the flip's close is taken back
    // by the second flip's reopening, and its opening is taken by that flip's close while pending.
    let (mut h, client, _) = funded_at(V59);
    let open = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    h.advance(MIN_HOLD_MS);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Sell, "2", "100000"));
    h.advance(1_000);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "2", "100000"));
    tick_after(&mut h, MIN_HOLD_MS);
    assert_eq!(volumes(&h, &client), (open.price, Fixed::ZERO));
    let book = h.engine.account_volume(client.account).unwrap();
    assert!(book.opening.is_empty() && book.closing.is_empty(), "{book:?}");
    // The long is matured: closing it counts.
    h.price(BTC, "100000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "100000")).clone();
    tick_after(&mut h, HORIZON_MS);
    assert_eq!(volumes(&h, &client), (open.price + close.price, Fixed::ZERO));
}

#[test]
fn pending_quantity_never_exceeds_the_position_through_adds_closes_and_flips() {
    let (mut h, client, _) = funded_at(V59);
    let steps = [
        (Side::Buy, "3", 0),
        (Side::Buy, "2", 20_000),
        (Side::Sell, "1", 1_000),
        (Side::Sell, "6", 50_000),
        (Side::Buy, "1", 2_000),
        (Side::Buy, "4", 70_000),
        (Side::Sell, "5", 3_000),
        (Side::Buy, "2", 1_000),
    ];
    for (n, (side, qty, wait)) in steps.into_iter().enumerate() {
        h.advance(wait);
        h.price(BTC, if n % 2 == 0 { "100000" } else { "100050" });
        only_fill(&trade(&mut h, &client, BTC, side, qty, "100000"));
        let held = h.engine.accounts()[&client.account].position(BTC).qty.abs();
        let pending: Fixed = h
            .engine
            .account_volume(client.account)
            .and_then(|book| book.opening.get(&BTC))
            .map_or(Fixed::ZERO, |lots| lots.iter().map(|lot| lot.qty).sum());
        assert!(pending <= held, "step {n}: {pending} pending of {held} held");
    }
}

#[test]
fn a_split_counts_the_matured_lots_before_it_scales_the_position() {
    // A two-for-one split doubles a unit; one new for two billion old takes the position to none.
    for (new_units, old_units) in [(2, 1), (1, 2_000_000_000)] {
        let (mut h, client, _) = funded_at(V59);
        h.price(AAPL, "80");
        let buy = only_fill(&trade(&mut h, &client, AAPL, Side::Buy, "1", "80")).clone();
        let split = CorporateAction {
            id: format!("split-{new_units}-{old_units}"),
            market: AAPL,
            previous_close: ny(2026, 10, 20, 16, 0),
            ex_open: ny(2026, 10, 21, 9, 30),
            adjustment: Adjustment::Split { new_units, old_units },
        };
        assert!(h.operate(AdminCommand::ScheduleCorporateAction(split)).contains(&Event::AdminApplied));
        let counted = buy.price;
        tick_at(&mut h, ny(2026, 10, 20, 15, 0));
        assert_eq!(volumes(&h, &client), (counted, Fixed::ZERO));
        h.advance_to(ny(2026, 10, 21, 9, 30));
        let events = h.cmd(Command::Tick);
        assert!(events.iter().any(|event| matches!(event, Event::CorporateSplit { .. })), "{events:?}");
        // The lot matured long before the split, so it counted, whatever the position became.
        assert_eq!(volumes(&h, &client), (counted, Fixed::ZERO), "{new_units} for {old_units}");
        let book = h.engine.account_volume(client.account).unwrap();
        assert!(book.opening.is_empty() && book.days.len() == 1, "{book:?}");
    }
}

#[test]
fn the_state_root_commits_each_volume_book_beside_the_leaves_the_exit_hashes() {
    let (mut h, client, _) = funded_at(V59);
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    let root = h.engine.state_root();
    let state = serde_json::to_value(&h.engine).unwrap();
    let mut rebuilt: Engine = serde_json::from_value(state.clone()).unwrap();
    assert_eq!(rebuilt.state_root(), root);
    let account = client.account.to_string();
    let mut moved = state.clone();
    moved["volumes"][&account]["opening"][BTC.to_string()][0]["notional"] = serde_json::json!("1");
    let mut moved: Engine = serde_json::from_value(moved).unwrap();
    assert_ne!(moved.state_root(), root);
    // The exit's proofs hold beside the volume leaf, with and without principal awaiting finality.
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

/// `client`'s volume standing.
fn standing(h: &Harness, client: &Client) -> VolumeStanding {
    h.engine.volume_standing(client.account).expect("from V59")
}

/// `client`'s counted and pending volume.
fn volumes(h: &Harness, client: &Client) -> (Fixed, Fixed) {
    let standing = standing(h, client);
    (standing.matured, standing.pending)
}

/// Sequences a tick at `ts`.
fn tick_at(h: &mut Harness, ts: Millis) {
    h.advance_to(ts);
    h.cmd(Command::Tick);
}

/// Sequences a tick `ms` from now.
fn tick_after(h: &mut Harness, ms: Millis) {
    h.advance(ms);
    h.cmd(Command::Tick);
}
