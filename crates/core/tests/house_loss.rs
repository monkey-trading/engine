//! `V59`'s stress trigger on the house's marked loss to its clients over the trailing day, through
//! `Engine::apply`: activation gives level 2 a threshold of zero; once the house has lost money over
//! the day every priced market is at level 2 before the next fill, a close there pays the level's
//! spread up to its ceiling, and the level steps down after the cooldown once the loss is back at or
//! below zero, or has aged out of the window; and a pair held across an unpriced move fires it when the
//! market prices again, though the close-out realises a gain. What the loss counts is in
//! `house_loss_accounting.rs`, and the bounds on its parameters in `house_loss_bounds.rs`.

mod common;

use common::close_spread::{evidence, reduce, trade};
use common::events::only_fill;
use common::house_loss::{harness, level, loss, reprice, second_client};
use common::session_keys::funded_at;
use common::{AAPL, BTC, ETH, Harness, f};
use engine_core::Fixed;
use engine_core::access::AccessDeposit;
use engine_core::command::Command;
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::house_loss::{LossTrigger, SLOT_MS, WINDOW_MS, loss_triggers};
use engine_core::params::StressTrigger;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

/// The harness's stress cooldown.
const COOLDOWN_MS: Millis = 300_000;
const DAY_MS: Millis = 24 * 60 * 60 * 1000;

#[test]
fn activation_gives_level_two_a_threshold_of_zero_and_records_it_as_launch() {
    let (h, _) = harness(1_000, "100000");
    let params = h.engine.params().expect("set");
    let zero = StressTrigger { house_loss: Some(Fixed::ZERO), ..StressTrigger::default() };
    assert_eq!(params.stress_levels[2].trigger, Some(zero));
    let launch = vec![LossTrigger { level: 2, house_loss: Fixed::ZERO }];
    assert_eq!(h.engine.operator_bounds().launch_loss_triggers, Some(launch));
    // An idle house has lost nothing, which is not above zero.
    let reading = h.engine.house_loss().expect("V59");
    assert_eq!((reading.loss, reading.level), (Fixed::ZERO, 0));
    assert_eq!([level(&h, BTC), level(&h, ETH)], [0, 0]);

    // Parameters with no level 2 take no threshold, and launch records none.
    let (h, _, _) = funded_at(ProtocolVersion::V59);
    assert!(loss_triggers(&h.engine.params().expect("set").stress_levels).is_empty());
    assert_eq!(h.engine.operator_bounds().launch_loss_triggers, Some(vec![]));
}

#[test]
fn once_the_house_has_lost_money_over_the_day_every_priced_market_is_at_level_two() {
    let (mut h, client) = harness(1_000, "1000000");
    let open = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000")).clone();
    // The open pays BTC's first band, 1 bp: the house is 10 ahead.
    assert_eq!((open.spread_bps, open.price), (1, f("100010")));
    assert_eq!(loss(&h), f("-10"));
    // A move the client gains less than its spread on leaves the house ahead.
    reprice(&mut h, "100005");
    assert_eq!((loss(&h), level(&h, BTC)), (f("-5"), 0));

    // At 100020 the client is 10 up: the house has lost 10 over the day, so BTC and ETH rise to level 2
    // in the same command. AAPL has no price, and nothing fills there until it has one.
    let events = h.price(BTC, "100020");
    assert_eq!(loss(&h), f("10"));
    assert!(events.contains(&Event::HouseLossChanged { level: 2, loss: f("10") }), "{events:?}");
    assert!(events.contains(&Event::StressChanged { market: BTC, level: 2 }), "{events:?}");
    assert!(events.contains(&Event::StressChanged { market: ETH, level: 2 }), "{events:?}");
    assert_eq!(level(&h, AAPL), 0);
    // Its next fill pays level 2: the 6 bp minimum and four times the 1 bp band.
    let add = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100020")).clone();
    assert_eq!(add.spread_bps, 10);
}

#[test]
fn the_level_steps_down_after_the_cooldown_once_the_loss_is_at_or_below_zero() {
    let (mut h, client) = fired();
    // Back under the client's entry, the house is ahead again, but the level holds through the cooldown.
    h.advance(1_000);
    let events = h.price(BTC, "99990");
    assert!(events.contains(&Event::HouseLossChanged { level: 0, loss: f("-20") }), "{events:?}");
    assert_eq!(level(&h, BTC), 2);
    h.advance(COOLDOWN_MS - 1_001);
    reprice(&mut h, "99990");
    assert_eq!([level(&h, BTC), level(&h, ETH)], [2, 2]);
    h.advance(1);
    reprice(&mut h, "99990");
    assert_eq!([level(&h, BTC), level(&h, ETH)], [0, 0]);
    let close = only_fill(&reduce(&mut h, &client, BTC, Side::Sell, "1", "99990")).clone();
    assert_eq!(close.spread_bps, 1, "a close back at level 0 pays the band alone");

    // A loss still above zero past the cooldown keeps the level.
    let (mut h, _) = fired();
    h.advance(COOLDOWN_MS);
    reprice(&mut h, "100020");
    assert_eq!((loss(&h), level(&h, BTC)), (f("10"), 2));
}

#[test]
fn a_loss_older_than_the_window_leaves_it() {
    let (mut h, _) = fired();
    let fired_at = h.ts;
    // The client keeps its gain open, and the house's loss stays above zero while it is in the window.
    h.advance_to(fired_at + DAY_MS - 60_000);
    reprice(&mut h, "100020");
    assert_eq!((loss(&h), level(&h, BTC)), (f("10"), 2));
    // A slot past a day after the gain, the window starts after it: the house has lost nothing since.
    h.advance_to(fired_at + WINDOW_MS + SLOT_MS);
    let events = h.price(BTC, "100020");
    assert!(events.contains(&Event::HouseLossChanged { level: 0, loss: Fixed::ZERO }), "{events:?}");
    assert_eq!(level(&h, BTC), 0);
}

#[test]
fn a_close_at_level_two_pays_the_level_spread_up_to_its_ceiling() {
    let (mut h, client) = harness(1_000, "2000000");
    // 12m of BTC pays 4,400 through its bands on the open, 3.67 bps.
    let open = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "120", "100000")).clone();
    assert_eq!(open.spread_bps, 3);
    // A 0.1% move puts the client 7,600 up, and BTC at level 2.
    reprice(&mut h, "100100");
    assert!(loss(&h).is_positive());
    assert_eq!(level(&h, BTC), 2);
    // Level 2 charges 6 bps and four times the 3.67, 20.67 bps, past BTC's 20 bp ceiling: the pure
    // close pays the ceiling.
    let close = only_fill(&reduce(&mut h, &client, BTC, Side::Sell, "120", "100100")).clone();
    assert_eq!(close.index - close.price, close.index.mul_bps(20), "{close:?}");
}

#[test]
fn a_pair_across_an_unpriced_move_fires_the_trigger_when_the_market_prices_again() {
    // BTC at 1% margin: each leg holds 1m of BTC on 20,000.
    let (mut h, long) = harness(100, "20000");
    let short = second_client(&mut h, 2, "20000");
    only_fill(&trade(&mut h, &long, BTC, Side::Buy, "10", "100000"));
    only_fill(&trade(&mut h, &short, BTC, Side::Sell, "10", "100000"));
    assert_eq!(loss(&h), f("-200"), "the pair pays the house both spreads");
    assert_eq!(level(&h, BTC), 0);

    // The venues go quiet, and BTC prices again 3% higher.
    h.advance(5_000);
    let cash = h.engine.house_cash();
    let events = h.price(BTC, "103000");
    // The short's 30,100 loss passes its balance: its close-out takes its whole 20,000 for the house and
    // writes off the rest, a realised gain, while the long's 29,900 stays open.
    let short_closed = |e: &Event| matches!(e, Event::CloseOut { account, .. } if *account == short.account);
    assert!(events.iter().any(short_closed), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::BadDebt { account, .. } if *account == short.account)));
    assert_eq!(h.engine.house_cash() - cash, f("20000"));
    // The marked loss counts the open gain: the house is down 9,900 on the day, all of it this one
    // command's, since it had lost nothing before.
    assert_eq!(loss(&h), f("9900"));
    assert!(events.contains(&Event::HouseLossChanged { level: 2, loss: f("9900") }), "{events:?}");
    assert_eq!(level(&h, BTC), 2);
    // The long's close pays level 2: 6 bps and four times its 1.03 bp band charge, against the
    // 1.03 bps level 0 charges.
    let close = only_fill(&reduce(&mut h, &long, BTC, Side::Sell, "10", "103000")).clone();
    assert_eq!(close.spread_bps, 10, "{close:?}");
}

#[test]
fn whenever_the_house_has_lost_money_over_the_day_every_priced_market_is_at_level_two() {
    let (mut h, winner) = harness(1_000, "5000000");
    let honest = second_client(&mut h, 2, "5000000");
    let (mut mid, mut state, mut held) = (100_000i64, 11u64, 0i64);
    let (mut fired, mut cleared, mut capital) = (0, 0, 0);
    // About three days of 10-minute steps, so losses also age out of the window.
    for step in 0..400 {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        h.advance(1 + (state >> 40) as i64 % 600_000);
        mid += ((state >> 20) % 41) as i64 * 10 - 200;
        let price = mid.to_string();
        let mut events = h.price(BTC, &price);
        events.extend(h.price(ETH, "3000"));
        let before = loss(&h);
        events.extend(match (state >> 8) % 5 {
            // The winner buys or sells a BTC; the honest client round-trips one, paying its spreads.
            0 | 1 if held < 5 => {
                held += 1;
                trade(&mut h, &winner, BTC, Side::Buy, "1", &price)
            }
            0 | 1 => {
                held -= 1;
                trade(&mut h, &winner, BTC, Side::Sell, "1", &price)
            }
            2 => {
                trade(&mut h, &honest, BTC, Side::Buy, "1", &price);
                trade(&mut h, &honest, BTC, Side::Sell, "1", &price)
            }
            // House capital moved in is no loss.
            3 => {
                capital += 1;
                let events = credit_house(&mut h, honest.account, "250000");
                assert_eq!(loss(&h), before, "step {step}: capital moved the loss");
                events
            }
            _ => vec![],
        });
        fired += events.iter().filter(|e| matches!(e, Event::HouseLossChanged { level: 2, .. })).count();
        cleared += events.iter().filter(|e| matches!(e, Event::HouseLossChanged { level: 0, .. })).count();
        // Above zero, BTC is at level 2; at level 2, the loss is above zero or the cooldown still runs.
        let (loss, stress) = (loss(&h), &h.engine.markets()[&BTC].stress);
        if loss.is_positive() {
            assert_eq!(stress.level, 2, "step {step}: a loss of {loss} at level {}", stress.level);
        } else if stress.level == 2 {
            assert!(h.ts - stress.auto_since < COOLDOWN_MS, "step {step}: level 2 past its cooldown at {loss}");
        }
    }
    assert!(fired > 1 && cleared > 1 && capital > 1, "fired {fired}, cleared {cleared}, capital {capital}");
}

// ── Helpers ──

/// `harness` with client 1 a BTC long from 100010 and the house 10 down at 100020, so BTC and ETH are
/// at level 2.
fn fired() -> (Harness, common::Client) {
    let (mut h, client) = harness(1_000, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    reprice(&mut h, "100020");
    assert_eq!((loss(&h), level(&h, BTC), level(&h, ETH)), (f("10"), 2, 2));
    (h, client)
}

/// Credits `amount` of house capital paid by `payer`, through the screened deposit command.
fn credit_house(h: &mut Harness, payer: engine_core::types::AccountId, amount: &str) -> Vec<Event> {
    let deposit = AccessDeposit {
        account: None,
        payer: Some(payer),
        amount: f(amount),
        chain_ref: format!("0xhouse:{}", h.log.len()),
    };
    let ev = evidence(h, eip712::deposit_screening_hash(&deposit), None, Some(payer));
    let events = h.cmd(Command::AccessDeposit { deposit, evidence: ev });
    assert!(events.iter().any(|e| matches!(e, Event::HouseCapitalMoved { .. })), "{events:?}");
    events
}
