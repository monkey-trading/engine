//! The loss window's bookkeeping, driven directly at chosen NAVs; the trigger through `Engine::apply`
//! is in `tests/house_loss.rs`.

use super::*;
use crate::params::StressTrigger;

/// A slot boundary.
const T0: Millis = 2_000_000 * SLOT_MS;

#[test]
fn the_window_opens_at_the_result_at_activation() {
    let engine = opened(T0 + 1_000, 100);
    let slots: Vec<SlotResult> = engine.house_loss.slots.iter().copied().collect();
    assert_eq!(slots, [slot(2_000_000 - 1, 100, 100), slot(2_000_000, 100, 100)]);
    assert_eq!(engine.house_loss_at(usd(100)), Fixed::ZERO);
    // A fall in NAV in the first command after it is already a loss.
    assert_eq!(engine.house_loss_at(usd(90)), usd(10));
    assert_eq!(engine.house_loss_at(usd(130)), usd(-30));
}

#[test]
fn the_loss_runs_from_the_slot_holding_the_start_of_the_day_before() {
    let mut engine = opened(T0 + 5_000, 100);
    // One result per slot: 100 in the first, then 90, 70 and 40.
    for (k, nav) in [100, 90, 70, 40].into_iter().enumerate() {
        engine.now = T0 + k as Millis * SLOT_MS + 5_000;
        record(&mut engine, nav);
    }
    // Before the first slot's end is a day old, the window starts at the activation's 100.
    engine.now = T0 + SLOT_MS + WINDOW_MS - 1;
    assert_eq!(engine.house_loss_at(usd(40)), usd(60));
    // From then, from the higher of the slot before the start and the slot holding it.
    engine.now = T0 + SLOT_MS + WINDOW_MS;
    assert_eq!(engine.house_loss_at(usd(40)), usd(60));
    engine.now = T0 + 2 * SLOT_MS + WINDOW_MS;
    assert_eq!(engine.house_loss_at(usd(40)), usd(50));
    engine.now = T0 + 3 * SLOT_MS + WINDOW_MS - 1;
    assert_eq!(engine.house_loss_at(usd(40)), usd(50));
    engine.now = T0 + 3 * SLOT_MS + WINDOW_MS;
    assert_eq!(engine.house_loss_at(usd(40)), usd(30));
    engine.now = T0 + 4 * SLOT_MS + WINDOW_MS;
    assert_eq!(engine.house_loss_at(usd(40)), Fixed::ZERO);
}

#[test]
fn income_in_the_slot_holding_the_windows_start_hides_no_loss() {
    // 100 before the slot, then 200 from five minutes into it.
    let mut engine = opened(T0, 100);
    engine.now = T0 + SLOT_MS + 5 * 60_000;
    record(&mut engine, 200);
    // A day after ten minutes into the slot, the house is 10 down on the 200 it held a day before; the
    // 100 at the slot's start would read a 90 gain.
    engine.now = T0 + SLOT_MS + 10 * 60_000 + WINDOW_MS;
    record(&mut engine, 190);
    assert_eq!(engine.house_loss_at(usd(190)), usd(10));
    // A high the slot passed through counts too: the reading never falls below the day's loss.
    let mut engine = opened(T0, 100);
    engine.now = T0 + SLOT_MS + 60_000;
    record(&mut engine, 300);
    engine.now = T0 + SLOT_MS + 2 * 60_000;
    record(&mut engine, 150);
    engine.now = T0 + SLOT_MS + 10 * 60_000 + WINDOW_MS;
    assert_eq!(engine.house_loss_at(usd(190)), usd(110));
}

#[test]
fn the_window_keeps_at_most_a_day_of_slots() {
    let mut engine = opened(T0, 100);
    for k in 0..3 * (WINDOW_MS / SLOT_MS) {
        engine.now = T0 + k * SLOT_MS + 1;
        record(&mut engine, 100 + k);
        let kept = engine.house_loss.slots.len() as i64;
        assert!(kept <= WINDOW_MS / SLOT_MS + 2, "{kept} slots kept at slot {k}");
    }
    let newest = 100 + 3 * (WINDOW_MS / SLOT_MS) - 1;
    // The window runs from the slot holding its start, so it holds the 96 results after it, each a
    // dollar up.
    assert_eq!(engine.house_loss_at(usd(newest)), usd(-(WINDOW_MS / SLOT_MS)));
}

#[test]
fn a_gap_longer_than_the_window_measures_from_the_last_result_before_it() {
    let mut engine = opened(T0, 100);
    engine.now = T0 + 3 * WINDOW_MS;
    record(&mut engine, 80);
    assert_eq!(engine.house_loss_at(usd(80)), usd(20));
    assert_eq!(engine.house_loss.slots.len(), 2, "only the result before the gap and the current one");
}

#[test]
fn capital_moved_in_or_out_is_not_a_loss() {
    let mut engine = opened(T0, 100);
    engine.note_house_capital(usd(50));
    assert_eq!(engine.house_loss_at(usd(150)), Fixed::ZERO);
    // A distribution out of the house is not a loss to its clients either.
    engine.note_house_capital(usd(-30));
    assert_eq!(engine.house_loss_at(usd(120)), Fixed::ZERO);
    assert_eq!(engine.house_loss_at(usd(110)), usd(10));
    record(&mut engine, 110);
    assert_eq!(engine.house_loss.slots.back().map(|s| (s.result, s.high)), Some((usd(90), usd(100))));
}

#[test]
fn before_v59_nothing_is_recorded() {
    let mut engine = Engine { protocol_version: ProtocolVersion::V58, now: T0, ..Engine::default() };
    record(&mut engine, 100);
    engine.note_house_capital(usd(50));
    assert!(engine.house_loss.is_default());
    assert_eq!(engine.house_loss_at(usd(10)), Fixed::ZERO);
    assert_eq!(engine.house_loss(), None);
}

#[test]
fn the_loss_level_is_the_highest_level_whose_threshold_the_loss_is_above() {
    let levels = [level(None), level(Some(-5)), level(None), level(Some(0))];
    let cases = [(-10, 0), (-5, 0), (-4, 1), (0, 1), (1, 3), (1_000_000, 3)];
    for (loss, expected) in cases {
        assert_eq!(loss_level(&levels, usd(loss)), expected, "a loss of {loss}");
    }
    assert_eq!(loss_level(&[level(None), level(None)], usd(1_000_000)), 0);
    assert_eq!(
        loss_triggers(&levels),
        [LossTrigger { level: 1, house_loss: usd(-5) }, LossTrigger { level: 3, house_loss: Fixed::ZERO }]
    );
}

// ── Helpers ──

/// An engine under `V59` at `now`, whose window opened there with the house holding `nav`.
fn opened(now: Millis, nav: i64) -> Engine {
    let mut engine = Engine { protocol_version: ProtocolVersion::V59, now, house_cash: usd(nav), ..Engine::default() };
    engine.open_window();
    engine
}

fn record(engine: &mut Engine, nav: i64) {
    let mut events = Vec::new();
    engine.record_house_loss(usd(nav), &mut events);
    assert!(events.is_empty(), "no parameters, so no level: {events:?}");
}

fn usd(amount: i64) -> Fixed {
    Fixed::from_int(amount)
}

fn slot(slot: i64, result: i64, high: i64) -> SlotResult {
    SlotResult { slot, result: usd(result), high: usd(high) }
}

fn level(house_loss: Option<i64>) -> StressLevel {
    StressLevel {
        size_spread_multiplier_bps: 10_000,
        min_spread_bps: 0,
        trigger: house_loss.map(|loss| StressTrigger { house_loss: Some(usd(loss)), ..StressTrigger::default() }),
    }
}
