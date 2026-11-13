//! The seven-day notice on the stress parameters and on a listed market's financing markup from
//! `V49`, through `Engine::apply`: a change with no notice, or before its time, is refused, the named
//! value applies once from its time on, a new listing needs none, and earlier versions take either at
//! once.

mod common;

use common::close_spread::{ceilings, install_expect, rejected};
use common::{BTC, ETH, Harness};
use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::listing_bounds::ListingNotice;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice, StressSettings};
use engine_core::params::{ProtocolParams, StressTrigger};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};

/// The fixture at `version`.
fn at(version: ProtocolVersion) -> Harness {
    let mut h = Harness::v21();
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version });
    h
}

fn v49() -> Harness {
    at(ProtocolVersion::V49)
}

/// The parameters in force with the top level's spread a basis point wider.
fn wider(h: &Harness) -> ProtocolParams {
    let mut params = h.engine.params().expect("the fixture sets parameters").clone();
    let top = params.stress_levels.len() - 1;
    params.stress_levels[top].min_spread_bps += 1;
    params
}

fn stress_notice(params: &ProtocolParams, effective_at: Millis) -> AdminCommand {
    let stress = Some(StressSettings::of(params));
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress,
        effective_at,
        score_tiers: None,
    })
}

fn markup_of(h: &Harness, market: MarketId) -> i64 {
    h.engine.markets()[&market].spec.financing.markup_bps
}

/// `market` listed again with `markup_bps`.
fn relisted(h: &Harness, market: MarketId, markup_bps: i64) -> AdminCommand {
    let mut spec = h.engine.markets()[&market].spec.clone();
    spec.financing.markup_bps = markup_bps;
    AdminCommand::UpsertMarket(spec)
}

fn applied(events: &[Event]) -> bool {
    events.last() == Some(&Event::AdminApplied)
}

#[test]
fn a_stress_change_needs_a_notice_and_applies_once_from_its_time() {
    let mut h = v49();
    let before = h.engine.params().unwrap().clone();
    let params = wider(&h);
    let events = h.operate(AdminCommand::SetParams(params.clone()));
    assert_eq!(rejected(&events), "a stress parameter change needs a notice of at least seven days");

    let due = h.ts + NOTICE_MS;
    h.operator_admin(stress_notice(&params, due));
    h.advance_to(due - 1);
    let events = h.operate(AdminCommand::SetParams(params.clone()));
    assert_eq!(rejected(&events), format!("the stress parameter change takes effect from {due}"));

    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::SetParams(params.clone()))));
    assert_eq!(StressSettings::of(h.engine.params().unwrap()), StressSettings::of(&params));
    assert_eq!(h.engine.operator_bounds().notice, None, "spent");
    // Going back is a change too, and the notice is spent.
    let events = h.operate(AdminCommand::SetParams(before));
    assert_eq!(rejected(&events), "a stress parameter change needs a notice of at least seven days");
}

#[test]
fn every_stress_parameter_takes_the_notice() {
    let mut h = v49();
    let current = h.engine.params().unwrap().clone();
    let top = current.stress_levels.len() - 1;
    // A trigger every reading meets holds every priced market at the top level.
    let mut always = current.clone();
    always.stress_levels[top].trigger = Some(StressTrigger { move_bps: Some(0), ..StressTrigger::default() });
    let mut copied = current.clone();
    copied.stress_levels.push(current.stress_levels[top].clone());
    let mut cooldown = current.clone();
    cooldown.stress_cooldown_ms *= 1_000;
    let mut window = current.clone();
    window.stress_move_window_ms *= 2;
    let mut size = current.clone();
    size.stress_levels[top].size_spread_multiplier_bps += 1;
    for (name, params) in
        [("always", always), ("copied", copied), ("cooldown", cooldown), ("window", window), ("size", size)]
    {
        let events = h.operate(AdminCommand::SetParams(params));
        assert_eq!(rejected(&events), "a stress parameter change needs a notice of at least seven days", "{name}");
    }
    assert_eq!(h.engine.params(), Some(&current));
}

#[test]
fn only_the_noticed_stress_parameters_apply() {
    let mut h = v49();
    let params = wider(&h);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(stress_notice(&params, due));
    h.advance_to(due);
    let mut other = params.clone();
    other.stress_cooldown_ms += 1;
    let events = h.operate(AdminCommand::SetParams(other));
    assert_eq!(rejected(&events), "a stress parameter change needs a notice of at least seven days");
    assert!(h.engine.operator_bounds().notice.is_some(), "a refused change spends nothing");
}

#[test]
fn parameters_that_leave_the_stress_alone_need_no_stress_notice_and_keep_a_pending_one() {
    let mut h = v49();
    let noticed = wider(&h);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(stress_notice(&noticed, due));
    let mut capped = h.engine.params().unwrap().clone();
    capped.house_total_cap_bps += 1;
    assert!(applied(&h.operate(AdminCommand::SetParams(capped.clone()))));
    assert_eq!(
        h.engine.operator_bounds().notice.as_ref().and_then(|n| n.stress.clone()),
        Some(StressSettings::of(&noticed))
    );
    // The noticed stress then applies over the parameters in force.
    h.advance_to(due);
    let params = ProtocolParams { house_total_cap_bps: capped.house_total_cap_bps, ..noticed };
    assert!(applied(&h.operate(AdminCommand::SetParams(params))));
}

#[test]
fn a_rate_and_stress_notice_is_spent_part_by_part_and_a_new_notice_replaces_it() {
    let mut h = v49();
    let params = wider(&h);
    let fee = h.engine.params().unwrap().profit_fee_bps + 1;
    let due = h.ts + NOTICE_MS;
    let stress = Some(StressSettings::of(&params));
    let notice = ParamsNotice {
        profit_fee_bps: Some(fee),
        closeout_level_bps: None,
        stress,
        effective_at: due,
        score_tiers: None,
    };
    h.operator_admin(AdminCommand::AnnounceParams(notice));
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::SetParams(params.clone()))));
    let left = h.engine.operator_bounds().notice.clone().unwrap();
    assert_eq!((left.profit_fee_bps, left.stress), (Some(fee), None));

    // A notice naming only a rate replaces one naming the stress parameters.
    h.operator_admin(stress_notice(&wider(&h), h.ts + NOTICE_MS));
    let effective_at = h.ts + NOTICE_MS;
    let rate_only = ParamsNotice {
        profit_fee_bps: Some(fee),
        closeout_level_bps: None,
        stress: None,
        effective_at,
        score_tiers: None,
    };
    h.operator_admin(AdminCommand::AnnounceParams(rate_only));
    assert_eq!(h.engine.operator_bounds().notice.as_ref().unwrap().stress, None);
}

#[test]
fn a_stress_notice_is_refused_before_v49_invalid_or_too_soon() {
    let mut h = at(ProtocolVersion::V47);
    let params = wider(&h);
    let events = h.operate(stress_notice(&params, h.ts + NOTICE_MS));
    assert_eq!(rejected(&events), "a stress notice needs protocol V49");

    let mut h = v49();
    let mut empty = wider(&h);
    empty.stress_levels.clear();
    let events = h.operate(stress_notice(&empty, h.ts + NOTICE_MS));
    assert_eq!(rejected(&events), "stress_levels needs at least the normal level");
    // A top level whose spread, with the widest toxicity add-on, reaches the price is refused too.
    let mut reaching = wider(&h);
    let top = reaching.stress_levels.len() - 1;
    reaching.stress_levels[top].min_spread_bps = 10_000;
    let events = h.operate(stress_notice(&reaching, h.ts + NOTICE_MS));
    assert!(rejected(&events).ends_with("reach the price, so no closing trade could fill"), "{events:?}");
    assert_eq!(h.engine.operator_bounds().notice, None);
    let soon = h.ts + NOTICE_MS - 1;
    let events = h.operate(stress_notice(&wider(&h), soon));
    assert_eq!(rejected(&events), format!("a parameter notice takes effect no sooner than {}", h.ts + NOTICE_MS));
    assert_eq!(h.engine.operator_bounds().notice, None);
}

#[test]
fn before_v49_the_stress_parameters_and_the_markup_move_at_once() {
    let mut h = at(ProtocolVersion::V47);
    let params = wider(&h);
    assert!(applied(&h.operate(AdminCommand::SetParams(params.clone()))));
    assert_eq!(h.engine.params(), Some(&params));
    let markup = markup_of(&h, BTC) + 100;
    let command = relisted(&h, BTC, markup);
    assert!(applied(&h.operate(command)));
    assert_eq!(markup_of(&h, BTC), markup);
    let events =
        h.operate(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: markup, effective_at: h.ts + NOTICE_MS });
    assert_eq!(rejected(&events), "a markup notice needs protocol V49");
}

#[test]
fn a_markup_change_needs_a_notice_and_applies_once_from_its_time() {
    let mut h = v49();
    let before = markup_of(&h, BTC);
    // The top of the allowed range.
    let markup = 10_000;
    let command = relisted(&h, BTC, markup);
    let events = h.operate(command);
    assert_eq!(
        rejected(&events),
        format!("market {BTC}'s markup change to {markup} needs a notice of at least seven days")
    );

    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: markup, effective_at: due });
    assert_eq!(h.engine.operator_bounds().markups[&BTC], ListingNotice::markup(markup, due));
    h.advance_to(due - 1);
    let command = relisted(&h, BTC, markup);
    assert_eq!(
        rejected(&h.operate(command)),
        format!("market {BTC}'s markup change to {markup} takes effect from {due}")
    );
    let command = relisted(&h, BTC, markup - 1);
    assert_eq!(
        rejected(&h.operate(command)),
        format!("market {BTC}'s markup change to {} needs a notice of at least seven days", markup - 1)
    );

    h.advance_to(due);
    let command = relisted(&h, BTC, markup);
    assert!(applied(&h.operate(command)));
    assert_eq!(markup_of(&h, BTC), markup);
    assert!(h.engine.operator_bounds().markups.is_empty(), "spent");
    let command = relisted(&h, BTC, before);
    assert!(rejected(&h.operate(command)).ends_with("needs a notice of at least seven days"));
}

#[test]
fn each_market_keeps_its_own_markup_notice() {
    let mut h = v49();
    let (btc, eth) = (markup_of(&h, BTC) + 1, markup_of(&h, ETH) + 2);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: btc, effective_at: due });
    h.operator_admin(AdminCommand::AnnounceMarkup { market: ETH, markup_bps: eth, effective_at: due + 1_000 });
    // A second notice for BTC replaces BTC's alone.
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: btc + 1, effective_at: due + 2_000 });
    let markups = &h.engine.operator_bounds().markups;
    assert_eq!(markups[&BTC], ListingNotice::markup(btc + 1, due + 2_000));
    assert_eq!(markups[&ETH], ListingNotice::markup(eth, due + 1_000));
    // A parameter notice leaves them alone.
    h.operator_admin(stress_notice(&wider(&h), due));
    assert_eq!(h.engine.operator_bounds().markups.len(), 2);

    h.advance_to(due + 1_000);
    let command = relisted(&h, ETH, eth);
    assert!(applied(&h.operate(command)));
    assert_eq!(h.engine.operator_bounds().markups.keys().copied().collect::<Vec<_>>(), [BTC]);
}

#[test]
fn a_relisting_that_keeps_the_markup_and_a_new_listing_need_no_notice() {
    let mut h = v49();
    let pending = markup_of(&h, BTC) + 1;
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: pending, effective_at: due });
    let mut spec = h.engine.markets()[&BTC].spec.clone();
    spec.max_slippage_bps += 1;
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(spec))));
    assert!(h.engine.operator_bounds().markups.contains_key(&BTC), "the notice waits for its change");

    // A market not yet listed takes any markup: nobody holds it.
    const NEW: MarketId = 7;
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling {
        market: NEW,
        max_close_spread_bps: 25,
        effective_at: due,
        listing: None,
    });
    h.advance_to(due);
    h.cmd(Command::Tick);
    let mut listing = h.engine.markets()[&BTC].spec.clone();
    (listing.id, listing.symbol) = (NEW, "NEW-USD".into());
    listing.financing.markup_bps = 9_000;
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(listing))));
    assert_eq!(markup_of(&h, NEW), 9_000);
}

#[test]
fn a_markup_notice_is_refused_for_an_unlisted_market_out_of_range_too_soon_or_unsigned() {
    let mut h = v49();
    let due = h.ts + NOTICE_MS;
    let announce = |market, markup_bps, effective_at| AdminCommand::AnnounceMarkup { market, markup_bps, effective_at };
    assert_eq!(rejected(&h.operate(announce(9, 100, due))), "market 9 is not listed");
    for markup in [10_001, -10_001] {
        assert_eq!(rejected(&h.operate(announce(BTC, markup, due))), "financing.markup_bps must be within ±10000");
    }
    assert_eq!(
        rejected(&h.operate(announce(BTC, 100, due - 1))),
        format!("a markup notice takes effect no sooner than {due}")
    );
    let events = h.cmd(Command::Admin(announce(BTC, 100, due)));
    assert_eq!(rejected(&events), "from protocol V8 the operator signs this command once funds are credited");
    assert!(h.engine.operator_bounds().markups.is_empty());
}

#[test]
fn a_replay_reaches_the_same_root_over_notices() {
    let mut h = v49();
    let params = wider(&h);
    let markup = markup_of(&h, BTC) + 1;
    let due = h.ts + NOTICE_MS;
    h.operator_admin(stress_notice(&params, due));
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: markup, effective_at: due });
    let pending = h.engine.state_root();
    assert_eq!(common::batch::replayed(&h).state_root(), pending);
    h.advance_to(due);
    h.operator_admin(AdminCommand::SetParams(params));
    let command = relisted(&h, BTC, markup);
    h.operator_admin(command);
    let spent = h.engine.state_root();
    assert_ne!(spent, pending);
    assert_eq!(common::batch::replayed(&h).state_root(), spent);
}

#[test]
fn a_refused_relisting_keeps_its_markup_notice() {
    let mut h = v49();
    let markup = markup_of(&h, BTC) + 1;
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: markup, effective_at: due });
    h.advance_to(due);
    // The noticed markup passes, then the listing fails a later check.
    let mut spec = h.engine.markets()[&BTC].spec.clone();
    spec.financing.markup_bps = markup;
    spec.price_rules.max_divergence_bps = 0;
    let events = h.operate(AdminCommand::UpsertMarket(spec));
    assert!(rejected(&events).starts_with("a divergence share above zero can never fire in"), "{events:?}");
    assert_eq!(h.engine.operator_bounds().markups[&BTC], ListingNotice::markup(markup, due));
    let command = relisted(&h, BTC, markup);
    assert!(applied(&h.operate(command)));
}

#[test]
fn pending_notices_survive_a_snapshot() {
    let mut h = v49();
    let params = wider(&h);
    let markup = markup_of(&h, BTC) + 1;
    let due = h.ts + NOTICE_MS;
    h.operator_admin(stress_notice(&params, due));
    h.operator_admin(AdminCommand::AnnounceMarkup { market: BTC, markup_bps: markup, effective_at: due });
    let mut restored: Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    restored.restore_indexes();
    assert_eq!(restored.operator_bounds(), h.engine.operator_bounds());
    assert_eq!(restored.state_root(), h.engine.state_root());
}
