//! A futures market's contracts and roll from `V87`, through `Engine::apply` with rotation off: a relisting
//! changes the contracts from the one the market prices from on only as a seven-day listing notice or a vote
//! names them, though contracts may be added after the last at once; and a market whose front contract stopped
//! quoting before a gap to its next contract was recorded rolls onto that contract once the front has had no price
//! for seven days, on the gap from the market's mark. So no relisting leaves a held market at once without a next
//! contract its venues quote, a market left so by a noticed listing or a vote has a price again once a next
//! contract quotes, and no front outage short of seven days, closed hours included, takes a move off the books.

mod common;

use chrono::{DateTime, Utc};
use common::close_spread::{ceilings, source_key, trade};
use common::events::{fills, only_fill};
use common::spreads::tenths;
use common::toxicity::v54_params;
use common::weekend::client;
use common::{BTC, CHAIN_ID, Harness, VAULT, f, market, operator};
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::futures::{FuturesContract, MARK_ROLL_WAIT_MS};
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::params::ProtocolParams;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Millis, Side};
use engine_core::volume_tier::launch_volume_tiers;

const DAY_MS: Millis = 86_400_000;
const V86: ProtocolVersion = ProtocolVersion::V86;
const V87: ProtocolVersion = ProtocolVersion::V87;
/// US 500, priced from E-mini futures.
const US500: MarketId = 20;
/// When the index rolls from December onto March: an hour after the harness start.
const ROLL_AFTER_MS: Millis = 3_600_000;

#[test]
fn a_held_futures_markets_next_contracts_stay_listed_without_a_notice_so_it_rolls_and_closes_out() {
    let (mut h, long) = held_us500(V87);
    let reason = "market 20's futures contract change needs a notice of at least seven days";
    // Dropping March and June, renaming March to a contract no venue quotes, and moving March's roll a month on
    // are each refused, while the market holds the long.
    let mut renamed = all_contracts();
    renamed[1].contract = "ESH7X".into();
    let mut later = all_contracts();
    later[1].from += chrono::Duration::days(30);
    for contracts in [vec![all_contracts()[0].clone()], renamed, later] {
        rejected(&h.operate(AdminCommand::UpsertMarket(us500(contracts))), reason);
    }
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.gap), Some(f("-40")));

    // December stops quoting after March's roll time; the market rolls on the gap and March's fall closes the
    // long out.
    let events = expiry_then_fall(&mut h);
    assert!(events.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "it rolls: {events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "the fall closes the long out: {events:?}");
    assert!(h.engine.accounts()[&long.account].positions.get(&US500).is_none_or(|p| p.qty.is_zero()));
}

#[test]
fn before_v87_a_relisting_drops_a_futures_markets_next_contracts_at_once() {
    let (mut h, _) = held_us500(V86);
    let front_only = AdminCommand::UpsertMarket(us500(vec![all_contracts()[0].clone()]));
    assert_eq!(h.operate(front_only).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.gap), None);
}

#[test]
fn contracts_added_after_the_last_or_dropped_before_the_front_change_at_once() {
    let (mut h, _) = held_us500(V87);
    let mut more = all_contracts();
    more.push(contract("ESU7", common::start() + 200 * DAY_MS));
    assert_eq!(h.operate(AdminCommand::UpsertMarket(us500(more.clone()))).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.gap), Some(f("-40")));

    // Once the market rolls onto March, December may go.
    expiry_then_fall(&mut h);
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().map(|futures| futures.contract.as_str()), Some("ESH7"));
    let events = h.operate(AdminCommand::UpsertMarket(us500(more[1..].to_vec())));
    assert_eq!(events.last(), Some(&Event::AdminApplied));
    assert!(h.engine.operator_bounds().markups.is_empty(), "no notice was named or spent");
}

#[test]
fn a_notice_names_a_futures_markets_new_contracts_and_a_relisting_applies_them_once_from_its_time() {
    let (mut h, _) = held_us500(V87);
    let mut moved = all_contracts();
    moved[1].from += chrono::Duration::days(30);
    let at = h.ts + NOTICE_MS;
    let refusals = [
        (BTC, Some(moved.clone()), "market 1 is not priced from futures contracts"),
        (US500, Some(vec![]), "names at least one contract"),
        (US500, Some(vec![moved[1].clone(), moved[0].clone()]), "does not roll in after"),
    ];
    for (market, contracts, reason) in refusals {
        rejected(&h.operate(contracts_notice(market, contracts, at)), reason);
    }
    assert_eq!(h.operate(contracts_notice(US500, Some(moved.clone()), at)).last(), Some(&Event::AdminApplied));
    let relisting = AdminCommand::UpsertMarket(us500(moved.clone()));
    rejected(&h.operate(relisting.clone()), "market 20's futures contract change takes effect from");

    h.advance_to(at);
    assert_eq!(h.operate(relisting).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&US500].spec.contracts, moved);
    assert!(!h.engine.operator_bounds().markups.contains_key(&US500), "the relisting spends the notice");
    rejected(&h.operate(AdminCommand::UpsertMarket(us500(all_contracts()))), "needs a notice");
}

#[test]
fn before_v87_a_notice_cannot_name_futures_contracts() {
    let (mut h, _) = held_us500(V86);
    let notice = contracts_notice(US500, Some(all_contracts()), h.ts + NOTICE_MS);
    rejected(&h.operate(notice), "a futures contract notice needs protocol V87");
}

#[test]
fn from_v87_a_market_whose_front_stopped_before_a_gap_rolls_from_its_mark_once_it_has_had_no_price_for_seven_days() {
    assert_eq!(MARK_ROLL_WAIT_MS, 7 * DAY_MS);
    for version in [V86, V87] {
        let (mut h, long) = held_on_front_alone(version, all_contracts());
        assert_eq!(h.engine.markets()[&US500].mark, Some(f("6000")));
        // December stops quoting, never having quoted beside March: just past its freshness bound, a price
        // elsewhere finds it without a price.
        h.advance(2_000);
        h.price(BTC, "100000");
        let since = h.ts;
        let unpriced = h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.front_unpriced_since);
        assert_eq!(unpriced, (version == V87).then_some(since), "{version:?}");
        // A day past March's roll time March quotes, and the market waits.
        h.advance_to(common::start() + ROLL_AFTER_MS + DAY_MS);
        let early = futures_prices(&mut h, "ESH7", "6040");
        assert!(!early.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{version:?}: {early:?}");

        // Short of seven days without a price nothing rolls; at seven days March's quote rolls the market from its
        // mark, from `V87`.
        h.advance_to(since + MARK_ROLL_WAIT_MS - 1);
        let short = futures_prices(&mut h, "ESH7", "6040");
        assert!(!short.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{version:?}: {short:?}");
        h.advance(1);
        let first = futures_prices(&mut h, "ESH7", "6040");
        let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: f("-40") };
        assert_eq!(first.contains(&rolled), version == V87, "{version:?}: {first:?}");
        let index = h.engine.markets()[&US500].index(h.ts).map(|index| index.mid);
        assert_eq!(index.ok(), (version == V87).then(|| f("6000")), "{version:?}: the index takes up from the mark");

        let events = fall(&mut h);
        let closed = h.engine.accounts()[&long.account].positions.get(&US500).is_none_or(|p| p.qty.is_zero());
        assert_eq!(closed, version == V87, "{version:?}: {events:?}");
    }
}

#[test]
fn a_brief_front_outage_takes_no_move_off_the_books_however_long_past_the_roll_time() {
    let (mut h, long) = held_on_front_alone(V87, all_contracts());
    // December quotes on eight days past March's roll time, and March has never quoted.
    h.advance_to(common::start() + ROLL_AFTER_MS + 8 * DAY_MS);
    futures_prices(&mut h, "ESZ6", "6000");
    // The venues are held back for just past December's freshness bound while the market falls a quarter;
    // March's first quote comes back first and the market waits.
    h.advance(1_001);
    let first = futures_prices(&mut h, "ESH7", "4530");
    assert!(!first.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{first:?}");
    // December's quote measures the gap, the market rolls on it, and the fall closes the long out.
    let events = futures_prices(&mut h, "ESZ6", "4490");
    let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: f("-40") };
    assert!(events.contains(&rolled), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(h.engine.accounts()[&long.account].positions.get(&US500).is_none_or(|p| p.qty.is_zero()));
}

#[test]
fn a_market_with_no_mark_waits_for_a_gap_to_roll() {
    let mut h = futures_genesis(V87, all_contracts());
    // December has never quoted, so US 500 has no mark; March quotes seven days past its roll time.
    h.advance_to(common::start() + ROLL_AFTER_MS + MARK_ROLL_WAIT_MS);
    let events = futures_prices(&mut h, "ESH7", "6040");
    assert!(!events.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{events:?}");
    assert!(h.engine.markets()[&US500].index(h.ts).is_err());
    // With December quoting beside it, the gap is measured and the market rolls on it.
    let events = futures_prices(&mut h, "ESZ6", "6000");
    let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: f("-40") };
    assert!(events.contains(&rolled), "{events:?}");
    assert_eq!(h.engine.markets()[&US500].index(h.ts).map(|index| index.mid).ok(), Some(f("6000")));
}

#[test]
fn a_contract_added_at_once_rolls_only_on_a_measured_gap_so_a_stall_takes_no_move_off_the_books() {
    let (mut h, long) = held_on_front_alone(V87, vec![all_contracts()[0].clone()]);
    // US 500 is on December, its last contract. March is added at once, its roll time already past.
    let added = AdminCommand::UpsertMarket(us500(vec![all_contracts()[0].clone(), contract("ESH7", h.ts - DAY_MS)]));
    assert_eq!(h.operate(added).last(), Some(&Event::AdminApplied));

    // The venues are held back for an hour while the market falls a quarter. March's quote comes back first,
    // with December's quotes stale and no gap recorded: the market waits.
    h.advance(3_600_000);
    let first = futures_prices(&mut h, "ESH7", "4530");
    assert!(!first.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{first:?}");
    assert!(h.engine.markets()[&US500].index(h.ts).is_err());
    // December's quote measures the gap, the market rolls on it, and the fall closes the long out.
    let events = futures_prices(&mut h, "ESZ6", "4490");
    let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: f("-40") };
    assert!(events.contains(&rolled), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(h.engine.accounts()[&long.account].positions.get(&US500).is_none_or(|p| p.qty.is_zero()));
}

#[test]
fn a_market_whose_next_contracts_a_notice_dropped_rolls_from_its_mark_once_they_are_back_and_seven_days_pass() {
    let (mut h, long) = held_us500(V87);
    // A noticed relisting leaves December alone; December then stops quoting with no gap recorded.
    let front_only = vec![all_contracts()[0].clone()];
    let at = h.ts + NOTICE_MS;
    assert_eq!(h.operate(contracts_notice(US500, Some(front_only.clone()), at)).last(), Some(&Event::AdminApplied));
    h.advance_to(at);
    assert_eq!(h.operate(AdminCommand::UpsertMarket(us500(front_only))).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.gap), None);
    assert!(h.engine.markets()[&US500].index(h.ts).is_err(), "December has stopped quoting");

    // March and June go back at once, being after the last, and the engine sees December without a price.
    assert_eq!(h.operate(AdminCommand::UpsertMarket(us500(all_contracts()))).last(), Some(&Event::AdminApplied));
    let since = h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.front_unpriced_since);
    assert_eq!(since, Some(at));
    h.advance(1_000);
    let early = futures_prices(&mut h, "ESH7", "6040");
    assert!(!early.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{early:?}");

    // Seven days on, March's quote rolls the market from its mark.
    h.advance_to(at + MARK_ROLL_WAIT_MS);
    let events = futures_prices(&mut h, "ESH7", "6040");
    assert!(events.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&US500].index(h.ts).map(|index| index.mid).ok(), Some(f("6000")));
    let events = fall(&mut h);
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(h.engine.accounts()[&long.account].positions.get(&US500).is_none_or(|p| p.qty.is_zero()));
}

#[test]
fn a_front_outage_counts_afresh_once_a_next_contract_is_added_back() {
    let (mut h, long) = held_us500(V87);
    let front_only = vec![all_contracts()[0].clone()];
    let at = h.ts + 9 * DAY_MS;
    assert_eq!(h.operate(contracts_notice(US500, Some(front_only.clone()), at)).last(), Some(&Event::AdminApplied));
    // Just past December's freshness bound, a price elsewhere finds it without a price.
    h.advance(2_000);
    h.price(BTC, "100000");
    let since = h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.front_unpriced_since);
    assert_eq!(since, Some(h.ts));

    // The noticed relisting leaves December alone, and December quotes again.
    h.advance_to(at);
    assert_eq!(h.operate(AdminCommand::UpsertMarket(us500(front_only))).last(), Some(&Event::AdminApplied));
    futures_prices(&mut h, "ESZ6", "6000");
    // Just past its freshness bound March is added back, and its first quote comes in a fall a quarter down:
    // December's outage counts from now, so the market waits for a gap.
    h.advance(1_001);
    assert_eq!(h.operate(AdminCommand::UpsertMarket(us500(all_contracts()))).last(), Some(&Event::AdminApplied));
    let first = futures_prices(&mut h, "ESH7", "4530");
    assert!(!first.iter().any(|e| matches!(e, Event::MarketRolled { .. })), "{first:?}");
    let events = futures_prices(&mut h, "ESZ6", "4490");
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(h.engine.accounts()[&long.account].positions.get(&US500).is_none_or(|p| p.qty.is_zero()));
}

/// From `V88`, a contract added after the last to a futures market that holds positions needs a notice, as
/// any other change to its contracts does: one no venue quotes would be the contract the market must roll
/// onto once its front stops. Before `V88` it applies at once, as it still does to a market no one holds.
#[test]
fn a_contract_added_after_the_last_to_a_held_futures_market_needs_a_notice() {
    for (version, at_once) in [(V87, true), (ProtocolVersion::V88, false)] {
        let (mut h, _) = held_on_front_alone(version, vec![all_contracts()[0].clone()]);
        let unquoted = vec![all_contracts()[0].clone(), contract("ESX9", common::start() + ROLL_AFTER_MS)];
        let events = h.operate(AdminCommand::UpsertMarket(us500(unquoted.clone())));
        assert_eq!(events.last() == Some(&Event::AdminApplied), at_once, "{version:?}: {events:?}");
        if at_once {
            continue;
        }
        rejected(&events, "market 20's futures contract change needs a notice of at least seven days");
        // A noticed addition applies from the notice's time.
        let at = h.ts + NOTICE_MS;
        assert_eq!(h.operate(contracts_notice(US500, Some(unquoted.clone()), at)).last(), Some(&Event::AdminApplied));
        h.advance_to(at);
        assert_eq!(h.operate(AdminCommand::UpsertMarket(us500(unquoted.clone()))).last(), Some(&Event::AdminApplied));
        assert_eq!(h.engine.markets()[&US500].spec.contracts, unquoted);
    }
    let mut h = futures_genesis(ProtocolVersion::V88, vec![all_contracts()[0].clone()]);
    futures_prices(&mut h, "ESZ6", "6000");
    let unquoted = vec![all_contracts()[0].clone(), contract("ESX9", common::start() + ROLL_AFTER_MS)];
    let events = h.operate(AdminCommand::UpsertMarket(us500(unquoted.clone())));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "a market no one holds: {events:?}");
    assert_eq!(h.engine.markets()[&US500].spec.contracts, unquoted);
}

// ── Helpers ──

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

fn contract(name: &str, from: Millis) -> FuturesContract {
    FuturesContract { contract: name.into(), from: DateTime::<Utc>::from_timestamp_millis(from).expect("in range") }
}

/// US 500 around the clock at 5% margin, priced from `contracts`.
fn us500(contracts: Vec<FuturesContract>) -> MarketSpec {
    MarketSpec {
        contracts,
        spread_tiers: tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 50)]),
        markout_floor_bps: Some(f("1")),
        min_level_distance_bps: Some(25),
        ..market(US500, "US500", AssetClass::Index, Schedule::AlwaysOpen, 500)
    }
}

/// December, then March from `ROLL_AFTER_MS` after the start, then June.
fn all_contracts() -> Vec<FuturesContract> {
    let start = common::start();
    vec![
        contract("ESZ6", start - 90 * DAY_MS),
        contract("ESH7", start + ROLL_AFTER_MS),
        contract("ESM7", start + 120 * DAY_MS),
    ]
}

/// The operator's notice naming `contracts` for `market` from `effective_at`.
fn contracts_notice(market: MarketId, contracts: Option<Vec<FuturesContract>>, effective_at: Millis) -> AdminCommand {
    let notice = ListingNotice { markup_bps: None, contracts, ..ListingNotice::markup(0, effective_at) };
    AdminCommand::AnnounceListing { market, notice }
}

/// A log at `version` that lists BTC, and US 500 on `contracts`, at genesis, with the ceiling registry and the
/// access policy.
fn futures_genesis(version: ProtocolVersion, contracts: Vec<FuturesContract>) -> Harness {
    let mut h = Harness::empty();
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.admin(AdminCommand::SetParams(ProtocolParams { volume_tiers: launch_volume_tiers(), ..v54_params() }));
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });
    let btc = MarketSpec {
        spread_tiers: tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 20), (None, 50)]),
        markout_floor_bps: Some(f("1")),
        min_level_distance_bps: Some(20),
        ..market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
    };
    h.admin(AdminCommand::UpsertMarket(btc));
    h.admin(AdminCommand::UpsertMarket(us500(contracts)));
    h.cmd(Command::HouseCapital { amount: h.house_capital, chain_ref: "genesis".into() });
    let mut entries: Vec<CloseSpreadCeiling> = ceilings().into_iter().filter(|c| c.market == BTC).collect();
    entries.push(CloseSpreadCeiling { market: US500, max_close_spread_bps: 25 });
    h.admin(AdminCommand::SetCloseSpreadCeilings { ceilings: entries });
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h
}

/// Two venues quoting `contract` at `mid`, stamped now.
fn futures_prices(h: &mut Harness, contract: &str, mid: &str) -> Vec<Event> {
    let quotes = (0..2)
        .map(|i| SourceQuote { contract: Some(contract.into()), ..common::quote(&format!("venue{i}"), mid, h.ts) })
        .collect();
    h.cmd(Command::Prices { market: US500, quotes })
}

/// A client long 1 US 500 at 6,000 on $1,000 (6x), bought while December alone quotes, on `contracts`.
fn held_on_front_alone(version: ProtocolVersion, contracts: Vec<FuturesContract>) -> (Harness, common::Client) {
    let mut h = futures_genesis(version, contracts);
    let long = client(&mut h, 2, "1000");
    futures_prices(&mut h, "ESZ6", "6000");
    only_fill(&trade(&mut h, &long, US500, Side::Buy, "1", "6000"));
    (h, long)
}

/// `held_on_front_alone`, with March then quoting beside December, so the gap to it is recorded.
fn held_us500(version: ProtocolVersion) -> (Harness, common::Client) {
    let (mut h, long) = held_on_front_alone(version, all_contracts());
    futures_prices(&mut h, "ESH7", "6040");
    assert_eq!(h.engine.markets()[&US500].futures.as_ref().and_then(|futures| futures.gap), Some(f("-40")));
    (h, long)
}

/// March falling a quarter from 6,040, a price every 200 ms.
fn fall(h: &mut Harness) -> Vec<Event> {
    let mut all = Vec::new();
    for mid in ["5500", "5000", "4540"] {
        h.advance(200);
        all.extend(futures_prices(h, "ESH7", mid));
    }
    all
}

/// December stops quoting after March's roll time; March alone quotes, then falls a quarter.
fn expiry_then_fall(h: &mut Harness) -> Vec<Event> {
    h.advance_to(common::start() + ROLL_AFTER_MS + DAY_MS);
    let mut all = futures_prices(h, "ESH7", "6040");
    all.extend(fall(h));
    assert!(fills(&all).iter().all(|fill| fill.liquidation), "only close-outs fill: {all:?}");
    all
}
