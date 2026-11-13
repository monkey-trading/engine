//! `V52`'s listing bounds through `Engine::apply`: spread bands in tenths of a bp at or above their
//! class's floor, a listed market's spread and margin bands moved only as a notice of at least seven
//! days names them and within its launch bands, and activation converting what is listed.

mod common;

use common::close_spread::{NOTICE_MS, ceilings, install_expect, trade, widen_slippage};
use common::events::only_fill;
use common::spreads::{applied, btc_bands, funded, refused, spread_notice, tenths, with_spreads};
use common::{BTC, DE40, EURUSD, Harness, f};
use engine_core::auth::ClientAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::AdminCommand;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, Tier};
use engine_core::operator_bounds::{ParamsNotice, StressSettings};
use engine_core::protocol::ProtocolVersion;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::Side;

const V52: ProtocolVersion = ProtocolVersion::V52;

/// BTC's launch bands doubled: 2bp to 1m, 4bp to 5m, 10bp above.
fn doubled() -> Vec<SpreadTier> {
    tenths(&[(Some(1_000_000), 20), (Some(5_000_000), 40), (None, 100)])
}

fn notice(spread: Option<Vec<SpreadTier>>, margin: Option<Vec<Tier>>, markup: Option<i64>, at: i64) -> ListingNotice {
    ListingNotice { spread_tiers: spread, margin_tiers: margin, markup_bps: markup, effective_at: at }
}

#[test]
fn a_band_change_needs_a_notice_and_applies_once_from_its_time() {
    let (mut h, client) = funded(V52, "1000000");
    let launch = btc_bands(&h);
    let relist = |h: &mut Harness, bands| h.operate(AdminCommand::UpsertMarket(with_spreads(h, BTC, bands)));
    assert_eq!(
        refused(&relist(&mut h, doubled())),
        "market 1's spread band change needs a notice of at least seven days"
    );

    let (announce, due) = spread_notice(&h, BTC, doubled());
    h.operator_admin(announce);
    h.advance_to(due - 1);
    assert_eq!(refused(&relist(&mut h, doubled())), format!("market 1's spread band change takes effect from {due}"));
    h.advance_to(due);
    assert!(applied(&relist(&mut h, doubled())));
    assert!(h.engine.operator_bounds().markups.is_empty(), "spent");
    // The new bands price: $1m pays 2bp each way.
    h.price(BTC, "100000");
    assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000")).price, f("100020"));
    assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Sell, "10", "100000")).price, f("99980"));
    // Going back to the launch bands is a change too, and the notice is spent.
    assert_eq!(refused(&relist(&mut h, launch)), "market 1's spread band change needs a notice of at least seven days");
}

#[test]
fn a_notice_holds_each_spread_rate_from_its_launch_rate_to_twice_it() {
    let (mut h, _) = funded(V52, "1");
    let (_, due) = spread_notice(&h, BTC, doubled());
    let announce = |h: &mut Harness, bands| {
        h.operate(AdminCommand::AnnounceListing { market: BTC, notice: notice(Some(bands), None, None, due) })
    };
    // (bands, the refusal it names)
    let cases = [
        (tenths(&[(Some(1_000_000), 21), (Some(5_000_000), 40), (None, 100)]), "outside 10..=20"),
        (tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 49)]), "outside 50..=100"),
        // A bound moved up charges 1bp from 1m to 2m, below its launch 2bp.
        (tenths(&[(Some(2_000_000), 10), (Some(5_000_000), 20), (None, 50)]), "outside 20..=40"),
        (tenths(&[(Some(1_000_000), 9), (Some(5_000_000), 20), (None, 50)]), "below the 10 tenths floor"),
        (vec![SpreadTier::bps(Some(f("1000000")), 1), SpreadTier::bps(None, 5)], "are in tenths of a bp"),
    ];
    for (bands, reason) in cases {
        let events = announce(&mut h, bands.clone());
        assert!(refused(&events).contains(reason), "{bands:?}: {events:?}");
    }
    assert!(h.engine.operator_bounds().markups.is_empty());
    // A bound moved down charges the outer rate sooner, within twice the inner one.
    assert!(applied(&announce(&mut h, tenths(&[(Some(500_000), 10), (Some(5_000_000), 20), (None, 50)]))));
}

#[test]
fn a_margin_raise_waits_its_notice_and_a_cut_below_launch_is_refused() {
    let (mut h, _) = funded(V52, "1");
    let due = h.ts + NOTICE_MS;
    let margin = |bands: &[(Option<i64>, i64)]| -> Vec<Tier> {
        bands.iter().map(|&(up_to, bps)| Tier { up_to: up_to.map(engine_core::Fixed::from_int), bps }).collect()
    };
    let cut = notice(None, Some(margin(&[(None, 900)])), None, due);
    let events = h.operate(AdminCommand::AnnounceListing { market: BTC, notice: cut });
    assert!(refused(&events).contains("below its launch rate"), "{events:?}");
    let raised = margin(&[(Some(1_000_000), 1_000), (None, 1_500)]);
    let relisted = MarketSpec { margin_tiers: raised.clone(), ..h.engine.markets()[&BTC].spec.clone() };
    let events = h.operate(AdminCommand::UpsertMarket(relisted.clone()));
    assert_eq!(refused(&events), "market 1's margin band change needs a notice of at least seven days");
    h.operator_admin(AdminCommand::AnnounceListing { market: BTC, notice: notice(None, Some(raised), None, due) });
    h.advance_to(due);
    h.operator_admin(AdminCommand::UpsertMarket(relisted.clone()));
    assert_eq!(h.engine.markets()[&BTC].spec.margin_tiers, relisted.margin_tiers);
}

#[test]
fn a_relisting_spends_only_the_noticed_values_it_applies() {
    let (mut h, _) = funded(V52, "1");
    let due = h.ts + NOTICE_MS;
    let spec = h.engine.markets()[&BTC].spec.clone();
    let margin = vec![Tier { up_to: None, bps: 1_100 }];
    let markup = spec.financing.markup_bps + 25;
    let all = notice(Some(doubled()), Some(margin.clone()), Some(markup), due);
    h.operator_admin(AdminCommand::AnnounceListing { market: BTC, notice: all });
    h.advance_to(due);
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { spread_tiers: doubled(), ..spec.clone() }));
    let left = notice(None, Some(margin.clone()), Some(markup), due);
    assert_eq!(h.engine.operator_bounds().markups[&BTC], left);
    let mut rest = MarketSpec { spread_tiers: doubled(), margin_tiers: margin, ..spec };
    rest.financing.markup_bps = markup;
    h.operator_admin(AdminCommand::UpsertMarket(rest));
    assert!(h.engine.operator_bounds().markups.is_empty());
}

#[test]
fn a_markup_notice_replaces_a_pending_listing_notice() {
    let (mut h, _) = funded(V52, "1");
    let (announce, due) = spread_notice(&h, BTC, doubled());
    h.operator_admin(announce);
    let markup = h.engine.markets()[&BTC].spec.financing.markup_bps + 25;
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: markup, effective_at: due });
    assert_eq!(h.engine.operator_bounds().markups[&BTC], ListingNotice::markup(markup, due));
    h.advance_to(due);
    let events = h.operate(AdminCommand::UpsertMarket(with_spreads(&h, BTC, doubled())));
    assert_eq!(refused(&events), "market 1's spread band change needs a notice of at least seven days");
}

#[test]
fn a_listing_notice_needs_v52_a_listed_market_a_named_value_and_seven_days() {
    let (mut v51, _) = funded(ProtocolVersion::V51, "1");
    let (announce, _) = spread_notice(&v51, BTC, doubled());
    assert_eq!(refused(&v51.operate(announce)), "a listing notice needs protocol V52");

    let (mut h, _) = funded(V52, "1");
    let due = h.ts + NOTICE_MS;
    // (market, notice, the refusal)
    let cases = [
        (99, notice(Some(doubled()), None, None, due), "market 99 is not listed".to_owned()),
        (BTC, notice(None, None, None, due), "a listing notice names nothing".to_owned()),
        (BTC, notice(None, None, Some(10_001), due), "financing.markup_bps must be within ±10000".to_owned()),
        (
            BTC,
            notice(Some(doubled()), None, None, due - 1),
            format!("a listing notice takes effect no sooner than {due}"),
        ),
    ];
    for (market, notice, reason) in cases {
        assert_eq!(refused(&h.operate(AdminCommand::AnnounceListing { market, notice })), reason);
    }
}

/// `V8`'s genesis with the FX markets listed, a ceiling for every market, a level-1 size multiplier
/// of zero, and `V51`.
fn before_v52() -> Harness {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    h.list_fx();
    widen_slippage(&mut h);
    let mut entries = ceilings();
    entries.extend([4, 5, 6].map(|market| CloseSpreadCeiling { market, max_close_spread_bps: 15 }));
    install_expect(&mut h, entries);
    let mut params = h.engine.params().unwrap().clone();
    params.stress_levels[1].size_spread_multiplier_bps = 0;
    h.operator_admin(AdminCommand::SetParams(params));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V51 });
    h
}

#[test]
fn activation_writes_the_bands_in_tenths_at_their_floor_and_takes_them_as_launch() {
    let mut h = before_v52();
    let whole = h.engine.markets()[&BTC].spec.spread_tiers.clone();
    assert!(whole.iter().all(SpreadTier::is_whole_bps));
    h.admin(AdminCommand::ActivateProtocol { version: V52 });
    // (market, its bands after activation): 0bp rises to 1bp on crypto and a tenth on FX and indices.
    for (market, first) in [(BTC, 10), (EURUSD, 1), (DE40, 1)] {
        let spec = &h.engine.markets()[&market].spec;
        assert_eq!(spec.spread_tiers, tenths(&[(Some(1_000_000), first), (Some(5_000_000), 20), (None, 50)]));
        let launch = &h.engine.operator_bounds().launch_bands[&market];
        assert_eq!((&launch.spread_tiers, &launch.margin_tiers), (&spec.spread_tiers, &spec.margin_tiers));
    }
    let levels = &h.engine.params().unwrap().stress_levels;
    assert_eq!(levels.iter().map(|l| l.size_spread_multiplier_bps).collect::<Vec<_>>(), [10_000, 10_000]);
    // A relisting of the bands in force changes nothing and needs no notice.
    h.operator_admin(AdminCommand::UpsertMarket(h.engine.markets()[&BTC].spec.clone()));
}

#[test]
fn from_v52_a_listing_is_in_tenths_and_at_least_its_class_floor() {
    let mut h = before_v52();
    // Before `V52` either unit lists and no floor binds; activation raises a tenth of a bp on BTC to
    // the crypto floor, which BTC charges from then.
    h.operator_admin(AdminCommand::UpsertMarket(with_spreads(&h, BTC, tenths(&[(None, 1)]))));
    h.admin(AdminCommand::ActivateProtocol { version: V52 });
    assert_eq!(btc_bands(&h), tenths(&[(None, 10)]));
    let whole = with_spreads(&h, BTC, vec![SpreadTier::bps(None, 5)]);
    let reason = refused(&h.operate(AdminCommand::UpsertMarket(whole))).to_owned();
    assert_eq!(reason, "from protocol V52 spread bands are in tenths of a bp (tenth_bps)");
    // (market, a first band below its floor)
    for (market, rate) in [(BTC, 9), (EURUSD, 0), (DE40, 0)] {
        let mut bands = h.engine.markets()[&market].spec.spread_tiers.clone();
        bands[0] = SpreadTier::tenths(bands[0].up_to, rate);
        let events = h.operate(AdminCommand::UpsertMarket(with_spreads(&h, market, bands)));
        assert!(refused(&events).contains("floor"), "{market}: {events:?}");
    }
}

#[test]
fn from_v52_no_stress_level_waives_the_size_spread() {
    let (mut h, _) = funded(V52, "1");
    let mut params = h.engine.params().unwrap().clone();
    params.stress_levels[1].size_spread_multiplier_bps = 9_999;
    let reason = "from protocol V52 every stress level's size multiplier is at least 10000 bps";
    assert_eq!(refused(&h.operate(AdminCommand::SetParams(params.clone()))), reason);
    let stress = Some(StressSettings::of(&params));
    let at = h.ts + NOTICE_MS;
    let notice = ParamsNotice { profit_fee_bps: None, closeout_level_bps: None, stress, effective_at: at };
    assert_eq!(refused(&h.operate(AdminCommand::AnnounceParams(notice))), reason);
}
