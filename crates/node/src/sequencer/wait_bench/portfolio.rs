//! `price_wait_budget` with portfolio accounts (#916): every one of the rig's 100k accounts holds a hedged
//! pair in the development genesis's offset group, BTC-USD against ETH-USD, half of them each way round,
//! which from `V81` close-outs and from `V84` new risk and withdrawals read under the portfolio requirement,
//! so every BTC-USD price has 100k holders whose close-out the index keys.

use engine_core::Fixed;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Side};

use super::{ACCOUNTS, Rig, account, hold_price_wait};
use crate::clock::Clock;

const BTC: MarketId = 1;
const ETH: MarketId = 2;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_price_wait_budget() {
    let mut rig = Rig::new("wait-portfolio");
    hedge(&mut rig);
    rig.activate_latest();
    hold_price_wait(rig);
}

/// Gives each of the rig's accounts a $100 BTC-USD/ETH-USD pair, long BTC-USD and short ETH-USD or the other
/// way round in turn, under `V23`, whose unsigned orders the rig sequences, pricing both markets again every
/// 512 accounts so no quote ages out.
fn hedge(rig: &mut Rig) {
    for index in 0..ACCOUNTS {
        if index % 512 == 0 {
            reprice(rig);
        }
        for (market, side, qty, quoted) in [(BTC, Side::Buy, "0.001", 100_000), (ETH, Side::Sell, "0.025", 4_000)] {
            // Every other account holds the pair the other way round, so the house's net exposure stays
            // within its caps.
            let side = if index % 2 == 0 { side } else { side.opposite() };
            let order = OrderRequest {
                account: account(index),
                client_id: u64::from(index),
                market,
                side,
                qty: qty.parse().unwrap(),
                kind: OrderKind::Market { quoted: Fixed::from_int(quoted), max_slippage_bps: Some(10_000) },
                reduce_only: false,
                auth: None,
                take_profit: None,
                stop_loss: None,
            };
            let stamped = rig.seq.sequence(Command::PlaceOrder(order)).unwrap().expect("sequenced");
            assert!(stamped.as_slice().iter().any(|event| matches!(event, Event::Fill(_))), "{index}");
        }
        if index % 1024 == 1023 {
            rig.seq.flush().unwrap();
        }
    }
    rig.seq.flush().unwrap();
}

/// Prices BTC-USD at 100,000 and ETH-USD at 4,000 from two venues, stamped now.
fn reprice(rig: &mut Rig) {
    for (market, mid) in [(BTC, 100_000), (ETH, 4_000)] {
        let ts = rig.clock.as_ref().now_ms();
        let quotes = ["venue-a", "venue-b"].map(|source| SourceQuote {
            source: source.into(),
            mid: Fixed::from_int(mid),
            ts,
            contract: None,
        });
        rig.seq.sequence(Command::Prices { market, quotes: quotes.to_vec() }).unwrap();
    }
}
