//! The level index against pricing every level: the same random sessions, run once with the index and
//! once pricing every level in a market on each of its prices, emit the same events and reach the same
//! roots, and every key the index takes for current is the key its inputs give. The sessions hold longs
//! and shorts of many sizes in a USD market, in one converting at its own index and in one converting at
//! another's, and move prices, FX indices, stress, time and levels.

use crate::Fixed;
use crate::access::AccessPolicy;
use crate::auth::ClientAuth;
use crate::close_spread::CloseSpreadCeiling;
use crate::command::{AdminCommand, Command};
use crate::common::close_spread::{attest, ceilings, fund, source_key, trade};
use crate::common::levels::{levels, set};
use crate::common::{AAPL, BTC, CHAIN_ID, Client, DE40, EURUSD, Harness, USDJPY, VAULT, ny};
use crate::corporate::{Adjustment, CorporateAction};
use crate::event::Event;
use crate::listing_bounds::ListingNotice;
use crate::market::MarketSpec;
use crate::protocol::ProtocolVersion;
use crate::spread_tier::SpreadTier;
use crate::types::{MarketId, Side};
use crate::volume_tier::DAY_MS;

/// The sessions each test runs, and the steps in each.
const SESSIONS: u64 = 24;
const STEPS: usize = 160;
const HOUR_MS: i64 = 60 * 60 * 1000;

/// A deterministic stream of numbers for a session.
struct Draws(u64);

impl Draws {
    /// The next number below `n`.
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }

    /// `mid` moved by up to `bps` either way, in tenths of a bp.
    fn moved(&mut self, mid: Fixed, bps: u64) -> Fixed {
        let tenths = self.below(bps * 20 + 1) as i64 - (bps * 10) as i64;
        mid + mid.mul_tenth_bps_rounded(tenths, crate::fixed::Rounding::Down)
    }
}

#[test]
fn the_index_fires_exactly_the_levels_pricing_them_all_fires() {
    for seed in 0..SESSIONS {
        let cached = session(seed, false);
        let uncached = session(seed, true);
        for (step, (cached, uncached)) in cached.iter().zip(&uncached).enumerate() {
            assert_eq!(cached, uncached, "session {seed}, step {step}");
        }
        let fired =
            cached.iter().flat_map(|(events, _)| events).filter(|e| matches!(e, Event::PositionLevelTriggered { .. }));
        assert!(fired.count() > 0, "session {seed} fired no level");
    }
}

/// The events and root of every step of session `seed`, pricing every level on each price when
/// `uncached`, and checking the index's keys after each step when not.
fn session(seed: u64, uncached: bool) -> Vec<(Vec<Event>, [u8; 32])> {
    let mut draws = Draws(seed);
    let (mut h, clients) = setup();
    super::trigger::UNCACHED_LEVELS.set(uncached);
    let mut mids = [(BTC, f("100000")), (EURUSD, f("1.1")), (USDJPY, f("150")), (DE40, f("20000"))];
    let mut trace = Vec::new();
    for client in &clients {
        let market = [BTC, USDJPY, DE40][draws.below(3) as usize];
        let side = if draws.below(2) == 0 { Side::Buy } else { Side::Sell };
        let qty = match market {
            BTC => ["0.01", "0.5", "3", "40"][draws.below(4) as usize],
            USDJPY => ["1000", "200000", "4000000"][draws.below(3) as usize],
            _ => ["1", "20", "300"][draws.below(3) as usize],
        };
        let mid = mids.iter().find(|(id, _)| *id == market).expect("priced").1;
        trace.push((trade(&mut h, client, market, side, qty, &mid.to_string()), h.engine.state_root()));
        trace.push((set_near(&mut h, client, market, &mut draws), h.engine.state_root()));
    }
    for _ in 0..STEPS {
        let events = match draws.below(100) {
            0..25 => {
                let jump = if draws.below(10) == 0 { 300 } else { 40 };
                reprice(&mut h, &mut mids, 0, jump, &mut draws)
            }
            25..50 => near_a_level(&mut h, &mut mids, &mut draws),
            50..62 => reprice(&mut h, &mut mids, 1, 30, &mut draws),
            62..72 => reprice(&mut h, &mut mids, 2, 40, &mut draws),
            72..82 => reprice(&mut h, &mut mids, 3, 60, &mut draws),
            82..90 => h.operate(AdminCommand::SetStressFloor { market: Some(BTC), level: draws.below(2) as usize }),
            90..94 => {
                h.advance(HOUR_MS);
                h.cmd(Command::Tick)
            }
            94..95 => {
                h.advance(24 * HOUR_MS);
                h.cmd(Command::Tick)
            }
            _ => {
                let client = &clients[draws.below(clients.len() as u64) as usize];
                let held = h.engine.accounts()[&client.account].positions.keys().next().copied();
                match held {
                    Some(market) => set_near(&mut h, client, market, &mut draws),
                    None => Vec::new(),
                }
            }
        };
        if !uncached {
            h.engine.assert_level_keys_current();
        }
        trace.push((events, h.engine.state_root()));
    }
    trace
}

#[test]
fn doubled_bands_and_a_lowered_ceiling_each_re_key_the_levels_they_bring_into_reach() {
    let (mut h, clients) = setup();
    let (stopped, taken) = (&clients[0], &clients[1]);
    // Two 120 BTC longs, one with a stop loss 21bp below its closing price.
    trade(&mut h, stopped, BTC, Side::Buy, "120", "100000");
    trade(&mut h, taken, BTC, Side::Buy, "120", "100000");
    let before = closing(&h, stopped);
    set(&mut h, stopped, BTC, levels(None, Some(&below(before, 210).to_string())));
    // BTC's bands double on a notice, and a day later its ceiling is lowered from 20bp.
    let spec = h.engine.markets()[&BTC].spec.clone();
    let doubled: Vec<SpreadTier> =
        spec.spread_tiers.iter().map(|tier| SpreadTier::tenths(tier.up_to, tier.tenth_bps() * 2)).collect();
    let doubled_at = h.ts + 7 * DAY_MS;
    let notice = ListingNotice {
        spread_tiers: Some(doubled.clone()),
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: None,
        price_rules: None,
        min_level_distance_bps: None,
        effective_at: doubled_at,
    };
    assert!(h.operate(AdminCommand::AnnounceListing { market: BTC, notice }).contains(&Event::AdminApplied));
    let lowered_at = doubled_at + DAY_MS;
    let ceiling = AdminCommand::AdoptCloseSpreadCeiling {
        market: BTC,
        max_close_spread_bps: CEILING_BPS,
        effective_at: lowered_at,
        listing: None,
    };
    assert!(h.operate(ceiling).contains(&Event::AdminApplied));
    h.advance_to(doubled_at);
    let events = h.operate(AdminCommand::UpsertMarket(MarketSpec { spread_tiers: doubled, ..spec }));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    // The doubled bands widen the close: the stop loss fires at a mid between where the old and the
    // new bands bring the closing price to it.
    let mid = f("100000");
    calm_price(&mut h, mid);
    let stop = h.engine.accounts()[&stopped.account].levels[&BTC].stop_loss.expect("set");
    let (old, new) = (mid - before, mid - closing(&h, stopped));
    assert!(new > old, "the doubled bands widen the close");
    let events = calm_price(&mut h, stop + (old + new).div_int(2));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::PositionLevelTriggered { account, .. } if *account == stopped.account))
    );
    // At stress level 1 the doubled bands charge the other long more than 13bp; its take profit is set
    // 21bp above its closing price there.
    h.operate(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    let mid = f("100000");
    calm_price(&mut h, mid);
    let before = closing(&h, taken);
    let take = above(before, 210);
    set(&mut h, taken, BTC, levels(Some(&take.to_string()), None));
    h.advance_to(lowered_at);
    calm_price(&mut h, mid);
    let (old, new) = (mid - before, mid - closing(&h, taken));
    assert!(new < old, "the lowered ceiling narrows the close");
    let events = calm_price(&mut h, take + (old + new).div_int(2));
    assert!(
        events.iter().any(|e| matches!(e, Event::PositionLevelTriggered { account, .. } if *account == taken.account))
    );
}

/// BTC priced at `mid` past the fast-move window, so the move raises no stress, with the level index's
/// keys checked after it.
fn calm_price(h: &mut Harness, mid: Fixed) -> Vec<Event> {
    h.advance(60_001);
    let events = h.price(BTC, &mid.to_string());
    h.engine.assert_level_keys_current();
    events
}

/// What closing `client`'s whole BTC position fills at now.
fn closing(h: &Harness, client: &Client) -> Fixed {
    crate::common::levels::closing_price(h, client, BTC)
}

/// `price` less `tenths` tenths of a bp of it, rounded down, and plus them, rounded up.
fn below(price: Fixed, tenths: i64) -> Fixed {
    price + price.mul_tenth_bps_rounded(-tenths, crate::fixed::Rounding::Down)
}
fn above(price: Fixed, tenths: i64) -> Fixed {
    price + price.mul_tenth_bps_rounded(tenths, crate::fixed::Rounding::Up)
}

/// The ceiling BTC's 20bp is lowered to: above its 12bp launch close charge, and below what its doubled
/// bands charge a 120 BTC close at stress level 1.
const CEILING_BPS: i64 = 13;

/// The `V79` access harness with EUR-USD, USD-JPY and Germany 40 listed beside the genesis markets,
/// every market priced, and eight clients declared and funded.
fn setup() -> (Harness, Vec<Client>) {
    let mut h = Harness::v8(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    h.list_fx();
    let fx = [(EURUSD, 15), (USDJPY, 15), (DE40, 25)];
    let mut registry = ceilings();
    registry.extend(fx.map(|(market, bps)| CloseSpreadCeiling { market, max_close_spread_bps: bps }));
    h.cmd(Command::Admin(AdminCommand::SetCloseSpreadCeilings { ceilings: registry }));
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 * HOUR_MS }));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V79 });
    for (market, mid) in [(BTC, "100000"), (EURUSD, "1.1"), (USDJPY, "150"), (DE40, "20000")] {
        h.price(market, mid);
    }
    let clients: Vec<Client> = (10..18).map(Client::new).collect();
    for client in &clients {
        attest(&mut h, client);
        fund(&mut h, client, "50000000");
    }
    (h, clients)
}

/// Prices `mids[index]`'s market moved by up to `bps`.
fn reprice(h: &mut Harness, mids: &mut [(MarketId, Fixed)], index: usize, bps: u64, draws: &mut Draws) -> Vec<Event> {
    let (market, mid) = mids[index];
    let moved = draws.moved(mid, bps);
    mids[index].1 = moved;
    h.price(market, &moved.to_string())
}

/// Prices BTC where a level held there is about to fire: up to 10bp past it on its position's side,
/// where the spread a close pays at stress level 0 or 1 brings the closing price to it.
fn near_a_level(h: &mut Harness, mids: &mut [(MarketId, Fixed)], draws: &mut Draws) -> Vec<Event> {
    let held: Vec<(bool, Fixed)> = h
        .engine
        .accounts()
        .values()
        .filter_map(|acct| Some((acct.positions.get(&BTC)?.qty.is_positive(), *acct.levels.get(&BTC)?)))
        .flat_map(|(long, levels)| [levels.take_profit, levels.stop_loss].into_iter().flatten().map(move |l| (long, l)))
        .collect();
    if held.is_empty() {
        return reprice(h, mids, 0, 40, draws);
    }
    let (long, level) = held[draws.below(held.len() as u64) as usize];
    let tenths = draws.below(101) as i64 * if long { 1 } else { -1 };
    let mid = level + level.mul_tenth_bps_rounded(tenths, crate::fixed::Rounding::Down);
    mids[0].1 = mid;
    h.price(BTC, &mid.to_string())
}

/// Sets `client`'s levels in `market` a little beyond its minimum distance from the closing price, each
/// left out at times.
fn set_near(h: &mut Harness, client: &Client, market: MarketId, draws: &mut Draws) -> Vec<Event> {
    let Some(position) = h.engine.accounts()[&client.account].positions.get(&market).copied() else {
        return Vec::new();
    };
    let side = if position.qty.is_positive() { Side::Sell } else { Side::Buy };
    let Ok(quote) = h.engine.quote(client.account, market, side, position.qty.abs()) else { return Vec::new() };
    let reference = quote.price;
    let distance = h.engine.markets()[&market].spec.min_level_distance_bps.expect("listed under V79");
    let away = |draws: &mut Draws, sign: i64| {
        let tenths = (distance * 10 + 5 + draws.below(80) as i64) * sign;
        (reference + reference.mul_tenth_bps_rounded(tenths, crate::fixed::Rounding::Up)).to_string()
    };
    let up = if position.qty.is_positive() { 1 } else { -1 };
    let take_profit = (draws.below(4) != 0).then(|| away(draws, up));
    let stop_loss = (draws.below(4) != 0).then(|| away(draws, -up));
    set(h, client, market, levels(take_profit.as_deref(), stop_loss.as_deref()))
}

/// `s` as a `Fixed`.
fn f(s: &str) -> Fixed {
    s.parse().expect("a decimal")
}

#[test]
fn a_tier_held_only_in_unsettled_lots_is_read_again_when_its_first_day_leaves_the_window() {
    let (mut h, client, _) = crate::common::session_keys::funded_at(ProtocolVersion::LATEST);
    fund(&mut h, &client, "200000");
    // Two openings of $510k fifty seconds apart before a UTC midnight: each matures a minute later, the
    // first before midnight and the second after, and no fill after the first matures settles the
    // book, so its volume stays in its lots.
    let midnight = (crate::volume_tier::day_of(h.ts) + 1) * DAY_MS;
    h.advance_to(midnight - 90_000);
    h.price(BTC, "100000");
    let named = levels(None, Some("99000"));
    crate::common::levels::open_with(&mut h, &client, BTC, Side::Buy, "5.1", "100000", named);
    h.advance_to(midnight - 40_000);
    h.price(BTC, "100000");
    trade(&mut h, &client, BTC, Side::Buy, "5.1", "100000");
    h.advance_to(midnight + 30_000);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(client.account), 1);
    let book = h.engine.account_volume(client.account).expect("a book");
    assert!(book.days.is_empty(), "{book:?}");
    // Thirty days on, the first lot's day leaves the window at midnight and the tier falls.
    let leaves = midnight + 29 * DAY_MS;
    while h.ts + DAY_MS < leaves {
        h.advance(DAY_MS);
        h.cmd(Command::Tick);
        h.engine.assert_level_keys_current();
    }
    h.advance_to(leaves + 1);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.volume_tier(client.account), 0);
    h.engine.assert_level_keys_current();
}

#[test]
fn an_fx_move_re_keys_no_level_of_a_market_converting_at_it() {
    let (mut h, clients) = setup();
    let client = &clients[0];
    trade(&mut h, client, DE40, Side::Buy, "10", "20000");
    let mut draws = Draws(7);
    assert!(set_near(&mut h, client, DE40, &mut draws).iter().any(|e| matches!(e, Event::PositionLevelsSet { .. })));
    let keys = |h: &Harness| h.engine.hot_indexes.as_ref().unwrap().levels.markets[&DE40].keys.clone();
    let before = keys(&h);
    for mid in ["1.101", "1.099", "1.12"] {
        h.price(EURUSD, mid);
        h.price(DE40, "20000");
        assert_eq!(keys(&h), before, "EUR-USD at {mid}");
    }
}

#[test]
fn a_split_keys_the_levels_it_scales_again() {
    let (mut h, client, _) = crate::common::session_keys::funded_at(ProtocolVersion::LATEST);
    h.price(AAPL, "80");
    crate::common::levels::open_with(&mut h, &client, AAPL, Side::Buy, "3", "80", levels(Some("90"), Some("70")));
    let split = CorporateAction {
        id: "keys".into(),
        market: AAPL,
        previous_close: ny(2026, 10, 28, 16, 0),
        ex_open: ny(2026, 10, 29, 9, 30),
        adjustment: Adjustment::Split { new_units: 3, old_units: 4 },
    };
    assert!(h.operate(AdminCommand::ScheduleCorporateAction(split)).contains(&Event::AdminApplied));
    h.advance_to(ny(2026, 10, 29, 9, 30));
    h.price(AAPL, "106.666666667");
    h.engine.assert_level_keys_current();
    // The stop loss, scaled to 93.333333333, fires on the first price whose close reaches it.
    let events = h.price(AAPL, "93.3");
    assert!(events.iter().any(|e| matches!(e, Event::PositionLevelTriggered { .. })), "{events:?}");
}

/// What a price's level path costs at 100k levels, timed alone: `Engine::trigger_levels` on each
/// sample's price, after the price command, so the close-out scan of the market's holders that every
/// price runs is left out. Levels a price does not reach cost it nothing, inside the ceiling's reach of
/// the index or out of it, and in a market converting at another's index while that index moves: each
/// is held to the resting case's 200 us p99. A market converting at another's index keys its levels
/// without that index, by its ceiling and least fill rate, so one inside the ceiling's reach of its
/// index is priced on every price: that case is timed and printed, over fewer samples, not held.
#[test]
#[ignore = "100k-level benchmark: run in an isolated CPU window"]
fn the_level_path_of_a_price_reaching_no_level_at_100k_levels() {
    const COPIES: usize = 100_000;
    const SAMPLES: usize = 100;
    // BTC's ceiling is 20bp: at 99,950 a long closes at 99,940, and a stop loss at 99,790 is inside
    // the ceiling's reach of the index, out of the closing price's.
    // Each case's name, market, the FX market whose prices move it, its two prices and the stop loss,
    // none for one set at the distance and brought inside the ceiling's reach (`in_band`).
    type Case<'a> = (&'a str, MarketId, Option<MarketId>, [&'a str; 2], Option<&'a str>);
    let cases: [Case; 4] = [
        ("in_band", BTC, None, ["99950", "99951"], Some("99790")),
        ("out_of_band", BTC, None, ["100000", "100001"], Some("99000")),
        ("fx_moves_out_of_band", DE40, Some(EURUSD), ["1.1", "1.1001"], Some("19000")),
        ("fx_moves_in_band", DE40, Some(EURUSD), ["1.1", "1.1001"], None),
    ];
    for (name, market, fx, mids, stop) in cases {
        let (mut h, clients) = setup();
        let client = &clients[0];
        let entry = if market == BTC { "100000" } else { "20000" };
        trade(&mut h, client, market, Side::Buy, "0.0001", entry);
        let (stop, at) = match stop {
            Some(stop) => (crate::common::f(stop), crate::common::f(entry)),
            None => in_band(&h, client, market),
        };
        let events = set(&mut h, client, market, levels(None, Some(&stop.to_string())));
        assert!(events.iter().any(|e| matches!(e, Event::PositionLevelsSet { .. })), "{events:?}");
        copy_account(&mut h, client.account, market, COPIES);
        let samples = if stop_held(name) { SAMPLES } else { SAMPLES / 5 };
        let mut times = Vec::with_capacity(samples);
        for sample in 0..samples {
            h.advance(1);
            match fx {
                Some(fx) => {
                    h.price(market, &at.to_string());
                    h.price(fx, mids[sample % 2]);
                }
                None => {
                    h.price(market, mids[sample % 2]);
                }
            }
            let mut events = Vec::new();
            let began = std::time::Instant::now();
            h.engine.trigger_levels(market, &mut events);
            times.push(began.elapsed().as_micros());
            assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))), "{name}: {events:?}");
        }
        if !stop_held(name) {
            let index = &h.engine.hot_indexes.as_ref().unwrap().levels.markets[&market];
            assert_eq!(index.reached(at).len(), COPIES + 1, "{name}: every level is in reach");
        }
        times.sort_unstable();
        let (p50, p99) = (times[samples / 2 - 1], times[samples * 99 / 100 - 1]);
        println!("LATENCY level_path_{name} levels={COPIES} samples={samples} p50_us={p50} p99_us={p99}");
        if stop_held(name) {
            assert!(p99 <= 200, "{name} p99 {p99}us exceeds 200us");
        }
    }
}

/// Whether the benchmark holds case `name` to the 200 us budget.
fn stop_held(name: &str) -> bool {
    name != "fx_moves_in_band"
}

/// A stop loss on `client`'s long in `market` at the minimum distance below its closing price, and an
/// index a quarter of that distance below the closing price, where the ceiling's reach covers the stop
/// loss and the closing price does not reach it.
fn in_band(h: &Harness, client: &Client, market: MarketId) -> (Fixed, Fixed) {
    let closing = crate::common::levels::closing_price(h, client, market);
    let distance = h.engine.markets()[&market].spec.min_level_distance_bps.unwrap() * 10;
    let stop = closing + closing.mul_tenth_bps_rounded(-distance, crate::fixed::Rounding::Down);
    (stop, closing - (closing - stop).div_int(4))
}

/// Gives `count` copies of `account` the same state and the market its aggregates, and rebuilds the
/// indexes.
fn copy_account(h: &mut Harness, account: crate::types::AccountId, market: MarketId, count: usize) {
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let key_of = |id: crate::types::AccountId| serde_json::to_value(id).unwrap().as_str().unwrap().to_owned();
    let template = state["accounts"][key_of(account)].clone();
    for index in 0..count as u64 {
        let mut address = [0xee; 20];
        address[12..].copy_from_slice(&(index + 1).to_be_bytes());
        state["accounts"][key_of(crate::types::AccountId(address))] = template.clone();
    }
    let book = &mut state["markets"][market.to_string()];
    for field in ["client_qty", "client_cost"] {
        let one: Fixed = serde_json::from_value(book[field].clone()).expect("an amount");
        book[field] = serde_json::to_value(one.mul(Fixed::from_int(count as i64 + 1))).expect("an amount");
    }
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    h.engine.restore_indexes();
}
