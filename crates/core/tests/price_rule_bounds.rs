//! Price rule bounds from `V76`, through `Engine::apply`: a listed market's price rules loosen (fewer
//! sources, a longer freshness bound or a wider divergence bound) only as a seven-day listing notice
//! names them, and never past the market's launch rules, while tighter rules apply at once. So the
//! operator, who is also its enclave's host, cannot widen a market's freshness bound, hold the venue
//! streams back, and trade an old price against the house.

mod common;

use common::close_spread::trade;
use common::events::{fills, only_fill};
use common::toxicity::v54_params;
use common::weekend::{client, genesis_at, genesis_with_params, globex};
use common::{BTC, ETH, Harness};
use engine_core::command::AdminCommand;
use engine_core::corporate::LAUNCH_CORPORATE_BOUNDS;
use engine_core::event::Event;
use engine_core::holding_fee::DEFAULT_PROFIT_FEE_CURVE;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis, Side};
use engine_core::volume_tier::launch_volume_tiers;

const MINUTE_MS: Millis = 60_000;
const HOUR_MS: Millis = 60 * MINUTE_MS;

/// A fresh log at `version` with the launch fee curve and volume tiers, 100m of house capital
/// credited, and BTC at $100,000 from two venues under its listed rules: quotes at most a second old,
/// from at least two sources. Rotation is off, as at deployment.
fn genesis(version: ProtocolVersion) -> Harness {
    let params = ProtocolParams {
        volume_tiers: launch_volume_tiers(),
        profit_fee_curve: DEFAULT_PROFIT_FEE_CURVE.to_vec(),
        ..v54_params()
    };
    let mut h = genesis_with_params(version, globex(vec![]), 0, 0, params);
    h.price(BTC, "100000");
    h
}

/// `market`'s listing with `price_rules`.
fn relisting(h: &Harness, market: MarketId, price_rules: PriceRules) -> AdminCommand {
    AdminCommand::UpsertMarket(MarketSpec { price_rules, ..h.engine.markets()[&market].spec.clone() })
}

/// BTC's listing with quotes good for six hours, from one source, at any divergence.
fn loosened(h: &Harness) -> AdminCommand {
    relisting(h, BTC, PriceRules { max_age_ms: 6 * HOUR_MS, max_divergence_bps: 10_000, min_sources: 1 })
}

/// The rules BTC is listed with.
fn rules(h: &Harness, market: MarketId) -> PriceRules {
    h.engine.markets()[&market].spec.price_rules
}

/// The operator's notice of `market`'s new `price_rules` from `effective_at`.
fn rules_notice(market: MarketId, price_rules: PriceRules, effective_at: Millis) -> AdminCommand {
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: None,
        price_rules: Some(price_rules),
        effective_at,
    };
    AdminCommand::AnnounceListing { market, notice }
}

fn applied(events: &[Event]) -> bool {
    events.last() == Some(&Event::AdminApplied)
}

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

#[test]
fn a_held_markets_price_rules_do_not_loosen_without_notice_so_an_hour_old_price_fills_nothing() {
    let mut h = genesis(ProtocolVersion::V76);
    // A client holds BTC, so the market is held; the second account is the operator's own.
    let holder = client(&mut h, 1, "50000");
    only_fill(&trade(&mut h, &holder, BTC, Side::Buy, "1", "100000"));
    let own = client(&mut h, 2, "100000");

    // Control: under the listed rules a quote two seconds old prices nothing.
    h.advance(2_000);
    assert!(fills(&trade(&mut h, &own, BTC, Side::Buy, "5", "100000")).is_empty(), "a stale quote fills nothing");

    // The operator relists BTC with the loosened rules, with no notice and positions open.
    h.price(BTC, "100000");
    let sampled_at = h.ts;
    let relisted = loosened(&h);
    let events = h.operate(relisted);
    let relisting_applied = applied(&events);
    rejected(&events, "market 1's price rule change needs a notice of at least seven days");

    // The host cuts the venue connections for an hour; the engine has seen nothing since $100,000.
    h.advance(HOUR_MS);
    let opened =
        fills(&trade(&mut h, &own, BTC, Side::Buy, "5", "100000")).first().map(|fill| (fill.index, h.ts - sampled_at));
    assert!(
        !relisting_applied && opened.is_none(),
        "relisting applied: {relisting_applied}; the buy filled against an index of {:?} the engine last saw {:?} ms ago",
        opened.map(|(index, _)| index),
        opened.map(|(_, age)| age)
    );
}

#[test]
fn a_round_trip_on_prices_the_host_held_back_takes_nothing_from_the_house() {
    let mut h = genesis(ProtocolVersion::V76);
    let holder = client(&mut h, 1, "50000");
    only_fill(&trade(&mut h, &holder, BTC, Side::Buy, "1", "100000"));
    let own = client(&mut h, 2, "100000");
    let relisted = loosened(&h);
    let relisting_applied = applied(&h.operate(relisted));

    // The venues move to $103,000 while the host holds their streams back for an hour. The operator
    // buys at the $100,000 the engine last saw, waits out the fee curve's last hold, lets the venues
    // back in and closes.
    h.advance(HOUR_MS);
    let nav = h.engine.house_nav();
    let opened = fills(&trade(&mut h, &own, BTC, Side::Buy, "5", "100000")).len();
    h.advance(20 * MINUTE_MS);
    h.price(BTC, "103000");
    let closed = fills(&trade(&mut h, &own, BTC, Side::Sell, "5", "103000")).len();
    let taken = h.balance(own.account) - common::f("100000");
    assert!(
        !taken.is_positive(),
        "relisting applied: {relisting_applied}; opened {opened} and closed {closed} fills; the operator's account \
         gained {taken} and house NAV moved by {}",
        h.engine.house_nav() - nav
    );
}

#[test]
fn tighter_price_rules_apply_at_once_and_looser_ones_only_from_a_notice_within_the_launch_rules() {
    let mut h = genesis(ProtocolVersion::V76);
    let launch = rules(&h, BTC);
    assert_eq!(h.engine.operator_bounds().launch_bands[&BTC].price_rules, Some(launch), "the first listing's rules");

    // Tighter rules apply at once, in every field; an unheld market's loosening needs a notice too.
    let tighter = PriceRules { max_age_ms: 500, max_divergence_bps: launch.max_divergence_bps / 2, min_sources: 3 };
    assert!(applied(&h.operate(relisting(&h, BTC, tighter))));
    h.price(BTC, "100000");
    let wider = PriceRules { max_age_ms: launch.max_age_ms * 2, ..rules(&h, ETH) };
    rejected(&h.operate(relisting(&h, ETH, wider)), "market 2's price rule change needs a notice");

    // A notice may name rules back up to launch's, not past them in any rule.
    let at = h.ts + NOTICE_MS;
    for past in [
        PriceRules { max_age_ms: launch.max_age_ms + 1, ..launch },
        PriceRules { min_sources: launch.min_sources - 1, ..launch },
        PriceRules { max_divergence_bps: launch.max_divergence_bps + 1, ..launch },
    ] {
        rejected(&h.operate(rules_notice(BTC, past, at)), "are looser than its launch rules");
    }
    rejected(&h.operate(rules_notice(BTC, PriceRules { min_sources: 0, ..launch }, at)), "min_sources");
    assert!(applied(&h.operate(rules_notice(BTC, launch, at))));
    rejected(&h.operate(relisting(&h, BTC, launch)), "market 1's price rule change takes effect from");

    // From its time the notice applies once, and a mix of a tighter and a looser rule needs one too.
    h.advance_to(at);
    h.price(BTC, "100000");
    assert!(applied(&h.operate(relisting(&h, BTC, launch))));
    assert_eq!(rules(&h, BTC), launch);
    assert!(!h.engine.operator_bounds().markups.contains_key(&BTC), "the relisting spends the notice");
    let mixed = PriceRules { max_age_ms: launch.max_age_ms / 2, min_sources: launch.min_sources - 1, ..launch };
    rejected(&h.operate(relisting(&h, BTC, mixed)), "needs a notice");
}

#[test]
fn before_v76_price_rules_loosen_at_once_and_a_notice_cannot_name_them() {
    let mut h = genesis(ProtocolVersion::V74);
    let launch = rules(&h, BTC);
    assert_eq!(h.engine.operator_bounds().launch_bands[&BTC].price_rules, None);
    rejected(&h.operate(rules_notice(BTC, launch, h.ts + NOTICE_MS)), "a price rule notice needs protocol V76");
    assert!(applied(&h.operate(loosened(&h))));
}

#[test]
fn v76s_activation_takes_each_listed_markets_rules_and_the_corporate_bounds_in_force_as_launchs() {
    let mut h = genesis_at(ProtocolVersion::V74, globex(vec![]), 0, 0);
    let held = h.engine.markets().iter().map(|(id, market)| (*id, market.spec.price_rules)).collect::<Vec<_>>();
    assert!(h.engine.operator_bounds().launch_corporate_bounds.is_none());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V76 });
    let bounds = h.engine.operator_bounds();
    for (id, price_rules) in held {
        assert_eq!(bounds.launch_bands[&id].price_rules, Some(price_rules), "market {id}");
    }
    assert_eq!(bounds.launch_corporate_bounds, Some(LAUNCH_CORPORATE_BOUNDS));
    let due = h.engine.next_rollover().expect("rollovers are scheduled");
    let rollover = &h.engine.params().expect("parameters are set").rollover;
    assert_eq!(bounds.last_rollover, Some(rollover.previous_before(due)), "the rollover before the one due");
}
