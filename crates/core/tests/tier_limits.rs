//! `V66`'s request and order limits, through `Engine::apply`: an account rests at most its volume tier's
//! limit, in place of the parameters' cap, and a new resting order past it is refused naming the tier
//! while orders already resting when the tier falls stay; the tier's request rate is what the node
//! admits the account's signed requests at; a table naming no limit leaves the parameters' cap; and the
//! activation gives a log's table its limits by position. Before `V66` a table may name none. The
//! limits' bounds are in `tier_limit_bounds.rs`.

mod common;

use common::close_spread::{fund, rest, trade};
use common::events::{only_fill, rejection};
use common::weekend::{GOLD, client, genesis_with_tiers, globex};
use common::{BTC, Client, Harness, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::params::{DEFAULT_MAX_RESTING_ORDERS, ProtocolParams};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{OrderId, Side};
use engine_core::volume_tier::{
    ACTIVATION_MAX_RESTING_ORDERS, ACTIVATION_REQUESTS_PER_SECOND, DAY_MS, MIN_HOLD_MS, RequestLimit, VolumeTier,
    day_of,
};

const V64: ProtocolVersion = ProtocolVersion::V64;
const V66: ProtocolVersion = ProtocolVersion::V66;

#[test]
fn a_fresh_wallet_rests_twenty_five_orders_and_the_next_is_refused_naming_its_tier() {
    let (mut h, a) = genesis(V66, true);
    let b = client(&mut h, 2, "10000");
    for _ in 0..25 {
        accepted(&bid(&mut h, &a));
    }
    let refusal = rejection(&bid(&mut h, &a)).clone();
    assert_eq!(refusal, Reject::TierOrderLimit { tier: 0, max: 25 });
    assert_eq!(h.engine.accounts()[&a.account].orders.len(), 25);
    // A stop counts the same as a limit.
    let stop = rest(&mut h, &a, BTC, Side::Sell, "0.01", OrderKind::Stop { price: f("90000") });
    assert_eq!(rejection(&stop), &refusal);
    // The limit is per account, and a market order is never limited.
    accepted(&bid(&mut h, &b));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "1", "2000"));
}

#[test]
fn each_tier_rests_up_to_its_own_limit() {
    let (mut h, a) = genesis(V66, true);
    reach_tier_one(&mut h, &a);
    assert_eq!(h.engine.volume_tier(a.account), 1);
    for _ in 0..50 {
        accepted(&bid(&mut h, &a));
    }
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TierOrderLimit { tier: 1, max: 50 });
    // $488k counted reaches tier 2, whose limit is 100.
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "145", "2000"));
    mature(&mut h);
    assert_eq!(h.engine.volume_tier(a.account), 2);
    for _ in 50..100 {
        accepted(&bid(&mut h, &a));
    }
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TierOrderLimit { tier: 2, max: 100 });
    // $608k counted reaches tier 3, whose limit of 250 is above the parameters' cap of 100, which it
    // replaces rather than joins.
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "60", "2000"));
    mature(&mut h);
    assert_eq!(h.engine.volume_tier(a.account), 3);
    assert_eq!(h.engine.params().unwrap().max_resting_orders, DEFAULT_MAX_RESTING_ORDERS);
    for _ in 100..250 {
        accepted(&bid(&mut h, &a));
    }
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TierOrderLimit { tier: 3, max: 250 });
    assert_eq!(h.engine.accounts()[&a.account].orders.len(), 250);
}

#[test]
fn orders_resting_when_the_tier_falls_stay_and_only_a_new_one_is_refused() {
    let (mut h, a) = genesis(V66, true);
    reach_tier_one(&mut h, &a);
    let ids: Vec<OrderId> = (0..30).map(|_| accepted(&bid(&mut h, &a))).collect();
    // Thirty days on the counted volume has left the window, and tier 0's limit is 25.
    h.advance_to((day_of(h.ts) + 30) * DAY_MS);
    h.cmd(Command::Tick);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(a.account), 0);
    let resting = &h.engine.accounts()[&a.account].orders;
    assert_eq!(resting.keys().copied().collect::<Vec<_>>(), ids);
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TierOrderLimit { tier: 0, max: 25 });
    assert_eq!(h.engine.accounts()[&a.account].orders.len(), 30);
}

#[test]
fn a_table_naming_no_limit_leaves_the_parameters_cap_and_the_nodes_rate() {
    let (mut h, a) = genesis(V66, false);
    assert!(h.engine.params().unwrap().volume_tiers.iter().all(|tier| tier.max_resting_orders == 0));
    for _ in 0..DEFAULT_MAX_RESTING_ORDERS {
        accepted(&bid(&mut h, &a));
    }
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TooManyOrders { max: DEFAULT_MAX_RESTING_ORDERS });
    assert_eq!(h.engine.request_limit(a.account), None);
    let standing = h.engine.volume_standing(a.account).unwrap();
    assert_eq!((standing.max_resting_orders, standing.requests_per_second), (None, None));
}

#[test]
fn before_v66_the_parameters_cap_applies_and_a_table_may_name_no_limit() {
    let (mut h, a) = genesis(V64, false);
    for _ in 0..DEFAULT_MAX_RESTING_ORDERS {
        accepted(&bid(&mut h, &a));
    }
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TooManyOrders { max: DEFAULT_MAX_RESTING_ORDERS });
    assert_eq!(h.engine.request_limit(a.account), None);
    let standing = h.engine.volume_standing(a.account).unwrap();
    assert_eq!((standing.max_resting_orders, standing.requests_per_second), (None, None));
    let named = ProtocolParams { volume_tiers: tiers(true), ..h.engine.params().unwrap().clone() };
    let events = h.operate(AdminCommand::SetParams(named));
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("needs protocol V66")));
}

#[test]
fn the_request_rate_and_the_account_read_follow_the_tier() {
    let (mut h, a) = genesis(V66, true);
    assert_eq!(h.engine.request_limit(a.account), Some(RequestLimit { tier: 0, per_second: 2 }));
    let standing = h.engine.volume_standing(a.account).unwrap();
    assert_eq!((standing.max_resting_orders, standing.requests_per_second), (Some(25), Some(2)));
    reach_tier_one(&mut h, &a);
    assert_eq!(h.engine.request_limit(a.account), Some(RequestLimit { tier: 1, per_second: 5 }));
    let standing = h.engine.volume_standing(a.account).unwrap();
    assert_eq!((standing.max_resting_orders, standing.requests_per_second), (Some(50), Some(5)));
}

#[test]
fn activation_gives_unnamed_limits_by_position_and_leaves_resting_orders() {
    let (mut h, a) = genesis(V64, false);
    for _ in 0..30 {
        accepted(&bid(&mut h, &a));
    }
    h.admin(AdminCommand::ActivateProtocol { version: V66 });
    let orders = |tiers: &[VolumeTier]| tiers.iter().map(|tier| tier.max_resting_orders).collect::<Vec<_>>();
    let rates = |tiers: &[VolumeTier]| tiers.iter().map(|tier| tier.requests_per_second).collect::<Vec<_>>();
    let launch = h.engine.operator_bounds().launch_volume_tiers.clone().unwrap();
    for tiers in [&h.engine.params().unwrap().volume_tiers, &launch] {
        assert_eq!(orders(tiers), ACTIVATION_MAX_RESTING_ORDERS);
        assert_eq!(rates(tiers), ACTIVATION_REQUESTS_PER_SECOND);
    }
    // The thirty orders stay, and a thirty-first is refused at tier 0's 25.
    assert_eq!(h.engine.accounts()[&a.account].orders.len(), 30);
    assert_eq!(rejection(&bid(&mut h, &a)), &Reject::TierOrderLimit { tier: 0, max: 25 });
    assert_eq!(h.engine.request_limit(a.account), Some(RequestLimit { tier: 0, per_second: 2 }));
}

// ── Helpers ──

/// A table whose tiers a few fills reach, each in a day: $100k counted reaches tier 1, $400k tier 2
/// and $600k tier 3. With `named`, it names the activation's limits and rates; without, none.
fn tiers(named: bool) -> Vec<VolumeTier> {
    [(0, 2_500), (100_000, 5_000), (400_000, 10_000), (600_000, 25_000)]
        .into_iter()
        .enumerate()
        .map(|(at, (usd, best_price_share_bps))| VolumeTier {
            threshold: Fixed::from_int(usd),
            day_divisor: 1,
            best_price_share_bps,
            margin_floor_bps: 0,
            max_resting_orders: if named { ACTIVATION_MAX_RESTING_ORDERS[at] } else { 0 },
            requests_per_second: if named { ACTIVATION_REQUESTS_PER_SECOND[at] } else { 0 },
        })
        .collect()
}

/// A log that activates `version` before its genesis with `tiers(named)`, client 1 holding $10,000, and
/// BTC at $100,000 and gold at $2,000.
fn genesis(version: ProtocolVersion, named: bool) -> (Harness, Client) {
    let mut h = genesis_with_tiers(version, globex(vec![]), 0, 0, tiers(named));
    let a = client(&mut h, 1, "10000");
    fund(&mut h, &a, "90000");
    h.price(GOLD, "2000");
    h.price(BTC, "100000");
    (h, a)
}

/// Opens $198k of gold, which counts once held and reaches tier 1.
fn reach_tier_one(h: &mut Harness, a: &Client) {
    only_fill(&trade(h, a, GOLD, Side::Sell, "99", "2000"));
    mature(h);
}

/// Lets the opening notional count, with gold and BTC at their prices.
fn mature(h: &mut Harness) {
    h.advance(MIN_HOLD_MS);
    h.price(GOLD, "2000");
    h.price(BTC, "100000");
}

/// A buy limit well below BTC's price, which rests.
fn bid(h: &mut Harness, a: &Client) -> Vec<Event> {
    rest(h, a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("50000") })
}

/// The order id an `OrderAccepted` among `events` carries; panics if there is none.
fn accepted(events: &[Event]) -> OrderId {
    events
        .iter()
        .find_map(|event| match event {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected an accepted order, got {events:?}"))
}
