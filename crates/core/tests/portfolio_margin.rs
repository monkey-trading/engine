//! Portfolio margin as a read (`portfolio`), through `Engine::apply`: a hedged book needs its groups'
//! worst losses and the cost of closing it, never more than its bands and never less than the floor
//! share of them; a book that offsets nothing keeps its bands; the initial requirement takes the tier
//! floor on gross notional, a group's weekend shocks over its window, and no offset for a group with a
//! member that cannot trade; with no group the requirement is the per-position one; and a grid that
//! does not fit the listings is refused.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::weekend::{GOLD, NIKKEI, OIL, OIL_WEEKEND_BPS, chicago, client, genesis_with_tiers, globex};
use common::{AAPL, BTC, Client, ETH, Harness, f};
use engine_core::Fixed;
use engine_core::fixed::Rounding;
use engine_core::portfolio::{FactorMove, GroupMember, GroupStanding, OffsetGroup, PortfolioGrid, PortfolioStanding};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use engine_core::volume_tier::launch_volume_tiers;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

#[test]
fn a_hedged_pair_needs_its_worst_loss_and_closing_cost_not_its_bands() {
    // $700k of oil long and $700k of gold short, at 1% each, and $100k of BTC alone at 10%: $24,000
    // per position. The group's net is $70k long (gold loads 0.9), so its worst loss is 3% of that and
    // 0.2% of each leg, $4,900; closing both legs at gold's and oil's 25bp ceilings costs $3,500,
    // $7,000 at the 50% close-out level; BTC keeps its $10,000 and adds $400 of closing cost.
    let (h, a) = book(0);
    let grid = grid(3_000);
    assert_eq!(h.equity_and_margin(a.account).1, f("24000"));
    let group = GroupStanding {
        members: vec![OIL, GOLD],
        net_notional: f("70000"),
        worst_loss: f("4900"),
        binding_direction: FactorMove::Down,
        offset: f("2100"),
    };
    let expected = PortfolioStanding {
        margin: f("22300"),
        initial_margin: f("22300"),
        maintenance_margin: f("11150"),
        groups: [("commodities".to_owned(), group)].into(),
    };
    assert_eq!(h.engine.portfolio_standing(a.account, &grid), Ok(Some(expected)));
    assert_eq!(h.engine.portfolio_margin(&h.engine.accounts()[&a.account], &grid), f("22300"));
    // A floor share of 95% holds it at $22,800.
    assert_eq!(standing(&h, &a, &self::grid(9_500)).margin, f("22800"));
}

#[test]
fn a_net_short_binds_on_the_factor_rising() {
    let (mut h, a) = book(0);
    // Selling 70 more ounces of gold takes the net to $56k short: 3% of it and 0.2% of each leg.
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "70", "2000"));
    let group = &standing(&h, &a, &grid(3_000)).groups["commodities"];
    assert_eq!((group.net_notional, group.binding_direction), (f("-56000"), FactorMove::Up));
    assert_eq!(group.worst_loss, f("1680") + f("3080"));
}

#[test]
fn a_book_that_offsets_nothing_keeps_its_bands() {
    // Oil alone: 3.2% of $700k and its closing cost are far above its 1% band.
    let mut h = genesis(0);
    let b = client(&mut h, 2, "100000");
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &b, OIL, Side::Buy, "10000", "70"));
    let read = standing(&h, &b, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("7000"), f("7000")));
    let group = &read.groups["commodities"];
    assert_eq!((group.members.as_slice(), group.worst_loss, group.offset), (&[OIL][..], f("22400"), f("-18900")));
    // A book in no group's markets names no group and keeps its requirement.
    h.price(BTC, "100000");
    let c = client(&mut h, 3, "100000");
    only_fill(&trade(&mut h, &c, BTC, Side::Buy, "1", "100000"));
    let read = standing(&h, &c, &grid(3_000));
    assert_eq!((read.margin, read.groups.len()), (f("10000"), 0));
}

#[test]
fn the_initial_requirement_takes_the_tier_floor_on_gross_notional() {
    // At tier 0's 5% floor each commodity band charges 5%: $80,000 per position. The portfolio model
    // is still $22,300 and the floor share $24,000, but 5% of the $1.5M gross is $75,000.
    let (h, a) = book(500);
    assert_eq!(h.engine.margin_standing(a.account).unwrap().initial_margin, f("80000"));
    let read = standing(&h, &a, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("22300"), f("75000")));
}

#[test]
fn over_the_weekend_window_the_initial_requirement_takes_the_weekend_shocks() {
    // From two hours before Friday's close oil's bands charge 7.5%: $69,500 per position. The group
    // takes its weekend shocks, 6% of the $70k net and 1% of each leg, $18,200; with the same closing
    // costs and BTC, $35,600. Maintenance keeps the normal shocks.
    let (mut h, a) = book(0);
    h.advance_to(chicago(2026, 10, 23, 14, 30));
    reprice(&mut h, true);
    assert_eq!(h.engine.margin_standing(a.account).unwrap().initial_margin, f("69500"));
    let read = standing(&h, &a, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("22300"), f("35600")));
}

#[test]
fn a_member_that_cannot_trade_gives_its_group_no_offset_at_initial_bands() {
    // Gold's quotes go stale: at initial bands the group counts at its $14,000 of bands and closing
    // costs, past the $24,000 per position, which binds. The close-out level's figure reads marks.
    let (mut h, a) = book(0);
    h.advance(2_000);
    reprice(&mut h, false);
    let read = standing(&h, &a, &grid(3_000));
    assert_eq!((read.margin, read.initial_margin), (f("22300"), f("24000")));
}

#[test]
fn a_grid_that_does_not_fit_the_listings_is_refused() {
    let (h, a) = book(0);
    let pair = |members: [MarketId; 2]| PortfolioGrid {
        groups: vec![OffsetGroup {
            members: members.map(|market| member(market, "1", 20)).to_vec(),
            ..grid(3_000).groups[0].clone()
        }],
        ..grid(3_000)
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
fn the_requirement_lies_between_its_floor_share_and_the_per_position_requirement() {
    let config =
        Config { cases: 64, rng_seed: RngSeed::Fixed(0x9f15_0915), failure_persistence: None, ..Config::default() };
    let book = (
        prop::sample::select(vec![0i64, 200, 500]),
        any::<(bool, bool, bool)>(),
        [-20i64..=20, -1_000..=1_000, -30_000..=30_000, -1_000..=1_000, -10_000..=10_000],
        [-200i64..=200, -200..=200, -200..=200, -200..=200, -200..=200],
    );
    let shocks = (
        1i64..=10_000,
        [1i64..=2_000, 1..=2_000],
        [0i64..=500, 0..=500, 0..=500, 0..=500],
        [-100i64..=99, -100..=99, -100..=99, -100..=99],
        0i64..=1_000,
    );
    let result =
        TestRunner::new(config).run(&(book, shocks), |((floor, (friday, stale, empty), qtys, moves), shocks)| {
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
            let share = |amount: Fixed| amount.mul_bps_rounded(grid.floor_share_bps, Rounding::Up);
            let holds =
                |ok: bool, what: &str| if ok { Ok(()) } else { Err(TestCaseError::fail(format!("{what}: {read:?}"))) };
            holds(read.margin == h.engine.portfolio_margin(&h.engine.accounts()[&a.account], &grid), "one read")?;
            holds(read.maintenance_margin == read.margin.mul_bps_rounded(5_000, Rounding::Up), "the close-out level")?;
            if empty {
                holds(
                    read.margin == per_position && read.initial_margin == initial,
                    "no group: the per-position requirement",
                )?;
            }
            holds(
                read.margin <= per_position && read.margin >= share(per_position),
                "between the share and the bands",
            )?;
            holds(
                read.initial_margin <= initial && read.initial_margin >= share(initial),
                "initial: between the share and the bands",
            )
        });
    result.unwrap();
}

// ── Helpers ──

/// A fresh log at the latest version on Globex hours, oil carrying its weekend margin, with tier 0's
/// margin floor at `floor_bps`.
fn genesis(floor_bps: i64) -> Harness {
    let mut tiers = launch_volume_tiers();
    tiers[0].margin_floor_bps = floor_bps;
    genesis_with_tiers(ProtocolVersion::LATEST, globex(vec![]), OIL_WEEKEND_BPS, 0, tiers)
}

/// `genesis(floor_bps)` with client 1 holding $100,000: $700k of oil long, $700k of gold short and
/// $100k of BTC long.
fn book(floor_bps: i64) -> (Harness, Client) {
    let mut h = genesis(floor_bps);
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

/// Oil and gold as one group, gold loading 0.9: a 3% factor shock and 0.2% residuals, 6% and 1% over
/// the weekend window, under a floor share of `floor_share_bps`.
fn grid(floor_share_bps: i64) -> PortfolioGrid {
    let weekend = |member: GroupMember| GroupMember { weekend_residual_shock_bps: 100, ..member };
    let group = OffsetGroup {
        name: "commodities".into(),
        factor_shock_bps: 300,
        weekend_factor_shock_bps: 600,
        members: vec![weekend(member(OIL, "1", 20)), weekend(member(GOLD, "0.9", 20))],
    };
    PortfolioGrid { floor_share_bps, groups: vec![group] }
}

/// A member with the same residual shock at all times.
fn member(market: MarketId, loading: &str, residual_shock_bps: i64) -> GroupMember {
    GroupMember { market, loading: f(loading), residual_shock_bps, weekend_residual_shock_bps: residual_shock_bps }
}

/// A grid of BTC and ETH, and oil and gold, from `shocks`: the floor share, each group's factor shock,
/// each member's residual shock, each member's loading in tenths (non-negative ones one tenth up, so
/// none is zero), and what the weekend adds to every shock; no group at all when `empty`.
fn random_grid(shocks: &(i64, [i64; 2], [i64; 4], [i64; 4], i64), empty: bool) -> PortfolioGrid {
    let (floor_share_bps, factors, residuals, loadings, weekend) = *shocks;
    let markets = [BTC, ETH, OIL, GOLD];
    let member = |i: usize| GroupMember {
        market: markets[i],
        loading: Fixed::from_int(if loadings[i] < 0 { loadings[i] } else { loadings[i] + 1 }).div_int(10),
        residual_shock_bps: residuals[i],
        weekend_residual_shock_bps: residuals[i] + weekend,
    };
    let group = |name: &str, g: usize| OffsetGroup {
        name: name.into(),
        factor_shock_bps: factors[g],
        weekend_factor_shock_bps: (factors[g] + weekend).min(10_000),
        members: vec![member(2 * g), member(2 * g + 1)],
    };
    let groups = if empty { vec![] } else { vec![group("crypto", 0), group("commodities", 1)] };
    let grid = PortfolioGrid { floor_share_bps, groups };
    assert_eq!(grid.validate(), Ok(()));
    grid
}

/// `client`'s portfolio standing under `grid`.
fn standing(h: &Harness, client: &Client, grid: &PortfolioGrid) -> PortfolioStanding {
    h.engine.portfolio_standing(client.account, grid).expect("the grid fits the listings").expect("credited")
}
