//! Portfolio margin as a read (`portfolio`), through `Engine::apply`: a hedged book's matched exposure
//! gets its group's credit, never more than `1 - φ` of its bands; a lone position, and every position
//! that offsets nothing, keeps its per-position requirement exactly, in every market and at every
//! size and tier; the initial requirement takes the tier floor in each band, a group's weekend shocks
//! over its members' weekend gap, where a market with no weekend rate takes no credit, and no credit for
//! a group with a member that cannot trade; the offsets add up to what the requirement saves; and a
//! grid that does not fit the listings is refused.

mod common;

use common::close_spread::trade;
use common::events::{fills, only_fill};
use common::weekend::{GOLD, NIKKEI, OIL, OIL_WEEKEND_BPS, chicago, client, genesis_with_tiers, globex};
use common::{AAPL, BTC, Client, ETH, Harness, USDJPY, f};
use engine_core::Fixed;
use engine_core::fixed::Rounding;
use engine_core::portfolio::{FactorMove, GroupMember, GroupStanding, OffsetGroup, PortfolioGrid, PortfolioStanding};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use engine_core::volume_tier::launch_volume_tiers;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

#[test]
fn a_hedged_pair_takes_its_matched_exposures_credit() {
    // $700k of oil long and $700k of gold short loading 0.9, at 1% each, and $100k of BTC alone at 10%:
    // $24,000 per position. Gold's $630k of loading-weighted exposure is matched whole, and oil's $700k
    // in the ratio 0.9: matched bands of $13,300 against a grid charge of 0.2% of each matched part and
    // its 25bp closing cost over the 50% level, 0.7% in all, $9,310. The credit is $3,990, below 70% of
    // the matched bands, and BTC keeps its $10,000.
    let (h, a) = book(0);
    let grid = grid(3_000);
    assert_eq!(h.equity_and_margin(a.account).1, f("24000"));
    let group = GroupStanding {
        members: vec![OIL, GOLD],
        net_notional: f("70000"),
        matched_notional: f("1260000"),
        worst_loss: f("2660"),
        binding_direction: FactorMove::Down,
        offset: f("3990"),
    };
    let expected = PortfolioStanding {
        margin: f("20010"),
        initial_margin: f("20010"),
        maintenance_margin: f("10005"),
        groups: [("commodities".to_owned(), group)].into(),
    };
    assert_eq!(h.engine.portfolio_standing(a.account, &grid), Ok(Some(expected)));
    assert_eq!(h.engine.portfolio_margin(&h.engine.accounts()[&a.account], &grid), f("20010"));
    // A floor share of 95% leaves a credit of 5% of the matched bands, $665.
    assert_eq!(standing(&h, &a, &self::grid(9_500)).margin, f("23335"));
}

#[test]
fn a_net_short_binds_on_the_factor_rising() {
    let (mut h, a) = book(0);
    // Selling 70 more ounces of gold takes the net to $56k short: oil is now the side matched whole, and
    // gold in the ratio 700/756.
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "70", "2000"));
    let group = &standing(&h, &a, &grid(3_000)).groups["commodities"];
    assert_eq!((group.net_notional, group.binding_direction), (f("-56000"), FactorMove::Up));
    assert_eq!((group.matched_notional, group.worst_loss), (f("1400000"), f("2955.555555556")));
}

#[test]
fn an_exactly_hedged_group_binds_either_way() {
    // 9,000 barrels of oil at $70 against 350 ounces of gold at $2,000 loading 0.9 nets to nothing:
    // every exposure is matched.
    let (mut h, a) = book(0);
    only_fill(&trade(&mut h, &a, OIL, Side::Sell, "1000", "70"));
    let group = &standing(&h, &a, &grid(3_000)).groups["commodities"];
    assert_eq!((group.net_notional, group.binding_direction), (Fixed::ZERO, FactorMove::Either));
    assert_eq!((group.matched_notional, group.worst_loss), (f("1260000"), f("2660")));
}

#[test]
fn a_book_that_offsets_nothing_keeps_its_bands() {
    // Oil alone takes no credit, and the group names it with nothing matched.
    let mut h = genesis(0);
    let b = client(&mut h, 2, "100000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &b, OIL, Side::Buy, "10000", "70"));
    let read = standing(&h, &b, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("7000"), f("7000")));
    let group = &read.groups["commodities"];
    assert_eq!(
        (group.members.as_slice(), group.matched_notional, group.offset),
        (&[OIL][..], Fixed::ZERO, Fixed::ZERO)
    );
    // A book in no group's markets names no group and keeps its requirement.
    h.price(BTC, "100000");
    let c = client(&mut h, 3, "100000");
    only_fill(&trade(&mut h, &c, BTC, Side::Buy, "1", "100000"));
    let read = standing(&h, &c, &grid(3_000));
    assert_eq!((read.margin, read.groups.len()), (f("10000"), 0));
}

#[test]
fn the_tier_floor_raises_each_band_and_the_credit_applies_at_every_tier() {
    // At tier 0's 5% floor each commodity band charges 5%: $80,000 per position. The group's matched
    // bands are $66,500 at that floor, and its credit 70% of them, $46,550, so a fresh wallet's hedge
    // needs $33,450.
    let (h, a) = book(500);
    assert_eq!(h.engine.margin_standing(a.account).unwrap().initial_margin, f("80000"));
    let read = standing(&h, &a, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("20010"), f("33450")));
}

#[test]
fn over_the_weekend_window_the_initial_requirement_takes_the_weekend_shocks() {
    // From two hours before Friday's close oil's bands charge 7.5%: $69,500 per position. Gold, with
    // no weekend rate, still matches $630k of oil but keeps its bands; oil's matched $47,250 of weekend
    // bands are credited less its 4% weekend residual and closing cost, $28,350: $18,900. Maintenance
    // keeps the normal shocks.
    let (mut h, a) = book(0);
    h.advance_to(chicago(2026, 10, 23, 14, 30));
    reprice(&mut h, true);
    assert_eq!(h.engine.margin_standing(a.account).unwrap().initial_margin, f("69500"));
    let read = standing(&h, &a, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("20010"), f("50600")));
}

#[test]
fn markets_with_no_weekend_margin_take_no_credit_over_their_groups_weekend_gap() {
    // Oil and gold carry no weekend margin, so neither keeps a weekend window, yet from two hours
    // before Friday's close until they reopen their matched parts keep their bands: no credit, where
    // Tuesday's was $3,990. Their weekend residuals here are their normal ones, so only that rule
    // removes it.
    let (mut h, a) = book_on(genesis_with(0, 0));
    let mut grid = grid(3_000);
    grid.groups[0].members.iter_mut().for_each(|member| member.weekend_residual_shock_bps = 20);
    assert_eq!(standing(&h, &a, &grid).initial_margin, f("20010"));
    h.advance_to(chicago(2026, 10, 23, 14, 0) - 1);
    reprice(&mut h, true);
    assert_eq!(standing(&h, &a, &grid).initial_margin, f("20010"));
    h.advance(1);
    reprice(&mut h, true);
    assert_eq!(h.engine.margin_standing(a.account).unwrap().initial_margin, f("24000"));
    let read = standing(&h, &a, &grid);
    assert_eq!((read.margin, read.initial_margin), (f("20010"), f("24000")));
}

#[test]
fn a_member_that_cannot_trade_gives_its_group_no_credit_at_initial_bands() {
    // Gold's quotes go stale: at initial bands the group takes no credit, so the requirement is the
    // $24,000 per position. The close-out level's figure reads marks.
    let (mut h, a) = book(0);
    h.advance(2_000);
    reprice(&mut h, false);
    let read = standing(&h, &a, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("20010"), f("24000")));
}

#[test]
fn a_grid_that_does_not_fit_the_listings_is_refused() {
    let (h, a) = book(0);
    let pair = |members: [MarketId; 2]| PortfolioGrid {
        groups: vec![OffsetGroup {
            members: members.map(|market| member(market, "1", 20)).to_vec(),
            ..grid(3_000).groups[0].clone()
        }],
    };
    let refused = h.engine.portfolio_standing(a.account, &pair([BTC, OIL])).unwrap_err();
    assert!(refused.contains("differ in asset class or schedule"), "{refused}");
    let refused = h.engine.portfolio_standing(a.account, &pair([NIKKEI, 99])).unwrap_err();
    assert!(refused.contains("not quoted in USD"), "{refused}");
    // A member not yet listed counts for nothing.
    assert_eq!(standing(&h, &a, &pair([OIL, 99])).margin, h.equity_and_margin(a.account).1);
    assert_eq!(h.engine.portfolio_standing(common::acct(9), &grid(3_000)), Ok(None));
}

#[test]
fn a_lone_position_keeps_its_per_position_requirement_in_every_market_at_every_size_and_tier() {
    let config =
        Config { cases: 96, rng_seed: RngSeed::Fixed(0x10e_0915), failure_persistence: None, ..Config::default() };
    // Every market the harness lists at its own price, sized up to about $5M of notional either way,
    // across its bands, at tier 0 under no margin floor, 2% or 5%, under a random grid over BTC and ETH
    // and over oil and gold.
    let markets =
        [(BTC, 100_000), (ETH, 2_000), (AAPL, 200), (OIL, 70), (GOLD, 2_000), (USDJPY, 150), (NIKKEI, 40_000)];
    let tier_floors = prop::sample::select(vec![0i64, 200, 500]);
    let strategy = (0..markets.len(), 1i64..=5_000_000, any::<bool>(), tier_floors, shocks());
    let result = TestRunner::new(config).run(&strategy, |(index, dollars, long, tier_floor, shocks)| {
        let mut h = genesis(tier_floor);
        let a = client(&mut h, 1, "10000000");
        for (market, price) in markets {
            h.price(market, &price.to_string());
        }
        let (market, price) = markets[index];
        // Japan 225 is quoted in yen at 150 to the dollar, and a unit of USD-JPY is a dollar.
        let dollars_per_unit = match market {
            NIKKEI => price / 150,
            USDJPY => 1,
            _ => price,
        };
        let qty = Fixed::from_int(dollars).div_rounded(Fixed::from_int(dollars_per_unit), Rounding::Down);
        let side = if long { Side::Buy } else { Side::Sell };
        if qty.is_zero() || fills(&trade(&mut h, &a, market, side, &qty.to_string(), &price.to_string())).is_empty() {
            return Ok(());
        }
        let read = standing(&h, &a, &random_grid(&shocks, false));
        let per_position = h.equity_and_margin(a.account).1;
        let initial = h.engine.margin_standing(a.account).unwrap().initial_margin;
        let credited = read.groups.values().any(|group| !group.offset.is_zero());
        if (read.margin, read.initial_margin) == (per_position, initial) && !credited {
            Ok(())
        } else {
            Err(TestCaseError::fail(format!("market {market}, {qty}: {read:?} against {per_position} and {initial}")))
        }
    });
    result.unwrap();
}

#[test]
fn the_requirement_lies_between_the_floor_shares_and_the_per_position_requirement() {
    let config =
        Config { cases: 64, rng_seed: RngSeed::Fixed(0x9f15_0915), failure_persistence: None, ..Config::default() };
    let book = (
        prop::sample::select(vec![0i64, 200, 500]),
        any::<(bool, bool, bool)>(),
        [-20i64..=20, -1_000..=1_000, -30_000..=30_000, -1_000..=1_000, -10_000..=10_000],
        [-200i64..=200, -200..=200, -200..=200, -200..=200, -200..=200],
    );
    let result =
        TestRunner::new(config).run(&(book, shocks()), |((floor, (friday, stale, empty), qtys, moves), shocks)| {
            let mut h = genesis(floor);
            let a = client(&mut h, 1, "10000000");
            let markets = [(BTC, 100_000), (ETH, 2_000), (OIL, 70), (GOLD, 2_000), (AAPL, 200)];
            for (market, price) in markets {
                h.price(market, &price.to_string());
            }
            for ((market, price), qty) in markets.iter().zip(qtys) {
                let side = if qty > 0 { Side::Buy } else { Side::Sell };
                if qty != 0 {
                    trade(&mut h, &a, *market, side, &qty.abs().to_string(), &price.to_string());
                }
            }
            if friday {
                h.advance_to(chicago(2026, 10, 23, 14, 30));
            }
            h.advance(if stale { 2_000 } else { 1 });
            for ((market, price), permille) in markets.iter().zip(moves) {
                if !(stale && *market == GOLD) {
                    h.price(*market, &Fixed::from_int(*price).mul_bps(10_000 + permille * 10).to_string());
                }
            }
            let grid = random_grid(&shocks, empty);
            let read = standing(&h, &a, &grid);
            let per_position = h.equity_and_margin(a.account).1;
            let initial = h.engine.margin_standing(a.account).unwrap().initial_margin;
            let least = grid.groups.iter().map(|group| group.floor_share_bps).min().unwrap_or(10_000);
            let share = |amount: Fixed| amount.mul_bps_rounded(least, Rounding::Up);
            let offsets: Fixed = read.groups.values().map(|group| group.offset).sum();
            let holds =
                |ok: bool, what: &str| if ok { Ok(()) } else { Err(TestCaseError::fail(format!("{what}: {read:?}"))) };
            holds(read.margin == h.engine.portfolio_margin(&h.engine.accounts()[&a.account], &grid), "one read")?;
            holds(read.maintenance_margin == read.margin.mul_bps_rounded(5_000, Rounding::Up), "the close-out level")?;
            holds(read.margin == per_position - offsets, "the offsets add up to the saving")?;
            if empty {
                holds(
                    read.margin == per_position && read.initial_margin == initial,
                    "no group: the per-position requirement",
                )?;
            }
            holds(
                read.margin <= per_position && read.margin >= share(per_position),
                "between the shares and the bands",
            )?;
            holds(
                read.initial_margin <= initial && read.initial_margin >= share(initial),
                "initial: between the shares and the bands",
            )
        });
    result.unwrap();
}

// ── Helpers ──

/// The shocks `random_grid` reads: each group's floor share, each member's residual shock and its
/// loading in tenths, and what the weekend adds to every residual.
type Shocks = ([i64; 2], [i64; 4], [i64; 4], i64);

/// A strategy for `Shocks`.
fn shocks() -> impl Strategy<Value = Shocks> {
    (
        [1i64..=10_000, 1..=10_000],
        [0i64..=500, 0..=500, 0..=500, 0..=500],
        [-100i64..=99, -100..=99, -100..=99, -100..=99],
        0i64..=1_000,
    )
}

/// A fresh log at the latest version on Globex hours, oil carrying its weekend margin, with tier 0's
/// margin floor at `floor_bps`.
fn genesis(floor_bps: i64) -> Harness {
    genesis_with(floor_bps, OIL_WEEKEND_BPS)
}

/// `genesis(floor_bps)` with oil's weekend margin at `oil_weekend_bps`.
fn genesis_with(floor_bps: i64, oil_weekend_bps: i64) -> Harness {
    let mut tiers = launch_volume_tiers();
    tiers[0].margin_floor_bps = floor_bps;
    genesis_with_tiers(ProtocolVersion::LATEST, globex(vec![]), oil_weekend_bps, 0, tiers)
}

/// `genesis(floor_bps)` with client 1 holding $100,000: $700k of oil long, $700k of gold short and
/// $100k of BTC long.
fn book(floor_bps: i64) -> (Harness, Client) {
    book_on(genesis(floor_bps))
}

/// `book`'s positions on `h`.
fn book_on(mut h: Harness) -> (Harness, Client) {
    let a = client(&mut h, 1, "100000");
    reprice(&mut h, true);
    only_fill(&trade(&mut h, &a, OIL, Side::Buy, "10000", "70"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "350", "2000"));
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    (h, a)
}

/// Prices oil at 70, BTC at 100,000 and, with `gold`, gold at 2,000.
fn reprice(h: &mut Harness, gold: bool) {
    h.price(OIL, "70");
    h.price(BTC, "100000");
    if gold {
        h.price(GOLD, "2000");
    }
}

/// Oil and gold as one group under a floor share of `floor_share_bps`, gold loading 0.9: 0.2%
/// residuals, 4% over the weekend window.
fn grid(floor_share_bps: i64) -> PortfolioGrid {
    let weekend = |member: GroupMember| GroupMember { weekend_residual_shock_bps: 400, ..member };
    let group = OffsetGroup {
        name: "commodities".into(),
        floor_share_bps,
        members: vec![weekend(member(OIL, "1", 20)), weekend(member(GOLD, "0.9", 20))],
    };
    PortfolioGrid { groups: vec![group] }
}

/// A member with the same residual shock at all times.
fn member(market: MarketId, loading: &str, residual_shock_bps: i64) -> GroupMember {
    GroupMember { market, loading: f(loading), residual_shock_bps, weekend_residual_shock_bps: residual_shock_bps }
}

/// A grid of BTC and ETH, and oil and gold, from `shocks`: each group's floor share, each member's
/// residual shock, each member's loading in tenths (non-negative ones one tenth up, so none is zero),
/// and what the weekend adds to every residual; no group at all when `empty`.
fn random_grid(shocks: &Shocks, empty: bool) -> PortfolioGrid {
    let (floor_shares, residuals, loadings, weekend) = *shocks;
    let markets = [BTC, ETH, OIL, GOLD];
    let member = |i: usize| GroupMember {
        market: markets[i],
        loading: Fixed::from_int(if loadings[i] < 0 { loadings[i] } else { loadings[i] + 1 }).div_int(10),
        residual_shock_bps: residuals[i],
        weekend_residual_shock_bps: residuals[i] + weekend,
    };
    let group = |name: &str, g: usize| OffsetGroup {
        name: name.into(),
        floor_share_bps: floor_shares[g],
        members: vec![member(2 * g), member(2 * g + 1)],
    };
    let groups = if empty { vec![] } else { vec![group("crypto", 0), group("commodities", 1)] };
    let grid = PortfolioGrid { groups };
    assert_eq!(grid.validate(), Ok(()));
    grid
}

/// `client`'s portfolio standing under `grid`.
fn standing(h: &Harness, client: &Client, grid: &PortfolioGrid) -> PortfolioStanding {
    h.engine.portfolio_standing(client.account, grid).expect("the grid fits the listings").expect("credited")
}
