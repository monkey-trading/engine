//! The price-rule bound from `V35`, through `Engine::apply`: a relisting that leaves a market with
//! open positions unpriced is refused unless the market is reduce-only.

mod common;

use common::{BTC, ETH, Harness, acct, market};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, Side};

fn rejected(reason: &str) -> Vec<Event> {
    vec![Event::AdminRejected { reason: reason.into() }]
}

/// The outcome of an admin command, leaving out the catch-up events a long advance adds.
fn op(h: &mut Harness, command: AdminCommand) -> Vec<Event> {
    h.operate(command).pop().into_iter().collect()
}

/// A trader holding a long in BTC, priced by three venues.
fn with_position() -> (Harness, AccountId) {
    let (mut h, trader) = position_before_v33();
    h = h.into_v33();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V35 });
    (h, trader)
}

/// A trader holding a long in BTC, priced by three venues, under `V21`: later versions screen
/// deposits and orders.
fn position_before_v33() -> (Harness, AccountId) {
    // `V21` takes unsigned requests.
    let mut h = Harness::v21();
    let trader = acct(1);
    h.deposit(trader, "100000");
    h.quotes(BTC, &["100000", "100000", "100000"]);
    let events = h.market_order(trader, BTC, Side::Buy, "1", "100000");
    assert!(matches!(&events[..], [.., Event::Fill(_)]), "{events:?}");
    (h, trader)
}

fn btc_with(h: &Harness, change: impl FnOnce(&mut MarketSpec)) -> AdminCommand {
    let mut spec = h.engine.markets()[&BTC].spec.clone();
    change(&mut spec);
    AdminCommand::UpsertMarket(spec)
}

#[test]
fn an_upsert_that_leaves_a_market_with_open_positions_unpriced_is_refused() {
    let (mut h, _) = with_position();
    let reason = "market 1 has open positions, so price rules it cannot price under need it reduce-only first";
    // Three venues quote now: a fourth source required, quotes older than the age allowed, and a
    // divergence narrower than the venues' own (here none, so a bound of zero still prices).
    let more_sources = btc_with(&h, |spec| spec.price_rules.min_sources = 4);
    assert_eq!(op(&mut h, more_sources), rejected(reason));
    h.advance(600);
    let fresher = btc_with(&h, |spec| spec.price_rules.max_age_ms = 500);
    assert_eq!(op(&mut h, fresher), rejected(reason));
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules.min_sources, 2);
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules.max_age_ms, 1_000);
    h.quotes(BTC, &["100000", "100010", "100000"]);
    let narrower = btc_with(&h, |spec| spec.price_rules.max_divergence_bps = 0);
    assert_eq!(op(&mut h, narrower), rejected(reason));
}

#[test]
fn tighter_price_rules_that_still_price_the_market_are_accepted() {
    let (mut h, _) = with_position();
    let same_sources = btc_with(&h, |spec| spec.price_rules.min_sources = 3);
    assert_eq!(op(&mut h, same_sources).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules.min_sources, 3);
}

#[test]
fn looser_price_rules_are_accepted_for_a_market_that_is_unpriced() {
    let (mut h, _) = with_position();
    h.advance(5_000);
    h.cmd(Command::Tick);
    let looser = btc_with(&h, |spec| {
        spec.price_rules.min_sources = 1;
        spec.price_rules.max_age_ms = 10_000;
    });
    assert_eq!(op(&mut h, looser).last(), Some(&Event::AdminApplied));
}

#[test]
fn tighter_price_rules_for_a_market_with_no_price_are_refused_as_they_cannot_be_shown_to_price() {
    let (mut h, _) = with_position();
    // Every quote is stale: the market has no price to show the new rules still give one.
    h.advance(5_000);
    h.cmd(Command::Tick);
    let tighter = btc_with(&h, |spec| spec.price_rules.min_sources = 9);
    assert_eq!(
        op(&mut h, tighter),
        rejected("market 1 has open positions, so price rules it cannot price under need it reduce-only first")
    );
}

#[test]
fn a_reduce_only_market_may_be_given_price_rules_that_leave_it_unpriced() {
    let (mut h, _) = with_position();
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    let unpriced = btc_with(&h, |spec| spec.price_rules.min_sources = 4);
    assert_eq!(op(&mut h, unpriced).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&BTC].spec.price_rules.min_sources, 4);
}

#[test]
fn every_market_reduce_only_also_allows_price_rules_that_leave_a_market_unpriced() {
    let (mut h, _) = with_position();
    h.operator_admin(AdminCommand::SetReduceOnly { market: None, reduce_only: true });
    let unpriced = btc_with(&h, |spec| spec.price_rules.min_sources = 4);
    assert_eq!(op(&mut h, unpriced).last(), Some(&Event::AdminApplied));
}

#[test]
fn a_market_without_open_positions_may_be_given_price_rules_that_leave_it_unpriced() {
    let (mut h, _) = with_position();
    let eth = h.engine.markets()[&ETH].spec.clone();
    assert!(h.engine.accounts().values().all(|account| !account.positions.contains_key(&ETH)));
    let unpriced = AdminCommand::UpsertMarket(MarketSpec {
        price_rules: engine_core::market::PriceRules { min_sources: 4, ..eth.price_rules },
        ..eth
    });
    assert_eq!(op(&mut h, unpriced).last(), Some(&Event::AdminApplied));
}

#[test]
fn a_new_listing_needs_no_price() {
    let mut h = Harness::v35();
    let mut sol = market(10, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    sol.price_rules.min_sources = 9;
    assert_eq!(op(&mut h, AdminCommand::UpsertMarket(sol)).last(), Some(&Event::AdminApplied));
}

#[test]
fn before_v35_price_rules_may_leave_a_market_with_open_positions_unpriced() {
    let (h, _) = position_before_v33();
    let mut h = h.into_v33();
    let mut spec = h.engine.markets()[&BTC].spec.clone();
    spec.price_rules.min_sources = 4;
    assert_eq!(op(&mut h, AdminCommand::UpsertMarket(spec)).last(), Some(&Event::AdminApplied));
}

/// `h` after `V33` and `V35`.
fn into_v35(h: Harness) -> Harness {
    let mut h = h.into_v33();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V35 });
    h
}

#[test]
fn the_quotes_a_relisting_drops_do_not_count_towards_the_price_it_must_keep() {
    let mut h = Harness::v21();
    let trader = acct(1);
    h.deposit(trader, "100000");
    h.quotes(BTC, &["100000", "100000", "100000"]);
    h.market_order(trader, BTC, Side::Buy, "1", "100000");
    // Venue 2's quote is 1,500 ms old, stale under the 1,000 ms limit but still held.
    h.advance(1_500);
    h.cmd(Command::Prices {
        market: BTC,
        quotes: vec![common::quote("venue0", "100000", h.ts), common::quote("venue1", "100000", h.ts)],
    });
    let mut h = into_v35(h);
    // Three sources fresh under a 2,000 ms limit, but a relisting that raises the limit drops the
    // quote stale under the old one, leaving two.
    let relist = btc_with(&h, |spec| {
        spec.price_rules.min_sources = 3;
        spec.price_rules.max_age_ms = 2_000;
    });
    assert_eq!(
        op(&mut h, relist),
        rejected("market 1 has open positions, so price rules it cannot price under need it reduce-only first")
    );
}

#[test]
fn price_rules_that_unprice_the_market_a_held_market_converts_at_are_refused() {
    let mut h = Harness::v21();
    h.list_fx();
    let trader = acct(1);
    h.deposit(trader, "100000");
    h.quotes(common::EURUSD, &["1.1", "1.1", "1.1"]);
    h.quotes(common::DE40, &["20000", "20000", "20000"]);
    h.market_order(trader, common::DE40, Side::Buy, "1", "20000");
    let mut h = into_v35(h);
    // Nobody holds EUR-USD, but Germany 40 is quoted in euros at its index.
    let eurusd = h.engine.markets()[&common::EURUSD].spec.clone();
    let mut tighter = eurusd.clone();
    tighter.price_rules.min_sources = 4;
    let reason = "market 6 has open positions and converts at market 4, which the price rules cannot price under, so it needs to be reduce-only first";
    assert_eq!(op(&mut h, AdminCommand::UpsertMarket(tighter.clone())), rejected(reason));
    h.operator_admin(AdminCommand::SetReduceOnly { market: Some(common::DE40), reduce_only: true });
    assert_eq!(op(&mut h, AdminCommand::UpsertMarket(tighter)), vec![Event::AdminApplied]);
}

#[test]
fn a_futures_market_must_still_give_a_valid_adjusted_index_under_tighter_rules() {
    use engine_core::futures::FuturesContract;
    let start = common::start();
    let contract = |name: &str, from: engine_core::types::Millis| FuturesContract {
        contract: name.into(),
        from: chrono::DateTime::from_timestamp_millis(from).expect("in range"),
    };
    let mut spec = market(20, "US500", AssetClass::Index, Schedule::AlwaysOpen, 500);
    spec.contracts = vec![contract("ESZ6", start - 86_400_000), contract("ESH7", start + 3_600_000)];
    spec.price_rules.max_divergence_bps = 10_000;
    let mut h = Harness::v21();
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    let quotes = |h: &Harness, contract: &str, mids: &[(&str, &str)]| Command::Prices {
        market: 20,
        quotes: mids
            .iter()
            .map(|(venue, mid)| engine_core::price::SourceQuote {
                contract: Some(contract.into()),
                ..common::quote(venue, mid, h.ts)
            })
            .collect(),
    };
    let trader = acct(1);
    h.deposit(trader, "100000");
    // The roll takes a gap of -40 into the adjustment: March trades 40 above December.
    h.cmd(quotes(&h, "ESZ6", &[("venue0", "100"), ("venue1", "100")]));
    h.cmd(quotes(&h, "ESH7", &[("venue0", "140"), ("venue1", "140")]));
    h.market_order(trader, 20, Side::Buy, "1", "100");
    h.advance(3_600_000);
    h.cmd(quotes(&h, "ESZ6", &[("venue0", "100"), ("venue1", "100")]));
    h.cmd(quotes(&h, "ESH7", &[("venue0", "140"), ("venue1", "140")]));
    assert_eq!(h.engine.markets()[&20].futures.as_ref().map(|f| f.adjustment), Some(common::f("-40")));
    // Three venues quote 150 and, 750 ms later, two quote 30: the median of all five gives 110, the
    // two fresh ones alone 30, whose adjusted index of -10 is no price.
    h.cmd(quotes(&h, "ESH7", &[("venue0", "150"), ("venue1", "150"), ("venue2", "150")]));
    h.advance(750);
    h.cmd(quotes(&h, "ESH7", &[("venue3", "30"), ("venue4", "30")]));
    let mut h = into_v35(h);
    assert!(h.engine.markets()[&20].index(h.ts).is_ok());
    let mut shorter = h.engine.markets()[&20].spec.clone();
    shorter.price_rules.max_age_ms = 500;
    let reason = "market 20 has open positions, so price rules it cannot price under need it reduce-only first";
    assert_eq!(op(&mut h, AdminCommand::UpsertMarket(shorter.clone())), rejected(reason));
    shorter.price_rules.max_age_ms = 800;
    assert_eq!(op(&mut h, AdminCommand::UpsertMarket(shorter)), vec![Event::AdminApplied]);
}
