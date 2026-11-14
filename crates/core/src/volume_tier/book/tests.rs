use super::*;
use crate::volume_tier::DAY_MS;

/// Noon on day 100.
const NOON: Millis = 100 * DAY_MS + DAY_MS / 2;

#[test]
fn an_opening_lot_counts_on_the_day_it_matures_and_not_before() {
    let mut volume = AccountVolume::default();
    // Opened 30 s before midnight, it matures on the next day.
    let opened = 101 * DAY_MS - 30_000;
    volume.push_opening(1, opening(opened + MIN_HOLD_MS, "2", "200"));
    let before = volume.window(opened + MIN_HOLD_MS - 1);
    assert!(before.iter().all(Fixed::is_zero));
    assert_eq!(volume.pending(opened + MIN_HOLD_MS - 1), usd("200"));
    let at = opened + MIN_HOLD_MS;
    assert_eq!((volume.window(at)[WINDOW_DAYS - 1], volume.pending(at)), (usd("200"), Fixed::ZERO));
    volume.settle(at);
    assert_eq!(volume.days, [DayVolume { day: 101, volume: usd("200") }]);
    assert!(volume.opening.is_empty());
}

#[test]
fn a_closing_lot_counts_on_its_closing_day_once_the_horizon_has_passed() {
    let mut volume = AccountVolume::default();
    let closed_at = 101 * DAY_MS - 1_000;
    volume.push_closing(closing(1, Side::Sell, "100", closed_at, ["1", "1", "100"]));
    assert_eq!(volume.pending(closed_at + HORIZON_MS - 1), usd("100"));
    volume.settle(closed_at + HORIZON_MS);
    assert_eq!(volume.days, [DayVolume { day: 100, volume: usd("100") }]);
    assert!(volume.closing.is_empty());
}

#[test]
fn a_lot_that_counts_nothing_adds_no_day() {
    let mut volume = AccountVolume::default();
    volume.push_opening(1, opening(NOON, "1", "0"));
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["1", "0", "0"]));
    volume.settle(NOON + HORIZON_MS);
    assert_eq!((volume.days.len(), volume.opening.len(), volume.closing.len()), (0, 0, 0));
}

#[test]
fn settling_drops_the_days_before_the_window() {
    let mut volume = AccountVolume::default();
    for day in [70, 71, 99] {
        count(&mut volume.days, day, usd("1"));
    }
    let now = 100 * DAY_MS;
    assert_eq!(volume.window(now).iter().copied().sum::<Fixed>(), usd("2"));
    volume.settle(now);
    assert_eq!(volume.days.iter().map(|day| day.day).collect::<Vec<_>>(), [71, 99]);
}

#[test]
fn a_close_takes_the_newest_opening_lots_first_and_their_notional_falls_with_them() {
    let mut volume = AccountVolume::default();
    volume.push_opening(1, opening(NOON, "1", "100"));
    volume.push_opening(1, opening(NOON + 1, "3", "301"));
    assert_eq!(volume.consume_opening(1, usd("2")), usd("2"));
    // A third of the newest lot is left, its notional rounded down.
    assert_eq!(volume.opening[&1], [opening(NOON, "1", "100"), opening(NOON + 1, "1", "100.333333333")]);
    assert_eq!(volume.consume_opening(1, usd("5")), usd("2"));
    assert!(volume.opening.is_empty());
    assert_eq!(volume.consume_opening(2, usd("1")), Fixed::ZERO);
}

#[test]
fn lots_maturing_together_join_and_a_full_position_joins_its_newest_lot() {
    let mut volume = AccountVolume::default();
    volume.push_opening(1, opening(NOON, "1", "10"));
    volume.push_opening(1, opening(NOON, "2", "20"));
    assert_eq!(volume.opening[&1], [opening(NOON, "3", "30")]);
    for at in 1..MAX_OPENING_LOTS as i64 {
        volume.push_opening(1, opening(NOON + at, "1", "1"));
    }
    assert_eq!(volume.opening[&1].len(), MAX_OPENING_LOTS);
    // The newest lot takes the new one and its later maturity.
    volume.push_opening(1, opening(NOON + 500, "1", "1"));
    let lots = &volume.opening[&1];
    assert_eq!((lots.len(), lots[MAX_OPENING_LOTS - 1]), (MAX_OPENING_LOTS, opening(NOON + 500, "2", "2")));
}

#[test]
fn a_merged_lot_counts_each_unit_at_the_lower_notional_so_no_part_of_it_counts_more() {
    // A unit opened at $100 and one at $200 merge at $100 each; closing the newest leaves $100, not
    // the $150 an average would.
    let mut volume = AccountVolume::default();
    volume.push_opening(1, opening(NOON, "1", "100"));
    volume.push_opening(1, opening(NOON, "1", "200"));
    assert_eq!(volume.opening[&1], [opening(NOON, "2", "200")]);
    volume.consume_opening(1, usd("1"));
    assert_eq!(volume.opening[&1], [opening(NOON, "1", "100")]);
    // A closing lot that closed nothing matured adds its leg and no notional.
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["2", "0", "0"]));
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["1", "1", "300"]));
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["1", "1", "100"]));
    assert_eq!(volume.closing, [closing(1, Side::Sell, "100", NOON, ["4", "2", "200"])]);
}

#[test]
fn closes_at_one_index_and_time_join_and_past_the_cap_the_oldest_counts_nothing() {
    let mut volume = AccountVolume::default();
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["1", "1", "100"]));
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["2", "2", "200"]));
    assert_eq!(volume.closing, [closing(1, Side::Sell, "100", NOON, ["3", "3", "300"])]);
    for at in 1..=MAX_CLOSING_LOTS as i64 {
        volume.push_closing(closing(1, Side::Sell, "100", NOON + at, ["1", "1", "1"]));
    }
    assert_eq!((volume.closing.len(), volume.closing[0].closed_at), (MAX_CLOSING_LOTS, NOON + 1));
    volume.settle(NOON + MAX_CLOSING_LOTS as i64 + HORIZON_MS);
    assert_eq!(volume.days, [DayVolume { day: 100, volume: usd(&MAX_CLOSING_LOTS.to_string()) }]);
}

#[test]
fn a_reopening_takes_its_sides_legs_newest_first_and_takes_back_only_at_their_index() {
    let mut volume = AccountVolume::default();
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["2", "2", "200"]));
    volume.push_closing(closing(1, Side::Sell, "101", NOON + 1, ["1", "1", "101"]));
    volume.push_closing(closing(1, Side::Buy, "100", NOON + 2, ["1", "1", "100"]));
    volume.push_closing(closing(2, Side::Sell, "100", NOON + 3, ["1", "1", "100"]));
    volume.push_closing(closing(1, Side::Sell, "100", NOON + 4, ["1", "1", "100"]));
    // The newest takes back its matured unit; half the one at 101 goes unmatched and still counts.
    assert_eq!(volume.reopen(1, Side::Sell, usd("100"), usd("1.5")), usd("1"));
    let left: Vec<_> = volume.closing.iter().map(|lot| (lot.closed_at, lot.leg, lot.qty, lot.notional)).collect();
    assert_eq!(
        left,
        [
            (NOON, usd("2"), usd("2"), usd("200")),
            (NOON + 1, usd("0.5"), usd("1"), usd("101")),
            (NOON + 2, usd("1"), usd("1"), usd("100")),
            (NOON + 3, usd("1"), usd("1"), usd("100")),
        ]
    );
    // The rest of the 101 leg goes unmatched, and the oldest at 100 is taken back whole.
    assert_eq!(volume.reopen(1, Side::Sell, usd("100"), usd("9")), usd("2"));
    let left: Vec<_> = volume.closing.iter().map(|lot| (lot.closed_at, lot.leg, lot.qty)).collect();
    assert_eq!(left, [(NOON + 1, usd("0"), usd("1")), (NOON + 2, usd("1"), usd("1")), (NOON + 3, usd("1"), usd("1"))]);
}

#[test]
fn a_close_merged_after_its_matured_units_left_the_leg_gives_none_of_them_back() {
    // At one time: a matured close at 100, a reopening at 101 that takes its leg, and a close of
    // pending quantity at 100, which joins the first lot. A reopening at 100 then takes the pending
    // close's leg, and gives back nothing.
    let mut volume = AccountVolume::default();
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["1", "1", "100"]));
    assert_eq!(volume.reopen(1, Side::Sell, usd("101"), usd("1")), Fixed::ZERO);
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["1", "0", "0"]));
    let merged = volume.closing[0];
    assert_eq!((merged.leg, merged.matured_leg, merged.qty), (usd("1"), Fixed::ZERO, usd("1")));
    assert_eq!(volume.reopen(1, Side::Sell, usd("100"), usd("1")), Fixed::ZERO);
    assert_eq!((volume.closing[0].qty, volume.closing[0].notional), (usd("1"), usd("100")));
}

#[test]
fn a_leg_gives_up_its_pending_units_first_unmatched_and_its_matured_units_first_matched() {
    // A close of three units, two of them pending, then one taken at another index and two at its own.
    let mut volume = AccountVolume::default();
    volume.push_closing(closing(1, Side::Sell, "100", NOON, ["3", "1", "100"]));
    assert_eq!(volume.reopen(1, Side::Sell, usd("101"), usd("1")), Fixed::ZERO);
    assert_eq!((volume.closing[0].leg, volume.closing[0].qty), (usd("2"), usd("1")));
    assert_eq!(volume.reopen(1, Side::Sell, usd("100"), usd("2")), usd("1"));
    assert!(volume.closing.is_empty());
}

#[test]
fn a_split_scales_pending_lots_toward_zero_and_drops_one_scaled_to_nothing() {
    let account = AccountId([1; 20]);
    let mut engine = Engine::new();
    engine.volumes.entry(account).or_default().opening.insert(1, vec![opening(NOON, "3", "300")]);
    let lots = |engine: &Engine| engine.volumes.get(&account).map(|volume| volume.opening[&1].clone());
    engine.scale_volume_lots(account, 1, 2, 1);
    assert_eq!(lots(&engine), Some(vec![opening(NOON, "6", "300")]));
    engine.scale_volume_lots(account, 1, 1, 4);
    assert_eq!(lots(&engine), Some(vec![opening(NOON, "1.5", "300")]));
    engine.scale_volume_lots(account, 1, 1, 2_000_000_000);
    assert_eq!(engine.volumes.get(&account), None, "a book left with nothing is removed");
}

#[test]
fn a_days_volume_saturates_rather_than_overflowing() {
    let mut days = Vec::new();
    count(&mut days, 1, Fixed::from_raw(i128::MAX - 1));
    count(&mut days, 1, usd("5"));
    assert_eq!(days[0].volume, Fixed::from_raw(i128::MAX));
}

// ── Helpers ──

fn usd(s: &str) -> Fixed {
    s.parse().unwrap()
}

fn opening(matures_at: Millis, qty: &str, notional: &str) -> OpeningLot {
    OpeningLot { matures_at, qty: usd(qty), notional: usd(notional) }
}

/// A closing lot of `[leg, matured quantity, notional]`, every matured unit in the leg.
fn closing(market: MarketId, side: Side, index: &str, closed_at: Millis, legs: [&str; 3]) -> ClosingLot {
    let [leg, qty, notional] = legs.map(usd);
    ClosingLot { market, side, index: usd(index), closed_at, leg, matured_leg: qty.min(leg), qty, notional }
}
