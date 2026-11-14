//! `V52`'s floor under the close-spread ceilings, through `Engine::apply`: a ceiling covers its
//! market's launch close charge, which counts the first spread band at the widest size multiplier,
//! and an adoption for a market not yet listed binds the listing that market's first listing must be;
//! no adoption goes below the charge a market launched with, and `V52` does not activate over a
//! lower ceiling. The genesis harness's launch close charge is 10bps: a 5bp stress minimum and a 3bp
//! add-on, plus the first band's 1bp at level 1's 2x.

mod common;

use common::close_spread::{NOTICE_MS, ceilings, install, install_expect, into_v40, share_params, with};
use common::spreads::{applied, genesis_v52, refused, tenths};
use common::toxicity::floored;
use common::{AAPL, BTC, ETH, Harness, f, market};
use engine_core::Engine;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::market::MarketSpec;
use engine_core::params::ToxicityTier;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AssetClass, MarketId};

/// A market not yet listed.
const SOL: MarketId = 7;

/// SOL-USD at 3bp a band: a launch close charge of 8bp plus 3bp at 2x, 14bps.
fn proposal() -> MarketSpec {
    MarketSpec {
        spread_tiers: tenths(&[(None, 30)]),
        ..market(SOL, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
    }
}

fn adopt(market: MarketId, bps: i64, effective_at: i64, listing: Option<MarketSpec>) -> AdminCommand {
    AdminCommand::AdoptCloseSpreadCeiling { market, max_close_spread_bps: bps, effective_at, listing }
}

/// The harness parameters with toxicity tier adding `bps`.
fn with_add_on(bps: i64) -> AdminCommand {
    let mut params = share_params();
    params.toxicity.tiers = vec![ToxicityTier { min_score_bps: 20, extra_spread_bps: bps }];
    AdminCommand::SetParams(params)
}

#[test]
fn an_install_refuses_a_ceiling_below_its_listed_markets_launch_close_charge() {
    // (BTC's ceiling, the refusal): zero, which `V40` reads as free closes, is below it too.
    for (bps, charge) in [(9, "9bps"), (0, "0bps")] {
        let mut h = genesis_v52();
        let events = install(&mut h, with(&ceilings(), BTC, bps));
        let reason = format!("market 1's close-spread ceiling of {charge} is below its launch close charge of 10.0bps");
        assert_eq!(refused(&events), reason);
    }
    let mut h = genesis_v52();
    // An entry for a market not yet listed waits for its first listing's check.
    let mut entries = with(&ceilings(), BTC, 10);
    entries.push(CloseSpreadCeiling { market: 8, max_close_spread_bps: 9 });
    install_expect(&mut h, entries);
    // At 9bps a crypto listing's 10bps charge is over the entry; an FX listing's 8.2bps is not.
    let crypto = MarketSpec {
        spread_tiers: tenths(&[(None, 10)]),
        ..market(8, "LINK-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
    };
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(crypto)));
    assert_eq!(refused(&events), "market 8's close-spread ceiling of 9bps is below its launch close charge of 10.0bps");
    let fx = MarketSpec {
        spread_tiers: tenths(&[(None, 1)]),
        ..market(8, "AUD-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200)
    };
    h.admin(AdminCommand::UpsertMarket(fx));
}

#[test]
fn an_adoption_for_a_listed_market_covers_its_launch_close_charge_and_names_no_listing() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    let due = h.ts + NOTICE_MS;
    let events = h.cmd(Command::Admin(adopt(BTC, 9, due, None)));
    assert_eq!(refused(&events), "market 1's close-spread ceiling of 9bps is below its launch close charge of 10.0bps");
    let named = MarketSpec { id: BTC, ..proposal() };
    assert_eq!(
        refused(&h.cmd(Command::Admin(adopt(BTC, 10, due, Some(named))))),
        "market 1 is listed, so its adoption names no listing"
    );
    h.admin(adopt(BTC, 10, due, None));
}

#[test]
fn an_adoption_for_a_market_not_yet_listed_binds_its_first_listing() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    let due = h.ts + NOTICE_MS;
    let other = MarketSpec { id: 8, ..proposal() };
    // (adoption, the refusal)
    let cases = [
        (adopt(SOL, 15, due, None), "market 7 is not listed, so its adoption names the listing it binds".to_owned()),
        (adopt(SOL, 15, due, Some(other)), "an adoption for market 7 names a listing of market 8".to_owned()),
        (
            adopt(SOL, 13, due, Some(proposal())),
            "market 7's close-spread ceiling of 13bps is below its launch close charge of 14.0bps".to_owned(),
        ),
    ];
    for (command, reason) in cases {
        assert_eq!(refused(&h.cmd(Command::Admin(command))), reason);
    }
    h.admin(adopt(SOL, 15, due, Some(proposal())));
    assert_eq!(snapshot(&h).operator_bounds().proposed_listings.len(), 0, "binds once applied");
    h.advance_to(due);

    // A listing other than the proposal is refused, as is the proposal while a wider add-on lifts its
    // charge to 16bps, over the 15bps ceiling.
    let renamed = MarketSpec { symbol: "SOL-PERP".into(), ..proposal() };
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(renamed)));
    assert_eq!(refused(&events), "market 7's first listing must be the listing its ceiling adoption bound");
    assert_eq!(h.engine.operator_bounds().proposed_listings[&SOL], proposal());
    h.admin(with_add_on(5));
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(proposal())));
    assert_eq!(
        refused(&events),
        "market 7's close-spread ceiling of 15bps is below its launch close charge of 16.0bps"
    );
    h.admin(with_add_on(3));
    assert!(applied(&h.cmd(Command::Admin(AdminCommand::UpsertMarket(proposal())))));
    let bounds = h.engine.operator_bounds();
    assert!(bounds.proposed_listings.is_empty(), "spent by the first listing");
    assert_eq!(bounds.launch_bands[&SOL].spread_tiers, proposal().spread_tiers);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&SOL], 15);
}

#[test]
fn an_adoption_naming_a_listing_the_listing_checks_refuse_is_refused() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    let due = h.ts + NOTICE_MS;
    // (listing, the refusal): one `MarketSpec::validate` refuses, and two `check_spread_tiers` does.
    let cases = [
        (MarketSpec { symbol: String::new(), ..proposal() }, "symbol is empty"),
        (
            MarketSpec { spread_tiers: tenths(&[(None, 9)]), ..proposal() },
            "a spread band of 9 tenths of a bp is below the 10 tenths floor of a Crypto market",
        ),
        (
            MarketSpec { spread_tiers: vec![SpreadTier::bps(None, 3)], ..proposal() },
            "from protocol V52 spread bands are in tenths of a bp (tenth_bps)",
        ),
    ];
    for (listing, reason) in cases {
        assert_eq!(refused(&h.cmd(Command::Admin(adopt(SOL, 15, due, Some(listing))))), reason);
    }
    assert_eq!(h.engine.pending_ceiling(), None);
}

#[test]
fn from_v58_an_adoption_binds_only_a_listing_that_names_a_markout_floor() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V58 });
    // `V54`'s launch score tiers add 5bps at the top, so the proposal's charge is 16bps.
    let due = h.ts + NOTICE_MS;
    assert_eq!(
        refused(&h.cmd(Command::Admin(adopt(SOL, 16, due, Some(proposal()))))),
        "market 7 names no markout floor"
    );
    h.admin(adopt(SOL, 16, due, Some(floored(proposal(), "1"))));
}

#[test]
fn v58_floors_each_listing_an_adoption_bound_so_its_first_listing_can_list() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    // Under `V54` an adoption binds a listing that names no floor, which a first listing must name.
    let due = h.ts + NOTICE_MS;
    h.admin(adopt(SOL, 16, due, Some(proposal())));
    h.advance_to(due);
    h.cmd(Command::Tick);
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(proposal())));
    assert_eq!(refused(&events), "market 7 names no markout floor");
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(floored(proposal(), "1"))));
    assert_eq!(refused(&events), "market 7's first listing must be the listing its ceiling adoption bound");
    // Another adoption, for market 8, waits out its notice.
    let link = MarketSpec { id: 8, symbol: "LINK-USD".into(), ..proposal() };
    h.admin(adopt(8, 16, h.ts + NOTICE_MS, Some(link)));

    // `V58` gives both a crypto listing's 1bp floor, and the first listing takes it.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V58 });
    assert_eq!(h.engine.operator_bounds().proposed_listings[&SOL], floored(proposal(), "1"));
    let pending = h.engine.pending_ceiling().and_then(|pending| pending.listing.as_ref());
    assert_eq!(pending.and_then(|listing| listing.markout_floor_bps), Some(f("1")));
    h.admin(AdminCommand::UpsertMarket(floored(proposal(), "1")));
    assert_eq!(h.engine.markets()[&SOL].spec.markout_floor_bps, Some(f("1")));
}

#[test]
fn v58_does_not_activate_across_v54_over_a_bound_listing_below_its_converted_close_charge() {
    // Under `V52` the proposal's charge is 14bps, with the 3bps add-on; `V54`'s launch score tiers
    // lift it to 16bps. A pending adoption binds the listing, then an applied one.
    for applied_first in [false, true] {
        let mut h = genesis_v52();
        install_expect(&mut h, ceilings());
        let due = h.ts + NOTICE_MS;
        h.admin(adopt(SOL, 15, due, Some(proposal())));
        if applied_first {
            h.advance_to(due);
            h.cmd(Command::Tick);
            assert!(h.engine.operator_bounds().proposed_listings.contains_key(&SOL));
        }
        let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V58 }));
        assert_eq!(
            refused(&events),
            "protocol V54 cannot activate: market 7's close-spread ceiling of 15bps is below the launch close charge \
             of 16.0bps of the listing its adoption bound, and a ceiling cannot be raised, so the log needs a fresh \
             start"
        );
        assert_eq!(h.engine.protocol_version(), ProtocolVersion::V52);
        // `V54` itself takes no check of a listing not yet listed.
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    }
    // A ceiling that covers the converted charge lets `V58` activate.
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    h.admin(adopt(SOL, 16, h.ts + NOTICE_MS, Some(proposal())));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V58 });
}

#[test]
fn v58_measures_a_bound_listing_a_pending_adoption_replaces_by_the_pending_one() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    // An applied adoption binds a 7bp listing at 30bps, a charge of 22bps under `V52` and 24bps under
    // `V54`'s tiers. A pending one at 16bps binds the 3bp proposal, 16bps under `V54`'s, and replaces
    // it: only the proposal can list.
    let wide = MarketSpec { spread_tiers: tenths(&[(None, 70)]), ..proposal() };
    let due = h.ts + NOTICE_MS;
    h.admin(adopt(SOL, 30, due, Some(wide)));
    h.advance_to(due);
    h.cmd(Command::Tick);
    h.admin(adopt(SOL, 16, h.ts + NOTICE_MS, Some(proposal())));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V58 });
    let pending = h.engine.pending_ceiling().and_then(|pending| pending.listing.clone());
    assert_eq!(pending, Some(floored(proposal(), "1")));
}

#[test]
fn a_first_listing_while_an_adoption_waits_is_held_to_its_listing_and_value() {
    let mut h = genesis_v52();
    // An entry for SOL from genesis, at 50bps, before any proposal.
    let mut entries = ceilings();
    entries.push(CloseSpreadCeiling { market: SOL, max_close_spread_bps: 50 });
    install_expect(&mut h, entries);
    let due = h.ts + NOTICE_MS;
    h.admin(adopt(SOL, 15, due, Some(proposal())));
    // While the adoption waits, another listing is refused, and so is the proposal once a wider
    // add-on lifts its charge over the pending 15bps, though under the entry's 50.
    let wider = MarketSpec { spread_tiers: tenths(&[(None, 100)]), ..proposal() };
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(wider)));
    assert_eq!(refused(&events), "market 7's first listing must be the listing its ceiling adoption bound");
    h.admin(with_add_on(5));
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(proposal())));
    assert_eq!(
        refused(&events),
        "market 7's close-spread ceiling of 15bps is below its launch close charge of 16.0bps"
    );
    h.admin(with_add_on(3));
    h.admin(AdminCommand::UpsertMarket(proposal()));
    // The adoption applies to the listed market, and binds nothing further.
    h.advance_to(due);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&SOL], 15);
    assert!(h.engine.operator_bounds().proposed_listings.is_empty());
}

#[test]
fn a_lower_add_on_admits_no_ceiling_below_the_charge_its_market_launched_with() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    // Without the add-on BTC's charge under the parameters in force is 7bps, but it launched at 10.
    h.admin(with_add_on(0));
    let events = h.cmd(Command::Admin(adopt(BTC, 9, h.ts + NOTICE_MS, None)));
    assert_eq!(refused(&events), "market 1's close-spread ceiling of 9bps is below its launch close charge of 10.0bps");
    h.admin(adopt(BTC, 10, h.ts + NOTICE_MS, None));
    // A wider add-on binds an adoption under the parameters in force.
    h.admin(with_add_on(5));
    let events = h.cmd(Command::Admin(adopt(BTC, 11, h.ts + NOTICE_MS, None)));
    assert_eq!(
        refused(&events),
        "market 1's close-spread ceiling of 11bps is below its launch close charge of 12.0bps"
    );
}

#[test]
fn v52_does_not_activate_over_a_ceiling_or_pending_adoption_below_the_launch_close_charge() {
    // Before `V52` an adoption may lower an entry to zero, which waives a close's whole spread, and a
    // ceiling only falls, so the activation is refused and the log keeps its version.
    let refuses = |events: &[engine_core::event::Event]| {
        let reason = refused(events);
        let below = "protocol V52 cannot activate: market 2's close-spread ceiling of 0bps is below its launch";
        assert!(reason.starts_with(below) && reason.ends_with("so the log needs a fresh start"), "{reason}");
    };
    let mut h = common::close_spread::harness();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V51 });
    assert!(applied(&h.operate(adopt(ETH, 0, h.ts + NOTICE_MS, None))));
    let pending = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V52 }));
    refuses(&pending);
    h.advance_to(h.ts + NOTICE_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&ETH], 0);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V52 }));
    refuses(&events);
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V51);
    // Every ceiling covering its charge, as the harness's genesis entries do, lets it activate.
    let mut h = common::close_spread::harness();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V51 });
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V52 });
    let launched = |market| h.engine.operator_bounds().launch_bands[&market].close_charge_tenth_bps;
    assert!([BTC, ETH, AAPL].into_iter().all(|market| launched(market) > 0));
}

#[test]
fn a_pending_adoption_keeps_its_listing_through_serialization() {
    let mut h = genesis_v52();
    install_expect(&mut h, ceilings());
    let due = h.ts + NOTICE_MS;
    h.admin(adopt(SOL, 15, due, Some(proposal())));
    let mut decoded = snapshot(&h);
    let tick = Sequenced { seq: h.log.len() as u64, ts: due, cmd: Command::Tick };
    decoded.apply(&tick).expect("in sequence");
    assert_eq!(decoded.operator_bounds().proposed_listings[&SOL], proposal());
}

#[test]
fn before_v52_an_adoption_names_no_listing() {
    let mut h = common::close_spread::harness();
    into_v40(&mut h);
    let events = h.operate(adopt(SOL, 15, h.ts + NOTICE_MS, Some(proposal())));
    assert_eq!(refused(&events), "an adoption's listing needs protocol V52");
}

/// The engine as a snapshot holds it.
fn snapshot(h: &Harness) -> Engine {
    serde_json::from_slice(&serde_json::to_vec(&h.engine).expect("engine serialises")).expect("engine deserialises")
}
