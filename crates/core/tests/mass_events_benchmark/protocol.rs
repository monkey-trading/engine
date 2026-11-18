//! A protocol activation on a state at the version before it, and V74's re-key of a market's resting
//! book after its least band rate changes.

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::MarketId;

use crate::common::BTC;
use crate::common::launch_book::{Book, Mix, NamedCounts, build, price_command};
use crate::report::timed;
use crate::support::{accounts, finish, report, report_accepted, roll_to};

const NAMED: NamedCounts = NamedCounts { adders: 1, openers: 0, closeouts: 0 };
/// Tier 0's resting-order limit, and every tier's (#614 D11), which the one-market books fill.
const LIMITS: [usize; 4] = [25, 50, 100, 250];
/// Resting orders in the one-market books: tier 0's limit at 100k accounts.
const ORDERS: usize = 2_500_000;

/// `ActivateProtocol` to the latest version on a book at the version before it, which rebuilds the
/// transient indexes as every activation does, and the rebuild alone. The owner accepted this case
/// over the bar (#904): an activation is a one-off, run by the operator when the node has no price waiting.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_activation() {
    // The version before the latest: move this with `LATEST`, so the case stays one activation.
    let before = ProtocolVersion::V77;
    assert_eq!(ProtocolVersion::LATEST, ProtocolVersion::V78, "move `before` to the version one back");
    let mix = Mix { version: before, ..Mix::proposal(accounts()) };
    let mut book = build(&mix, NAMED);
    assert_eq!(book.h.engine.protocol_version(), before);
    let (events, us) =
        timed(|| book.apply(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST })));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert_eq!(book.h.engine.protocol_version(), ProtocolVersion::LATEST);
    report_accepted("mass_activation_to_latest", us, &events, &format!("accounts={}", accounts()));
    // Recovery's rebuild of the transient indexes: a decoded copy takes them on its first command.
    let text = serde_json::to_string(&book.h.engine).expect("encoding");
    let (mut copy, decode) = timed(|| serde_json::from_str::<Engine>(&text).expect("decoding"));
    drop(text);
    let ((), restore) = timed(|| copy.restore_indexes());
    let seq = copy.last_seq().map_or(0, |seq| seq + 1);
    let ts = copy.now() + 1;
    let (events, first) = timed(|| copy.apply(&Sequenced { seq, ts, cmd: Command::Tick }).expect("a tick applies"));
    println!("MASS recovery decode_us={decode} restore_indexes_us={restore}");
    report_accepted("mass_recovery_first_command", first, &events, "");
}

/// The least share of the mid a fill in `market` pays, in tenths of a bp: its least band, or its
/// close-spread ceiling if that is lower, which is the rate V74 keys its resting limits at.
fn least_rate(book: &Book, market: MarketId) -> i64 {
    let least =
        book.h.engine.markets()[&market].spec.spread_tiers.iter().map(SpreadTier::tenth_bps).min().expect("bands");
    let ceiling = book.h.engine.close_spread_ceilings().and_then(|ceilings| ceilings.get(&market));
    ceiling.map_or(least, |bps| least.min(bps * 10))
}

/// Doubles every band of `market`, the most a notice may raise them, from seven days on, walks the clock
/// there and times the next price, which re-keys the market's resting book, then the one after.
fn rekey(book: &mut Book, market: MarketId, case: &str) {
    let resting = book.h.engine.markets()[&market].resting.len();
    let rate_before = least_rate(book, market);
    let held = |book: &Book| book.h.engine.markets()[&market].resting.len();
    let bands: Vec<SpreadTier> = book.h.engine.markets()[&market]
        .spec
        .spread_tiers
        .iter()
        .map(|band| SpreadTier::tenths(band.up_to, band.tenth_bps() * 2))
        .collect();
    let effective_at = book.h.ts + NOTICE_MS;
    let notice = ListingNotice {
        spread_tiers: Some(bands),
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: None,
        price_rules: None,
        effective_at,
    };
    let events = book.operate(AdminCommand::AnnounceListing { market, notice: notice.clone() });
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    roll_to(book, effective_at);
    // The relisting spends the notice: the new bands apply from this command, and the market's keys
    // follow at its next price.
    let spec = MarketSpec {
        spread_tiers: notice.spread_tiers.expect("bands"),
        ..book.h.engine.markets()[&market].spec.clone()
    };
    let (events, us) = timed(|| book.operate(AdminCommand::UpsertMarket(spec)));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert_eq!(held(book), resting, "the relisting leaves the resting book as it was");
    assert_ne!(least_rate(book, market), rate_before, "the relisting moves the rate the re-key reads");
    let first_band = book.h.engine.markets()[&market].spec.spread_tiers[0].tenth_bps();
    let extra = format!("market={market} resting_orders={resting} first_band_tenth_bps={first_band}");
    report(&format!("mass_relist_{case}"), us, &events, &extra);
    for (first, name, mid) in [(true, "first_price", "100.0001"), (false, "second_price", "100")] {
        book.h.advance(1);
        let cmd = price_command(&book.h, market, mid, book.h.ts);
        let (events, us) = timed(|| book.apply(cmd));
        let fills = events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
        assert_eq!(fills, 0, "no order is reached: {fills}");
        assert_eq!(held(book), resting, "the price leaves the resting book as it was");
        let extra = format!("market={market} resting_orders={resting} first_band_tenth_bps={first_band}");
        // The re-key on the first price after a relisting is a notice-gated exception to the bar (#904),
        // reported only; the second price is held to the guard.
        let name = format!("mass_rekey_{case}_{name}");
        if first {
            report_accepted(&name, us, &events, &extra);
        } else {
            report(&name, us, &events, &extra);
        }
    }
}

/// The busiest market of the launch book, re-keyed.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn mass_rekey_launch_book() {
    let mut book = build(&Mix::proposal(accounts()), NAMED);
    let busiest = *book.h.engine.markets().iter().max_by_key(|(_, market)| market.resting.len()).expect("markets").0;
    rekey(&mut book, busiest, "launch_book");
    finish();
}

/// A market that holds `ORDERS` resting orders, every account at one tier's limit, re-keyed. The count of
/// orders is tier 0's limit at 100k accounts; a higher limit has fewer accounts, so what the cost shows is
/// the orders, and the cost of each tier's limit at 100k accounts is its limit over 25 times this.
#[test]
#[ignore = "2.5M-order benchmark: run in an isolated CPU window"]
fn mass_rekey_at_each_tier_limit() {
    let orders = std::env::var("MASS_ORDERS").ok().map_or(ORDERS, |n| n.parse().expect("a count"));
    for (tier, limit) in LIMITS.into_iter().enumerate() {
        let accounts = orders.div_ceil(limit);
        let mut tiers = vec![0; 4];
        tiers[tier] = 100;
        let mix = Mix { only: Some(BTC), tiers, resting: 100, in_band: 0, ..Mix::proposal(accounts + NAMED.adders) };
        let mut book = build(&mix, NAMED);
        let tiers = &book.h.engine.params().expect("params").volume_tiers;
        assert_eq!(tiers[tier].max_resting_orders, limit, "tier {tier}'s limit is the genesis's");
        // Each filler account rests its tier's limit, and the template account its one order.
        assert_eq!(book.orders, accounts * limit + 1, "the book holds each account's limit");
        rekey(&mut book, BTC, &format!("tier_{tier}_limit_{limit}"));
    }
    finish();
}
