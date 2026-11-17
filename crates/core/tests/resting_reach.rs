//! `V74`'s re-check of only the resting limits a price could fill, through `Engine::apply`: a limit is
//! keyed at the index mid where its market's least fill rate, the lower of its least spread band and
//! its close-spread ceiling, first lets it fill. Each limit still fills on the first price its fill
//! price reaches; a limit refused before its price is checked is cancelled only once a price comes
//! within its reach, and a stop still triggers at its price; the stress refresh a visit ran moves to
//! the next fill, close-out or the command's end; and the keys follow bands raised above the ceiling
//! and bands lowered again. `V73` keeps the earlier visits. No fill pays less than the rate the keys
//! take (`resting_reach/least_rate.rs`).

mod common;
#[path = "resting_reach/least_rate.rs"]
mod least_rate;

use common::close_spread::{rest, rest_as, source_key, trade};
use common::events::fills;
use common::spreads::{spread_notice, tenths, with_spreads};
use common::toxicity::v54_params;
use common::weekend::client;
use common::{BTC, CHAIN_ID, Client, Harness, VAULT, f, market, operator};
use engine_core::Engine;
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{CancelReason, Event, Fill};
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::params::{ProtocolParams, StressLevel};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::toxicity::ScoreTier;
use engine_core::types::{AssetClass, MarketId, OrderId, Side};
use engine_core::volume_tier::launch_volume_tiers;

const V73: ProtocolVersion = ProtocolVersion::V73;
const V74: ProtocolVersion = ProtocolVersion::V74;
/// A crypto market whose first band charges 1bp to $1m and 100% above it.
const WIDE: MarketId = 44;
/// A crypto market with one band, which a notice raises above its ceiling.
const TIGHT: MarketId = 45;

#[test]
fn each_limit_fills_on_the_first_price_its_fill_price_reaches_as_before() {
    // BTC's first band charges 1bp: a buy at 99995 fills once the mid is 99985 (99994.9985), not at
    // 99990 (99999.999); the sell at 100005 then closes it once the mid is 100016 (100005.9984).
    let mids = ["99995", "99990", "99985", "100005", "100010", "100015", "100016"];
    let runs: Vec<Vec<Vec<Fill>>> = [V73, V74]
        .into_iter()
        .map(|version| {
            let mut h = genesis(version);
            let a = client(&mut h, 1, "1000000");
            h.price(BTC, "100000");
            resting(&rest(&mut h, &a, BTC, Side::Buy, "1", limit("99995")));
            resting(&rest(&mut h, &a, BTC, Side::Sell, "1", limit("100005")));
            mids.iter().map(|mid| fills(&h.price(BTC, mid)).into_iter().cloned().collect()).collect()
        })
        .collect();
    assert_eq!(runs[0], runs[1]);
    let prices: Vec<Vec<_>> = runs[1].iter().map(|step| step.iter().map(|fill| fill.price).collect()).collect();
    let expected = [vec![], vec![], vec![f("99994.9985")], vec![], vec![], vec![], vec![f("100005.9984")]];
    assert_eq!(prices, expected);
}

#[test]
fn a_limit_refused_before_its_price_is_checked_is_cancelled_once_a_price_reaches_it() {
    // WIDE's 1bp first band keys a buy at 99990 at 99980.002 and a sell at 100010 at 100020.002:
    // 99988 and 100012 pass the price but not that key, 99975 and 100025 pass both.
    let cases: [(&str, Side, Place); 5] = [
        ("a reduce-only limit whose position has gone", Side::Sell, |h, a| {
            trade(h, a, WIDE, Side::Buy, "1", "100000");
            let id = resting(&rest_as(h, a, WIDE, Side::Sell, "1", limit("100010"), true));
            trade(h, a, WIDE, Side::Sell, "1", "100000");
            id
        }),
        ("a reversal over the ceiling", Side::Sell, |h, a| {
            trade(h, a, WIDE, Side::Buy, "1", "100000");
            resting(&rest(h, a, WIDE, Side::Sell, "1001", limit("100010")))
        }),
        ("new risk in a reduce-only market", Side::Buy, |h, a| {
            let id = resting(&rest(h, a, WIDE, Side::Buy, "1", limit("99990")));
            h.operator_admin(AdminCommand::SetReduceOnly { market: Some(WIDE), reduce_only: true });
            id
        }),
        ("new risk below the minimum notional", Side::Buy, |h, a| {
            let id = resting(&rest(h, a, WIDE, Side::Buy, "0.01", limit("99990")));
            let spec = MarketSpec { min_order_notional: f("5000"), ..h.engine.markets()[&WIDE].spec.clone() };
            h.operator_admin(AdminCommand::UpsertMarket(spec));
            id
        }),
        ("a spread at its size that reaches the price", Side::Buy, |h, a| {
            // At level 1's 2x multiplier, 1000 units through the 100% band cost about twice the price.
            h.operator_admin(AdminCommand::SetStressFloor { market: Some(WIDE), level: 1 });
            resting(&rest(h, a, WIDE, Side::Buy, "1000", limit("99990")))
        }),
    ];
    for (case, side, place) in cases {
        let (out_of_reach, in_reach) = if side == Side::Buy { ("99988", "99975") } else { ("100012", "100025") };
        for version in [V73, V74] {
            let mut h = genesis(version);
            let a = client(&mut h, 1, "10000000");
            h.price(WIDE, "100000");
            let id = place(&mut h, &a);
            let events = h.price(WIDE, out_of_reach);
            assert!(fills(&events).is_empty(), "{case} at {version:?}: {events:?}");
            if version == V73 {
                assert!(cancelled(&events, id), "{case} at {version:?}: {events:?}");
                continue;
            }
            assert!(!cancelled(&events, id) && h.engine.accounts()[&a.account].orders.contains_key(&id), "{case}");
            let events = h.price(WIDE, in_reach);
            assert!(cancelled(&events, id), "{case} at {version:?}: {events:?}");
            // A reversal over the ceiling fills its closing slice first, as it does on any visit.
            let closed: Vec<_> = fills(&events).iter().map(|fill| (fill.order_id, fill.qty)).collect();
            let expected = if case == "a reversal over the ceiling" { vec![(id, f("1"))] } else { vec![] };
            assert_eq!(closed, expected, "{case}: {events:?}");
        }
    }
}

#[test]
fn a_reduce_only_limit_left_resting_fills_once_its_position_is_back() {
    // `V73` cancels the limit on the first price past 100010 after the position closed; `V74` keeps
    // it out of reach, so it closes the reopened long once the mid reaches its fill price.
    for (version, fills_at_reach) in [(V73, false), (V74, true)] {
        let mut h = genesis(version);
        let a = client(&mut h, 1, "1000000");
        h.price(WIDE, "100000");
        trade(&mut h, &a, WIDE, Side::Buy, "1", "100000");
        let id = resting(&rest_as(&mut h, &a, WIDE, Side::Sell, "1", limit("100010"), true));
        trade(&mut h, &a, WIDE, Side::Sell, "1", "100000");
        h.price(WIDE, "100012");
        trade(&mut h, &a, WIDE, Side::Buy, "1", "100012");
        let closed: Vec<_> = fills(&h.price(WIDE, "100025")).iter().map(|fill| fill.order_id).collect();
        assert_eq!(closed == [id], fills_at_reach, "{version:?}: {closed:?}");
        assert_eq!(h.engine.accounts()[&a.account].positions.contains_key(&WIDE), !fills_at_reach);
    }
}

#[test]
fn a_stop_still_fills_or_is_cancelled_on_the_first_price_past_its_trigger() {
    for version in [V73, V74] {
        let mut h = genesis(version);
        let a = client(&mut h, 1, "1000000");
        h.price(WIDE, "100000");
        trade(&mut h, &a, WIDE, Side::Buy, "1", "100000");
        let stop = OrderKind::Stop { price: f("99990") };
        let protective = resting(&rest(&mut h, &a, WIDE, Side::Sell, "1", stop));
        let orphan = resting(&rest_as(&mut h, &a, WIDE, Side::Sell, "1", stop, true));
        let events = h.price(WIDE, "99989");
        // The first stop closes the long, so the reduce-only one behind it has nothing to reduce.
        assert_eq!(fills(&events).iter().map(|fill| fill.order_id).collect::<Vec<_>>(), [protective], "{version:?}");
        assert!(cancelled(&events, orphan), "{version:?}: {events:?}");
    }
}

#[test]
fn a_stress_change_a_visit_ran_moves_to_the_close_out_that_follows() {
    // A 6% fall raises BTC to level 1 and closes out a 10x long. `V73` visits the limit at 94005 the
    // fall passes but its 7bp spread at level 1 keeps out of reach, and refreshes stress there,
    // before the close-out; `V74` refreshes it at the close-out's fill. The fills are the same.
    let mut runs = Vec::new();
    for version in [V73, V74] {
        let mut h = genesis(version);
        let (a, b) = (client(&mut h, 1, "1000000"), client(&mut h, 2, "10100"));
        h.price(BTC, "100000");
        trade(&mut h, &b, BTC, Side::Buy, "1", "100000");
        resting(&rest(&mut h, &a, BTC, Side::Buy, "1", limit("94005")));
        let events = h.price(BTC, "94000");
        let at = |kind: fn(&Event) -> bool| events.iter().position(kind).expect("emitted");
        let stressed = at(|e| matches!(e, Event::StressChanged { market: BTC, level: 1 }));
        let closed_out = at(|e| matches!(e, Event::CloseOut { .. }));
        assert_eq!(stressed < closed_out, version == V73, "{version:?}: {events:?}");
        assert_eq!(h.engine.accounts()[&a.account].orders.len(), 1, "{version:?}");
        runs.push(fills(&events).into_iter().cloned().collect::<Vec<_>>());
    }
    assert_eq!(runs[0].len(), 1);
    assert_eq!(runs[0], runs[1]);
}

#[test]
fn the_keys_follow_a_ceiling_below_raised_bands_and_lowered_bands() {
    let mut h = tight();
    let a = client(&mut h, 1, "10000000");
    h.price(TIGHT, "100000");
    trade(&mut h, &a, TIGHT, Side::Buy, "1", "100000");
    // A ceiling adopted at the 6bp launch close charge, then bands raised from 5bp to 10bp by notice:
    // a close clamps to the ceiling below the bands, so a sell at 99935 is keyed at 99995, not at
    // 100035 as 10bp would key it, and fills at 99996, at the ceiling.
    let effective_at = h.ts + NOTICE_MS;
    let ceiling = 6;
    let adoption = AdminCommand::AdoptCloseSpreadCeiling {
        market: TIGHT,
        max_close_spread_bps: ceiling,
        effective_at,
        listing: None,
    };
    h.operator_admin(adoption);
    h.advance_to(effective_at);
    relist(&mut h, tenths(&[(None, 100)]));
    h.price(TIGHT, "99800");
    let first = resting(&rest(&mut h, &a, TIGHT, Side::Sell, "1", limit("99935")));
    assert!(fills(&h.price(TIGHT, "99990")).is_empty());
    let filled: Vec<_> =
        fills(&h.price(TIGHT, "99996")).iter().map(|fill| (fill.order_id, fill.price, fill.spread_bps)).collect();
    assert_eq!(filled, [(first, f("99936.0024"), ceiling)]);
    // A limit keyed at the ceiling is keyed again at 5bp once the bands are lowered back to it, so
    // 99996 reaches a sell at 99945 (99946.002), which the ceiling keys at 100005.
    h.price(TIGHT, "100000");
    trade(&mut h, &a, TIGHT, Side::Buy, "1", "100000");
    h.price(TIGHT, "99800");
    let second = resting(&rest(&mut h, &a, TIGHT, Side::Sell, "1", limit("99945")));
    relist(&mut h, tenths(&[(None, 50)]));
    h.price(TIGHT, "99800");
    let filled: Vec<_> = fills(&h.price(TIGHT, "99996")).iter().map(|fill| (fill.order_id, fill.price)).collect();
    assert_eq!(filled, [(second, f("99946.002"))]);
    // A node restarting at any command rebuilds the same keys.
    let mut cached = Engine::new();
    let mut restored = Engine::new();
    for command in &h.log {
        restored = serde_json::from_slice(&serde_json::to_vec(&restored).unwrap()).unwrap();
        assert_eq!(cached.apply(command).unwrap(), restored.apply(command).unwrap(), "seq {}", command.seq);
        assert_eq!(cached.state_root(), restored.state_root());
    }
}

// ── Helpers ──

/// Rests a limit for a case, and returns its id.
type Place = fn(&mut Harness, &Client) -> OrderId;

/// A log that activates `version` before its genesis: the harness parameters in `V54`'s form with
/// the launch volume tiers, BTC-USD at 1bp to $1m, 2bp to $5m and 5bp above and WIDE, each 24/7 at
/// 10% margin with a 1bp markout floor, a 100% slippage bound and a 20bp ceiling, and the access policy.
fn genesis(version: ProtocolVersion) -> Harness {
    let params = ProtocolParams { volume_tiers: launch_volume_tiers(), ..v54_params() };
    let btc = tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 50)]);
    let wide = tenths(&[(Some(1_000_000), 10), (None, 100_000)]);
    listed(version, params, &[(BTC, "BTC-USD", btc, 20), (WIDE, "WIDE", wide, 20)])
}

/// `V74`'s genesis listing TIGHT alone at 5bp with a 25bp ceiling, under parameters whose stress and
/// toxicity charges are as low as allowed, 1x and a 1bp tier, so its launch close charge is 6bp.
fn tight() -> Harness {
    let mut params = ProtocolParams { volume_tiers: launch_volume_tiers(), ..v54_params() };
    params.stress_levels = vec![StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None }];
    params.toxicity.score_tiers = vec![ScoreTier { min_score: f("4"), extra_spread_bps: 1 }];
    listed(V74, params, &[(TIGHT, "TIGHT", tenths(&[(None, 50)]), 25)])
}

/// Client auth, the operator, `version`, `params`, a 4.3% SOFR, each of `markets` with its bands and
/// ceiling, 100m of house capital, the ceilings and the access policy.
fn listed(
    version: ProtocolVersion,
    params: ProtocolParams,
    markets: &[(MarketId, &str, Vec<SpreadTier>, i64)],
) -> Harness {
    let mut h = Harness::empty();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.admin(AdminCommand::SetParams(params));
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });
    for (id, symbol, spread_tiers, _) in markets {
        let spec = market(*id, symbol, AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
        let spread_tiers = spread_tiers.clone();
        let spec = MarketSpec { spread_tiers, markout_floor_bps: Some(f("1")), max_slippage_bps: 10_000, ..spec };
        h.admin(AdminCommand::UpsertMarket(spec));
    }
    h.cmd(Command::HouseCapital { amount: h.house_capital, chain_ref: "genesis".into() });
    let ceilings =
        markets.iter().map(|(market, _, _, bps)| CloseSpreadCeiling { market: *market, max_close_spread_bps: *bps });
    h.admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings.collect() });
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h
}

/// Relists TIGHT with `bands` after their seven-day notice.
fn relist(h: &mut Harness, bands: Vec<SpreadTier>) {
    let (notice, effective_at) = spread_notice(h, TIGHT, bands.clone());
    h.operator_admin(notice);
    h.advance_to(effective_at);
    h.operator_admin(AdminCommand::UpsertMarket(with_spreads(h, TIGHT, bands)));
}

/// A limit at `price`.
fn limit(price: &str) -> OrderKind {
    OrderKind::Limit { price: f(price) }
}

/// The id of the order `events` accepted, which rests: nothing filled or cancelled it.
fn resting(events: &[Event]) -> OrderId {
    assert!(fills(events).is_empty(), "{events:?}");
    let accepted = events.iter().find_map(|e| match e {
        Event::OrderAccepted { order_id, .. } => Some(*order_id),
        _ => None,
    });
    let id = accepted.unwrap_or_else(|| panic!("accepted: {events:?}"));
    assert!(!cancelled(events, id), "{events:?}");
    id
}

/// Whether `events` cancel order `id` as unfillable.
fn cancelled(events: &[Event], id: OrderId) -> bool {
    events.iter().any(
        |e| matches!(e, Event::OrderCancelled { order_id, reason: CancelReason::Unfillable, .. } if *order_id == id),
    )
}
