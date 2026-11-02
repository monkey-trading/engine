//! A close-all gives the outcome of the same reduce-only market orders sent one at a time: the
//! same events, balances, positions, market aggregates and house cash, over generated books,
//! price moves (which can close accounts out before the batch), quotes and slippage bounds.

mod common;

use std::cell::RefCell;

use common::batch::{funded_emitting, onboard, singles};
use common::close_spread::{order, rest};
use common::{AAPL, BTC, Client, ETH, Harness, UNLISTED};
use engine_core::command::{CloseEntry, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::types::{MarketId, Side};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

const SEED: u64 = 0x5eed_0043;
const CASES: u32 = 192;
/// Each listed market's launch price, in cents.
const BASE: [(MarketId, i64); 3] = [(BTC, 10_000_000), (ETH, 300_000), (AAPL, 20_000)];

/// One generated book, move set and batch.
#[derive(Clone, Debug)]
struct Case {
    /// (account index, market, long, quantity in hundredths of a unit)
    opens: Vec<(usize, MarketId, bool, i64)>,
    /// (account index, market) of a limit buy resting far below the market, which no move reaches
    rests: Vec<(usize, MarketId)>,
    /// (market, move in bps), applied in order
    moves: Vec<(MarketId, i64)>,
    /// (market, quote offset from the market's price in bps), in the order the batch lists them
    closes: Vec<(MarketId, i64)>,
    max_slippage_bps: Option<i64>,
}

fn market() -> impl Strategy<Value = MarketId> {
    prop_oneof![Just(BTC), Just(ETH), Just(AAPL)]
}

fn case() -> impl Strategy<Value = Case> {
    let opens = prop::collection::vec((0..3usize, market(), any::<bool>(), 1..400i64), 0..8);
    let rests = prop::collection::vec((0..3usize, market()), 0..6);
    let moves = prop::collection::vec((market(), -900..900i64), 0..4);
    // A permutation of a subset of the listed markets, possibly with one unlisted entry.
    let closes =
        (Just(vec![BTC, ETH, AAPL]).prop_shuffle(), 1..=3usize, any::<bool>(), prop::collection::vec(-45..45i64, 4))
            .prop_map(|(order, keep, unlisted, offsets)| {
                let mut markets: Vec<MarketId> = order.into_iter().take(keep).collect();
                if unlisted && markets.len() < 3 {
                    markets.push(UNLISTED);
                }
                markets.into_iter().zip(offsets).collect::<Vec<_>>()
            });
    let bound = prop_oneof![Just(None), (0..60i64).prop_map(Some)];
    (opens, rests, moves, closes, bound).prop_map(|(opens, rests, moves, closes, max_slippage_bps)| Case {
        opens,
        rests,
        moves,
        closes,
        max_slippage_bps,
    })
}

/// `cents` as a decimal string.
fn dollars(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// The deterministic book `case` describes, three funded accounts, and each market's last price.
fn book(case: &Case) -> (Harness, [Client; 3], Vec<(MarketId, i64)>) {
    let (mut h, first, _) = funded_emitting();
    let clients = [first, Client::new(3), Client::new(4)];
    onboard(&mut h, &clients[1], "100000");
    onboard(&mut h, &clients[2], "100000");
    let mut prices: Vec<(MarketId, i64)> = BASE.to_vec();
    for &(market, cents) in &prices {
        h.price(market, &dollars(cents));
    }
    for &(index, market, long, hundredths) in &case.opens {
        let cents = prices.iter().find(|(m, _)| *m == market).map(|(_, c)| *c).unwrap();
        let qty = format!("{}.{:02}", hundredths / 100, hundredths % 100);
        let side = if long { Side::Buy } else { Side::Sell };
        order(&mut h, &clients[index], market, side, &qty, &dollars(cents), false);
    }
    // A buy resting at 40% of the launch price, whose notional clears the minimum, stays untriggered
    // through the moves below (four steps of at most 9%, so the price stays above 65%).
    for &(index, market) in &case.rests {
        let cents = BASE.iter().find(|(m, _)| *m == market).map(|(_, c)| *c).unwrap();
        let (qty, price) = (if market == BTC { "0.1" } else { "1" }, dollars(cents * 4 / 10));
        rest(&mut h, &clients[index], market, Side::Buy, qty, OrderKind::Limit { price: common::f(&price) });
    }
    for &(market, bps) in &case.moves {
        let slot = prices.iter_mut().find(|(m, _)| *m == market).unwrap();
        slot.1 = slot.1 * (10_000 + bps) / 10_000;
        h.price(market, &dollars(slot.1));
    }
    (h, clients, prices)
}

fn entries(case: &Case, prices: &[(MarketId, i64)]) -> Vec<CloseEntry> {
    case.closes
        .iter()
        .map(|&(market, offset)| {
            let cents = prices.iter().find(|(m, _)| *m == market).map_or(10_000, |(_, c)| *c);
            let quoted = (cents * (10_000 + offset) / 10_000).max(1);
            CloseEntry { market, quoted: common::f(&dollars(quoted)) }
        })
        .collect()
}

fn outcome(h: &Harness) -> serde_json::Value {
    serde_json::json!({
        "accounts": h.engine.accounts(),
        "markets": h.engine.markets(),
        "house_cash": h.engine.house_cash(),
        // MONKEY emission weight inputs: gross losses accumulate per fill, from `V43`.
        "period_results": h.engine.period_results(),
    })
}

#[test]
fn a_close_all_matches_the_same_reduce_only_orders_sent_one_at_a_time() {
    let reached: RefCell<std::collections::BTreeSet<&'static str>> = RefCell::default();
    let config = Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), ..Config::default() };
    let result = TestRunner::new(config).run(&case(), |case| {
        let (mut batch, clients, prices) = book(&case);
        let (mut one_by_one, _, _) = book(&case);
        let list = entries(&case, &prices);
        let before = outcome(&batch);
        let orders = |h: &Harness| serde_json::to_value(&h.engine.accounts()[&clients[0].account].orders).unwrap();
        let resting = orders(&batch);
        assert_eq!(before, outcome(&one_by_one), "the two books start identical");

        let nonce = batch.engine.next_request_nonce(clients[0].account);
        let cmd = common::batch::close_all(
            &batch,
            &clients[0],
            clients[0].account,
            nonce,
            list.clone(),
            case.max_slippage_bps,
        );
        let events = batch.cmd(cmd);
        let expected: Vec<Event> =
            singles(&mut one_by_one, &clients[0], &list, case.max_slippage_bps).into_iter().flatten().collect();

        prop_assert_eq!(&events, &expected);
        prop_assert_eq!(outcome(&batch), outcome(&one_by_one));
        // Resting orders neither fill nor cancel: a close-all is market orders only.
        prop_assert_eq!(orders(&batch), resting.clone());
        // Accounts other than the requester are untouched, and cash is conserved.
        let others = |h: &Harness| serde_json::to_value(&h.engine.accounts()[&clients[1].account]).unwrap();
        prop_assert_eq!(others(&batch), others(&one_by_one));
        batch.assert_conserved(engine_core::Fixed::ZERO);
        // One request, one nonce.
        prop_assert_eq!(batch.engine.next_request_nonce(clients[0].account), nonce + 1);

        let mut reached = reached.borrow_mut();
        for event in &events {
            match event {
                Event::Fill(fill) => {
                    // A close never flips: an unclamped whole-position close ends flat.
                    prop_assert!(fill.position.is_zero(), "{fill:?}");
                    reached.insert("fill");
                    if fill.liquidation {
                        return Err(TestCaseError::fail("a close-all fill is never a liquidation"));
                    }
                }
                Event::OrderRejected { reason: Reject::Slippage { .. }, .. } => {
                    reached.insert("slippage");
                }
                Event::OrderRejected { reason: Reject::ReduceOnly, .. } => {
                    reached.insert("no position");
                }
                Event::OrderRejected { reason: Reject::UnknownMarket, .. } => {
                    reached.insert("unlisted market");
                }
                // Close-outs run on prices and schedules, never inside an order: a batch has none.
                Event::CloseOut { .. } => return Err(TestCaseError::fail("a close-all closed an account out")),
                _ => {}
            }
        }
        if events.iter().filter(|e| matches!(e, Event::Fill(_))).count() > 1 {
            reached.insert("several fills in one batch");
        }
        if batch.engine.period_results().get(&clients[0].account).is_some_and(|result| result.losses.is_positive()) {
            reached.insert("gross loss recorded for emissions");
        }
        if resting.as_object().is_some_and(|orders| !orders.is_empty()) {
            reached.insert("resting orders kept through a close-all");
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let reached = reached.into_inner();
    let expected = [
        "fill",
        "slippage",
        "no position",
        "unlisted market",
        "several fills in one batch",
        "resting orders kept through a close-all",
        "gross loss recorded for emissions",
    ];
    for path in expected {
        assert!(reached.contains(path), "no generated case reached `{path}`: {reached:?}");
    }
}
