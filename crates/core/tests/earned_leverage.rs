//! `V64`'s earned leverage, through `Engine::apply`: an account's initial requirement charges each
//! margin band at the larger of its rate and its volume tier's margin floor, so a fresh wallet opens
//! at most 20x, tier 1 at most 50x and the tiers above at the market's own bands; it admits fills that
//! add risk and withdrawals, while close-outs read the market's bands, so a falling tier closes nothing
//! out; a weekend margin and the floor charge the larger of the two band by band, and a weekend gap
//! closes out at the market's bands alone; and the activation gives a log's table its floors. The
//! floors' bounds are in `volume_tier_bounds.rs`.

mod common;

use common::close_spread::{fund, trade};
use common::events::{fills, only_fill, rejection};
use common::weekend::{GOLD, OIL, chicago, client, genesis_with_tiers, globex, withdraw};
use common::{AAPL, BTC, Client, Harness, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::engine::MarginStanding;
use engine_core::event::{Event, Reject};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use engine_core::volume_tier::{ACTIVATION_MARGIN_FLOORS_BPS, DAY_MS, MIN_HOLD_MS, VolumeTier, day_of};
use engine_core::weekend::WEEKEND_LEAD_MS;

const V63: ProtocolVersion = ProtocolVersion::V63;
const V64: ProtocolVersion = ProtocolVersion::V64;

#[test]
fn a_fresh_wallet_opens_at_most_twenty_times_its_equity() {
    // Gold's bands read 1% to $2M. At tier 0's 5% floor a $250k short, $9,950 of equity after its 2bp
    // spread, needs $12,500, and the quote declines it the same way; $198k needs $9,900 of $9,960.40.
    let (mut h, a) = genesis(V64);
    let refusal = rejection(&trade(&mut h, &a, GOLD, Side::Sell, "125", "2000")).clone();
    assert_eq!(refusal, Reject::InsufficientMargin { equity: f("9950"), required: f("12500") });
    assert_eq!(h.engine.quote(a.account, GOLD, Side::Sell, f("125")).unwrap().declined, Some(refusal));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "99", "2000"));
    let read = standing(&h, &a);
    assert_eq!((read.initial_margin, read.withdrawable), (f("9900"), f("60.4")));
    // A close-out reads gold's 1% at the 50% level: $990.
    assert_eq!(read.maintenance_margin, f("990"));
    // Gold and oil take the floor; BTC at 10% and AAPL at 20% are below it.
    let leverage = [GOLD, OIL, BTC, AAPL].map(|market| read.max_leverage[&market]);
    assert_eq!(leverage, [f("20"), f("20"), f("10"), f("5")]);
    // Before `V64` the same $250k opens at gold's 1%, and the read's requirement is the market's.
    let (mut h, a) = genesis(V63);
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "125", "2000"));
    assert_eq!(standing(&h, &a).initial_margin, f("2500"));
}

#[test]
fn each_tier_opens_up_to_its_floor_and_the_top_tiers_at_the_markets_bands() {
    let (mut h, a) = genesis(V64);
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "99", "2000"));
    // Held 60 s, the $198k counts and reaches tier 1, whose 2% floor makes $9,900.40 good for 50x.
    mature(&mut h);
    assert_eq!(h.engine.volume_tier(a.account), 1);
    let read = standing(&h, &a);
    assert_eq!((read.initial_margin, read.max_leverage[&GOLD]), (f("3960"), f("50")));
    assert_eq!(required(&trade(&mut h, &a, GOLD, Side::Sell, "150", "2000")), f("9960"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "145", "2000"));
    // $488k counted reaches tier 2, which has no floor: gold's own 1%, so 100x.
    mature(&mut h);
    assert_eq!(h.engine.volume_tier(a.account), 2);
    let read = standing(&h, &a);
    assert_eq!((read.initial_margin, read.max_leverage[&GOLD]), (f("4880"), f("100")));
    assert_eq!(required(&trade(&mut h, &a, GOLD, Side::Sell, "250", "2000")), f("9880"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "245", "2000"));
    assert_eq!(standing(&h, &a).initial_margin, f("9780"));
}

#[test]
fn a_falling_tier_closes_nothing_out_and_stops_withdrawals_at_the_initial_requirement() {
    let (mut h, a) = genesis(V64);
    short_at_tier_two(&mut h, &a);
    // Thirty days on the counted volume has left the window. Tier 0's 5% floor puts the initial
    // requirement on the $978k short at $48,900, far above its equity, and nothing is closed out.
    h.advance_to((day_of(h.ts) + 30) * DAY_MS);
    assert!(fills(&h.cmd(Command::Tick)).is_empty());
    assert!(fills(&h.price(GOLD, "2000")).is_empty());
    assert_eq!(h.engine.volume_tier(a.account), 0);
    assert_eq!((standing(&h, &a).initial_margin, standing(&h, &a).withdrawable), (f("48900"), Fixed::ZERO));
    // It adds no risk, and may still reduce.
    assert_eq!(required(&trade(&mut h, &a, GOLD, Side::Sell, "1", "2000")), f("49000"));
    only_fill(&trade(&mut h, &a, GOLD, Side::Buy, "1", "2000"));
    // With $45,000 more, it withdraws what its equity holds above the requirement, and no more.
    fund(&mut h, &a, "45000");
    let free = standing(&h, &a).withdrawable;
    assert!(free > f("5000") && free < f("10000"), "{free}");
    let whole = free.raw() / Fixed::ONE.raw();
    assert_eq!(refused(&withdraw(&mut h, &a, &(whole + 1).to_string())), Reject::InsufficientFunds { available: free });
    assert!(queued(&withdraw(&mut h, &a, &whole.to_string())));
    // Gold rising to $2,050 takes the equity far below the floor but not below the close-out level of
    // gold's 1%; at $2,100 the short is closed out.
    h.advance(1_000);
    assert!(fills(&h.price(GOLD, "2050")).is_empty());
    let (equity, margin) = h.equity_and_margin(a.account);
    assert!(equity < standing(&h, &a).initial_margin && equity > margin.mul_bps(5_000), "{equity} {margin}");
    h.advance(1_000);
    let closed = h.price(GOLD, "2100");
    assert!(!fills(&closed).is_empty() && fills(&closed).iter().all(|fill| fill.liquidation), "{closed:?}");
    assert!(closed.iter().any(|event| matches!(event, Event::CloseOut { account, .. } if *account == a.account)));
}

#[test]
fn a_weekend_margin_and_the_floor_charge_the_larger_band_by_band() {
    // Oil's weekend margin is 7.5% and gold's 1.5%, over 1% bands.
    let mut h = genesis_with_tiers(V64, globex(vec![]), 750, 150, tiers(V64));
    let (a, b) = (client(&mut h, 1, "10000"), client(&mut h, 2, "10000"));
    h.advance_to(chicago(2026, 10, 23, 16, 0) - WEEKEND_LEAD_MS);
    h.price(GOLD, "2000");
    h.price(OIL, "70");
    // At tier 0 a $190k gold short needs the 5% floor, $9,500 of $9,962: gold's 1.5% adds nothing.
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "95", "2000"));
    // A $140k oil long meets the floor's $7,000 but needs oil's 7.5%, $10,500, not both; $130k fills.
    let refusal = rejection(&trade(&mut h, &b, OIL, Side::Buy, "2000", "70")).clone();
    assert!(matches!(refusal, Reject::WeekendMargin { required, .. } if required == f("10500")), "{refusal:?}");
    only_fill(&trade(&mut h, &b, OIL, Side::Buy, "1857", "70"));
    assert_eq!(standing(&h, &b).max_leverage[&OIL], f("13.333333333"));
    // Withdrawals and the read hold the same larger band: b's initial requirement is oil's 7.5% of its
    // $129,990, and a's the floor's 5% of $190k, not either plus the other's excess.
    let (read_a, read_b) = (standing(&h, &a), standing(&h, &b));
    assert_eq!((read_a.initial_margin, read_b.initial_margin), (f("9500"), f("9749.25")));
    for (client, read) in [(&a, read_a), (&b, read_b)] {
        let free = h.equity_and_margin(client.account).0 - read.initial_margin;
        assert_eq!(read.withdrawable, free);
        let whole = free.raw() / Fixed::ONE.raw();
        let more = (whole + 1).to_string();
        assert_eq!(refused(&withdraw(&mut h, client, &more)), Reject::InsufficientFunds { available: free });
        assert!(queued(&withdraw(&mut h, client, &whole.to_string())), "{whole}");
    }
}

#[test]
fn a_weekend_gap_closes_out_at_the_markets_bands_whatever_the_floor() {
    // Gold's weekend margin is 1.5%, over its 1% bands.
    let mut h = genesis_with_tiers(V64, globex(vec![]), 0, 150, tiers(V64));
    let (c, d) = (client(&mut h, 3, "10000"), client(&mut h, 4, "10000"));
    let close = chicago(2026, 10, 23, 16, 0);
    // d opens a $190k gold short two hours before the close, reaches tier 1 a minute later and
    // withdraws down to its 2% floor; c opens the same short 30 s before the close, still at tier 0
    // when the gap starts.
    h.advance_to(close - WEEKEND_LEAD_MS);
    h.price(GOLD, "2000");
    only_fill(&trade(&mut h, &d, GOLD, Side::Sell, "95", "2000"));
    mature(&mut h);
    assert_eq!(h.engine.volume_tier(d.account), 1);
    let whole = standing(&h, &d).withdrawable.raw() / Fixed::ONE.raw();
    assert!(queued(&withdraw(&mut h, &d, &whole.to_string())), "{whole}");
    h.advance_to(close - 30_000);
    h.price(GOLD, "2000");
    only_fill(&trade(&mut h, &c, GOLD, Side::Sell, "95", "2000"));
    // At $2,010 the gap needs gold's 1.5% weekend margin at the market's bands, as the read says. c's
    // equity is below the floor's 5% but above that; d's is below it but above gold's own 1%.
    h.advance_to(close - 10_000);
    h.price(GOLD, "2010");
    let needs = f("2864.25");
    let equity = |h: &Harness, client: &Client| h.equity_and_margin(client.account).0;
    for client in [&c, &d] {
        assert_eq!(h.engine.weekend_requirements(client.account)[&GOLD].required, needs);
    }
    assert!(equity(&h, &c) < standing(&h, &c).initial_margin && equity(&h, &c) > needs);
    assert!(equity(&h, &d) < needs && equity(&h, &d) > f("1909.5"), "{}", equity(&h, &d));
    // The gap's start closes d out and not c.
    h.advance_to(close);
    let events = h.cmd(Command::Tick);
    assert_eq!(h.engine.volume_tier(c.account), 0);
    let closed: Vec<_> = fills(&events).iter().filter(|fill| fill.liquidation).map(|fill| fill.account).collect();
    assert_eq!(closed, [d.account], "{events:?}");
}

#[test]
fn activation_gives_the_table_its_floors_and_closes_no_held_position_out() {
    // A 49x gold short opened at `V63`'s 1%.
    let (mut h, a) = genesis(V63);
    only_fill(&trade(&mut h, &a, GOLD, Side::Sell, "245", "2000"));
    h.admin(AdminCommand::ActivateProtocol { version: V64 });
    let floors = |tiers: &[VolumeTier]| tiers.iter().map(|tier| tier.margin_floor_bps).collect::<Vec<_>>();
    let expected = [ACTIVATION_MARGIN_FLOORS_BPS[0], ACTIVATION_MARGIN_FLOORS_BPS[1], 0, 0];
    assert_eq!(floors(&h.engine.params().unwrap().volume_tiers), expected);
    assert_eq!(floors(h.engine.operator_bounds().launch_volume_tiers.as_deref().unwrap()), expected);
    // It stays open at tier 0's 5%, a $24,500 requirement it no longer meets, so it neither adds risk
    // nor withdraws, and may reduce.
    h.advance(1_000);
    assert!(fills(&h.price(GOLD, "2000")).is_empty());
    assert_eq!((standing(&h, &a).initial_margin, standing(&h, &a).withdrawable), (f("24500"), Fixed::ZERO));
    assert!(matches!(rejection(&trade(&mut h, &a, GOLD, Side::Sell, "1", "2000")), Reject::InsufficientMargin { .. }));
    assert_eq!(refused(&withdraw(&mut h, &a, "10")), Reject::InsufficientFunds { available: Fixed::ZERO });
    only_fill(&trade(&mut h, &a, GOLD, Side::Buy, "100", "2000"));
}

// ── Helpers ──

/// A table whose tiers a few fills reach, each in a day: $100k counted reaches tier 1 and $400k tier
/// 2, with the launch shares and, from `V64`, the activation floors.
fn tiers(version: ProtocolVersion) -> Vec<VolumeTier> {
    [(0, 2_500), (100_000, 5_000), (400_000, 10_000), (10_000_000, 25_000)]
        .into_iter()
        .enumerate()
        .map(|(at, (usd, best_price_share_bps))| VolumeTier {
            threshold: Fixed::from_int(usd),
            day_divisor: 1,
            best_price_share_bps,
            margin_floor_bps: if version >= V64 {
                ACTIVATION_MARGIN_FLOORS_BPS.get(at).copied().unwrap_or(0)
            } else {
                0
            },
        })
        .collect()
}

/// A log that activates `version` before its genesis with `tiers`, client 1 holding $10,000, and gold
/// at $2,000.
fn genesis(version: ProtocolVersion) -> (Harness, Client) {
    let mut h = genesis_with_tiers(version, globex(vec![]), 0, 0, tiers(version));
    let a = client(&mut h, 1, "10000");
    h.price(GOLD, "2000");
    (h, a)
}

/// `a` short 489 oz of gold at $2,000, 100x its equity, opened through tiers 0, 1 and 2.
fn short_at_tier_two(h: &mut Harness, a: &Client) {
    for qty in ["99", "145", "245"] {
        only_fill(&trade(h, a, GOLD, Side::Sell, qty, "2000"));
        mature(h);
    }
    assert_eq!(h.engine.volume_tier(a.account), 2);
}

/// Lets the opening notional count, with gold at $2,000.
fn mature(h: &mut Harness) {
    h.advance(MIN_HOLD_MS);
    h.price(GOLD, "2000");
}

fn standing(h: &Harness, client: &Client) -> MarginStanding {
    h.engine.margin_standing(client.account).expect("credited")
}

/// The initial requirement an order was refused below.
fn required(events: &[Event]) -> Fixed {
    match rejection(events) {
        Reject::InsufficientMargin { required, .. } => *required,
        other => panic!("expected a margin refusal, got {other:?}"),
    }
}

fn refused(events: &[Event]) -> Reject {
    match events.last() {
        Some(Event::WithdrawalRejected { reason, .. }) => reason.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn queued(events: &[Event]) -> bool {
    events.iter().any(|event| matches!(event, Event::WithdrawalQueued { .. }))
}
