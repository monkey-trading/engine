//! `V68`'s tier benefits, through `Engine::apply`: tiers 2 and 3 pay the base profit fee less their
//! discount past the hold curve, and the curve's fee where it is above every tier's discounted base;
//! each rollover cuts a position's markup by its account's discount; an account's emission weight grows
//! by its tier's boost at the period's close; the account read gives the three; and before `V68` every
//! tier pays the same. Activation gives a log's table its benefits by position. The benefits' bounds are
//! in `tier_benefit_bounds.rs`.

mod common;

use common::close_spread::trade;
use common::emissions::{allocations, next_period};
use common::events::{fills, only_fill};
use common::toxicity::v54_params;
use common::weekend::{GOLD, OIL, OIL_WEEKEND_BPS, client, genesis_with_params, globex};
use common::{BTC, Client, ETH, Harness, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::{Event, Fill};
use engine_core::holding_fee::{DEFAULT_PROFIT_FEE_CURVE, curve_fee_bps, discounted_base_bps, fee_at};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use engine_core::volume_tier::{
    ACTIVATION_EMISSIONS_BOOSTS_BPS, ACTIVATION_FEE_DISCOUNTS_BPS, ACTIVATION_FINANCING_DISCOUNTS_BPS, MIN_HOLD_MS,
    VolumeTier,
};

const V66: ProtocolVersion = ProtocolVersion::V66;
const V68: ProtocolVersion = ProtocolVersion::V68;
const MINUTE_MS: i64 = 60_000;

#[test]
fn a_winning_close_pays_the_base_less_the_discount_past_the_curve_and_the_curve_before_it() {
    let (mut h, [a, b]) = two_accounts(V68, [2, 0]);
    // 45 minutes is past the curve, which reaches the 10% base at 15: tier 2 pays 9%, tier 0 10%.
    assert_eq!(win(&mut h, &a, 45 * MINUTE_MS).profit_fee_bps, Some(900));
    assert_eq!(win(&mut h, &b, 45 * MINUTE_MS).profit_fee_bps, Some(1_000));
    // At two minutes the curve is above both bases, and both pay it.
    let curve = curve_fee_bps(&DEFAULT_PROFIT_FEE_CURVE, 2 * MINUTE_MS);
    assert_eq!(curve, 5_081);
    assert_eq!(win(&mut h, &a, 2 * MINUTE_MS).profit_fee_bps, Some(curve));
    assert_eq!(win(&mut h, &b, 2 * MINUTE_MS).profit_fee_bps, Some(curve));
}

#[test]
fn the_fee_is_the_discounted_bases_charge_on_the_gain_rounded_up() {
    let (mut h, [a, _]) = two_accounts(V68, [2, 0]);
    let close = win(&mut h, &a, 45 * MINUTE_MS);
    assert!(close.realised_pnl.is_positive(), "{close:?}");
    assert_eq!(close.profit_fee, close.realised_pnl.mul_bps_rounded(900, engine_core::fixed::Rounding::Up));
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn the_tier_is_read_at_the_close() {
    let (mut h, [a, _]) = two_accounts(V68, [2, 0]);
    // The position opens at tier 2, and its tier falls with the volume window before it closes.
    open_btc(&mut h, &a);
    h.advance(45 * MINUTE_MS);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(a.account), 2);
    assert_eq!(h.engine.close_fee(a.account, BTC).unwrap().fee_bps, 900);
    h.advance(30 * engine_core::volume_tier::DAY_MS);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(a.account), 0);
    assert_eq!(h.engine.close_fee(a.account, BTC).unwrap().fee_bps, 1_000);
    h.price(BTC, "101000");
    let close = only_fill(&trade(&mut h, &a, BTC, Side::Sell, "0.1", "101000")).clone();
    assert_eq!(close.profit_fee_bps, Some(1_000));
}

#[test]
fn the_close_fee_read_gives_the_discounted_base_and_when_the_curve_next_falls() {
    let (mut h, [a, b]) = two_accounts(V68, [2, 0]);
    open_btc(&mut h, &a);
    open_btc(&mut h, &b);
    h.advance(20 * MINUTE_MS);
    h.price(BTC, "100000");
    let (fee_a, fee_b) = (h.engine.close_fee(a.account, BTC).unwrap(), h.engine.close_fee(b.account, BTC).unwrap());
    assert_eq!((fee_a.fee_bps, fee_a.falls_at), (900, None));
    assert_eq!((fee_b.fee_bps, fee_b.falls_at), (1_000, None));
    // Inside the curve both read its fee, and it next falls at the same time.
    let (mut h, [a, b]) = two_accounts(V68, [2, 0]);
    open_btc(&mut h, &a);
    open_btc(&mut h, &b);
    h.advance(5 * MINUTE_MS);
    h.price(BTC, "100000");
    let (fee_a, fee_b) = (h.engine.close_fee(a.account, BTC).unwrap(), h.engine.close_fee(b.account, BTC).unwrap());
    assert_eq!(fee_a.fee_bps, curve_fee_bps(&DEFAULT_PROFIT_FEE_CURVE, 5 * MINUTE_MS));
    assert_eq!((fee_a.fee_bps, fee_a.falls_at.is_some()), (fee_b.fee_bps, true));
    assert_eq!(fee_a.falls_at.unwrap() - fee_a.opened_at, fee_b.falls_at.unwrap() - fee_b.opened_at);
}

#[test]
fn before_v68_every_tier_pays_the_base() {
    let (mut h, [a, b]) = two_accounts(V66, [2, 0]);
    assert_eq!(win(&mut h, &a, 45 * MINUTE_MS).profit_fee_bps, Some(1_000));
    assert_eq!(win(&mut h, &b, 45 * MINUTE_MS).profit_fee_bps, Some(1_000));
    let standing = h.engine.volume_standing(a.account).unwrap();
    assert_eq!(
        (standing.fee_discount_bps, standing.financing_discount_bps, standing.emissions_boost_bps),
        (None, None, None)
    );
}

#[test]
fn a_higher_tier_never_pays_more_at_any_hold_or_admitted_base() {
    let discounts = ACTIVATION_FEE_DISCOUNTS_BPS;
    for base in (1_000..=1_500).step_by(7) {
        for hold in (0..=20 * MINUTE_MS).step_by(997) {
            let fees: Vec<i64> = discounts
                .iter()
                .map(|d| fee_at(discounted_base_bps(base, *d), &DEFAULT_PROFIT_FEE_CURVE, hold))
                .collect();
            assert!(fees.windows(2).all(|pair| pair[0] >= pair[1]), "base {base} hold {hold}: {fees:?}");
            // Every tier pays at least what it paid at the launch base.
            let launch: Vec<i64> = discounts
                .iter()
                .map(|d| fee_at(discounted_base_bps(1_000, *d), &DEFAULT_PROFIT_FEE_CURVE, hold))
                .collect();
            assert!(fees.iter().zip(&launch).all(|(fee, at_launch)| fee >= at_launch), "base {base} hold {hold}");
        }
    }
    // A discounted base rounds up to the bp, so a discount never takes more than it names.
    assert_eq!(discounted_base_bps(1_001, 1_000), 901);
    assert_eq!(discounted_base_bps(1_000, 2_000), 800);
    assert_eq!(discounted_base_bps(1_000, 0), 1_000);
    assert_eq!(discounted_base_bps(0, 2_000), 0);
}

#[test]
fn a_rollover_cuts_a_positions_markup_by_its_accounts_discount() {
    let (mut h, [a, b]) = two_accounts(V68, [2, 0]);
    for client in [&a, &b] {
        open_btc(&mut h, client);
        only_fill(&trade(&mut h, client, ETH, Side::Sell, "2", "2500"));
    }
    h.advance_to(h.engine.next_rollover().unwrap());
    let events = h.cmd(Command::Tick);
    let charge = |account: &Client, market: MarketId| financing(&events, account, market);
    // The same long and short at the same marks: tier 2 pays the benchmark plus a markup cut by a
    // tenth, and receives the benchmark less it.
    let markup = h.engine.markets()[&BTC].spec.financing.markup_bps;
    let cut = |bps: i64| Fixed::from_bps(bps).mul_bps_rounded(9_000, engine_core::fixed::Rounding::Up);
    assert_eq!(markup, h.engine.markets()[&ETH].spec.financing.markup_bps);
    let rate = |markup: Fixed, side: i128| {
        let day = Fixed::from_bps(430);
        (if side > 0 { day + markup } else { markup - day }, side)
    };
    let amount = |market: MarketId, qty: &str, markup: Fixed, side: i128| {
        let mark = h.engine.markets()[&market].mark.unwrap();
        let (annual, _) = rate(markup, side);
        let notional = Fixed::from_raw(f(qty).raw()).mul_rounded(mark, engine_core::fixed::Rounding::Up);
        notional
            .mul_rounded(annual, engine_core::fixed::Rounding::Up)
            .div_int_rounded(365, engine_core::fixed::Rounding::Up)
    };
    assert_eq!(charge(&b, BTC), amount(BTC, "0.1", Fixed::from_bps(markup), 1));
    assert_eq!(charge(&a, BTC), amount(BTC, "0.1", cut(markup), 1));
    assert_eq!(charge(&b, ETH), amount(ETH, "2", Fixed::from_bps(markup), -1));
    assert_eq!(charge(&a, ETH), amount(ETH, "2", cut(markup), -1));
    assert!(charge(&a, BTC) < charge(&b, BTC) && charge(&a, ETH) < charge(&b, ETH));
}

#[test]
fn a_close_out_inside_a_catch_up_leaves_the_volume_the_later_rollovers_read() {
    // Tier 2 needs $2 of counted volume, so A's one BTC fill reaches it and expires from the window 30
    // days on. A holds a thin oil long that the first Friday's weekend close liquidates inside the same
    // catch-up as every later rollover; that close-out must not drop the day A's tier still needs.
    let mut h = genesis_with(V68, one_dollar_tiers(), OIL_WEEKEND_BPS);
    let a = client(&mut h, 1, "10000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "0.001", "100000"));
    h.advance(MIN_HOLD_MS);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(a.account), 2);
    h.advance(33 * engine_core::volume_tier::DAY_MS);
    let events = h.cmd(Command::Tick);
    assert!(fills(&events).iter().any(|fill| fill.liquidation && fill.market == OIL), "{events:?}");
    let charged: Vec<Fixed> = events
        .iter()
        .filter_map(|event| match event {
            Event::Financing { account, market, amount } if *account == a.account && *market == BTC => Some(*amount),
            _ => None,
        })
        .collect();
    let discounted = charged[0];
    let at_tier_two = charged.iter().take_while(|amount| **amount == discounted).count();
    assert!((29..=30).contains(&at_tier_two), "{at_tier_two} of {}", charged.len());
    assert!(at_tier_two < charged.len(), "A's volume has left the window by the last rollover");
}

#[test]
fn before_v68_every_account_pays_the_listed_markup() {
    let (mut h, [a, b]) = two_accounts(V66, [2, 0]);
    open_btc(&mut h, &a);
    open_btc(&mut h, &b);
    h.advance_to(h.engine.next_rollover().unwrap());
    let events = h.cmd(Command::Tick);
    assert_eq!(financing(&events, &a, BTC), financing(&events, &b, BTC));
}

#[test]
fn a_periods_weight_grows_by_its_accounts_tier_boost_at_the_close() {
    let mut h = emitting(V68);
    let tiered: Vec<Client> = (0..4).map(|tier| at_tier(&mut h, 10 + tier as u8, tier)).collect();
    for client in &tiered {
        lose(&mut h, client);
    }
    let allocated = allocations(&next_period(&mut h));
    let weights: Vec<Fixed> = tiered
        .iter()
        .map(|client| allocated.iter().find(|(account, ..)| *account == client.account).expect("allocated").2)
        .collect();
    // Equal losses weigh 1, 1.1, 1.25 and 1.5 times.
    let base = weights[0];
    assert!(base.is_positive());
    for (weight, boost) in weights.iter().zip(ACTIVATION_EMISSIONS_BOOSTS_BPS) {
        assert_eq!(*weight, base.mul_bps(10_000 + boost));
    }
    // The budget is shared by the boosted weights.
    let total: i128 = weights.iter().map(|w| w.raw()).sum();
    for (_, _, weight, amount) in &allocated {
        assert_eq!(*amount, 1_000_000 * weight.raw().unsigned_abs() / total.unsigned_abs());
    }
}

#[test]
fn the_boost_is_the_tiers_at_the_periods_end_however_late_a_command_closes_it() {
    let mut h = emitting(V68);
    let [a, b] = [at_tier(&mut h, 10, 2), at_tier(&mut h, 11, 0)];
    lose(&mut h, &a);
    lose(&mut h, &b);
    assert_eq!(h.engine.volume_tier(a.account), 2);
    // Thirty days on, the first command closes the period after tier 2's volume has left the window;
    // the period ended while A was at tier 2, so its weight is boosted at tier 2's 25%.
    h.advance(30 * engine_core::volume_tier::DAY_MS);
    let allocated = allocations(&h.price(BTC, "100000"));
    assert_eq!(h.engine.volume_tier(a.account), 0);
    let weight = |client: &Client| allocated.iter().find(|(account, ..)| *account == client.account).map(|e| e.2);
    let (weight_a, weight_b) = (weight(&a).expect("allocated"), weight(&b).expect("allocated"));
    assert_eq!(weight_a, weight_b.mul_bps(12_500));
}

#[test]
fn a_caught_up_rollover_reads_the_tier_at_the_rollover_not_at_the_catching_up_command() {
    let (mut h, [a, b]) = two_accounts(V68, [2, 0]);
    open_btc(&mut h, &a);
    open_btc(&mut h, &b);
    // Thirty-two days pass with no command; the first one after them charges every missed rollover. A's
    // gold volume leaves the window at midnight 30 days after its day, between two of the rollovers.
    h.advance(32 * engine_core::volume_tier::DAY_MS);
    let events = h.cmd(Command::Tick);
    let charges = |client: &Client| -> Vec<Fixed> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Financing { account, market, amount } if *account == client.account && *market == BTC => {
                    Some(*amount)
                }
                _ => None,
            })
            .collect()
    };
    let (charged_a, charged_b) = (charges(&a), charges(&b));
    assert!(charged_a.len() >= 30, "{}", charged_a.len());
    assert_eq!(charged_a.len(), charged_b.len());
    // B pays the same each day; A pays a tenth of the markup less on the days it was at tier 2, then
    // the full markup once its volume has left the window.
    let full = charged_b[0];
    assert!(charged_b.iter().all(|amount| *amount == full));
    let discounted = charged_a[0];
    assert!(discounted < full);
    let at_tier_two = charged_a.iter().take_while(|amount| **amount == discounted).count();
    assert!((29..=30).contains(&at_tier_two), "{at_tier_two}");
    assert!(charged_a[at_tier_two..].iter().all(|amount| *amount == full), "{charged_a:?}");
    assert!(at_tier_two < charged_a.len());
}

#[test]
fn before_v68_no_tier_boosts_a_weight() {
    let mut h = emitting(V66);
    let [a, b] = [at_tier(&mut h, 10, 2), at_tier(&mut h, 11, 0)];
    lose(&mut h, &a);
    lose(&mut h, &b);
    let allocated = allocations(&next_period(&mut h));
    let weights: Vec<Fixed> = allocated.iter().map(|e| e.2).collect();
    assert_eq!(weights.len(), 2);
    assert_eq!(weights[0], weights[1]);
}

#[test]
fn the_account_read_gives_the_tiers_benefits() {
    let (h, [a, b]) = two_accounts(V68, [2, 0]);
    let read = |client: &Client| {
        let standing = h.engine.volume_standing(client.account).unwrap();
        (standing.tier, standing.fee_discount_bps, standing.financing_discount_bps, standing.emissions_boost_bps)
    };
    assert_eq!(read(&a), (2, Some(1_000), Some(1_000), Some(2_500)));
    assert_eq!(read(&b), (0, Some(0), Some(0), Some(0)));
}

#[test]
fn activation_gives_a_tables_tiers_their_benefits_by_position_and_before_it_a_table_names_none() {
    let (mut h, _) = two_accounts(V66, [0, 0]);
    let column = |tiers: &[VolumeTier], read: fn(&VolumeTier) -> i64| tiers.iter().map(read).collect::<Vec<_>>();
    let params = h.engine.params().unwrap().volume_tiers.clone();
    assert_eq!(column(&params, |t| t.fee_discount_bps), [0, 0, 0, 0]);
    h.admin(AdminCommand::ActivateProtocol { version: V68 });
    let launch = h.engine.operator_bounds().launch_volume_tiers.clone().unwrap();
    for tiers in [&h.engine.params().unwrap().volume_tiers, &launch] {
        assert_eq!(column(tiers, |t| t.fee_discount_bps), ACTIVATION_FEE_DISCOUNTS_BPS);
        assert_eq!(column(tiers, |t| t.financing_discount_bps), ACTIVATION_FINANCING_DISCOUNTS_BPS);
        assert_eq!(column(tiers, |t| t.emissions_boost_bps), ACTIVATION_EMISSIONS_BOOSTS_BPS);
    }
}

// ── Helpers ──

/// A table whose tiers a few gold fills reach in a day: $100k counted reaches tier 1, $400k tier 2 and
/// $600k tier 3. With `version` at `V68` it names the benefits the activation gives by position.
fn tiers(version: ProtocolVersion) -> Vec<VolumeTier> {
    [(0, 2_500), (100_000, 5_000), (400_000, 10_000), (600_000, 25_000)]
        .into_iter()
        .enumerate()
        .map(|(at, (usd, best_price_share_bps))| {
            let named = version >= V68;
            VolumeTier {
                threshold: Fixed::from_int(usd),
                day_divisor: 1,
                best_price_share_bps,
                margin_floor_bps: 0,
                max_resting_orders: 0,
                requests_per_second: 0,
                fee_discount_bps: if named { ACTIVATION_FEE_DISCOUNTS_BPS[at] } else { 0 },
                financing_discount_bps: if named { ACTIVATION_FINANCING_DISCOUNTS_BPS[at] } else { 0 },
                emissions_boost_bps: if named { ACTIVATION_EMISSIONS_BOOSTS_BPS[at] } else { 0 },
            }
        })
        .collect()
}

/// A log at `version` with `tiers(version)`, BTC at $100,000, ETH at $2,500 and gold at $2,000.
fn genesis(version: ProtocolVersion) -> Harness {
    let mut h = genesis_with(version, tiers(version), 0);
    h.price(GOLD, "2000");
    h.price(ETH, "2500");
    h.price(BTC, "100000");
    h
}

/// A log at `version` with the default curve, `volume_tiers` and an oil weekend margin of `oil_bps`.
fn genesis_with(version: ProtocolVersion, volume_tiers: Vec<VolumeTier>, oil_bps: i64) -> Harness {
    let params = ProtocolParams { volume_tiers, profit_fee_curve: DEFAULT_PROFIT_FEE_CURVE.to_vec(), ..v54_params() };
    genesis_with_params(version, globex(vec![]), oil_bps, 0, params)
}

/// `tiers(V68)` with the thresholds of tiers 1 to 3 at $1, $2 and $10M.
fn one_dollar_tiers() -> Vec<VolumeTier> {
    let mut table = tiers(V68);
    table.iter_mut().zip([0, 1, 2, 10_000_000]).for_each(|(tier, usd)| tier.threshold = Fixed::from_int(usd));
    table
}

/// Two clients, seeds 1 and 2, each holding $100,000 and counted into the tier named for it: at 2,
/// gold fills worth $488k that have matured.
fn two_accounts(version: ProtocolVersion, tier_of: [usize; 2]) -> (Harness, [Client; 2]) {
    let mut h = genesis(version);
    let clients = [1u8, 2].map(|seed| client(&mut h, seed, "100000"));
    for (client, tier) in clients.iter().zip(tier_of) {
        reach(&mut h, client, tier);
    }
    (h, clients)
}

/// Client `seed`, holding $100,000 and counted into `tier`.
fn at_tier(h: &mut Harness, seed: u8, tier: usize) -> Client {
    let client = client(h, seed, "100000");
    reach(h, &client, tier);
    client
}

/// Sells gold worth enough, once matured, to reach `tier`: $198k, $488k and $608k counted.
fn reach(h: &mut Harness, client: &Client, tier: usize) {
    let sells = [None, Some("99"), Some("244"), Some("304")][tier];
    if let Some(qty) = sells {
        only_fill(&trade(h, client, GOLD, Side::Sell, qty, "2000"));
    }
    h.advance(MIN_HOLD_MS);
    h.price(GOLD, "2000");
    h.price(ETH, "2500");
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(client.account), tier);
}

/// Buys 0.1 BTC at $100,000.
fn open_btc(h: &mut Harness, client: &Client) {
    h.price(BTC, "100000");
    only_fill(&trade(h, client, BTC, Side::Buy, "0.1", "100000"));
}

/// Opens 0.1 BTC, holds it `hold_ms` and closes it at $101,000.
fn win(h: &mut Harness, client: &Client, hold_ms: i64) -> Fill {
    open_btc(h, client);
    h.advance(hold_ms);
    h.price(BTC, "101000");
    only_fill(&trade(h, client, BTC, Side::Sell, "0.1", "101000")).clone()
}

/// Buys 0.1 BTC at $100,000 and sells it at $99,000 on a fresh price.
fn lose(h: &mut Harness, client: &Client) {
    open_btc(h, client);
    h.price(BTC, "99000");
    only_fill(&trade(h, client, BTC, Side::Sell, "0.1", "99000"));
}

/// The financing `client` paid on `market` among `events`; negative when it received.
fn financing(events: &[Event], client: &Client, market: MarketId) -> Fixed {
    events
        .iter()
        .find_map(|event| match event {
            Event::Financing { account, market: m, amount } if *account == client.account && *m == market => {
                Some(*amount)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no financing for {market}: {events:?}"))
}

/// A log at `version` with the tier table, emissions started now at 1,000,000 base units a period, and a
/// signed account for each helper client.
fn emitting(version: ProtocolVersion) -> Harness {
    let mut h = genesis(version);
    let schedule = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(1_000_000) };
    h.operator_admin(AdminCommand::InitEmissions(schedule));
    h
}
