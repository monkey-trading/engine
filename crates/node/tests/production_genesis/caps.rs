//! The launch exposure-cap floors and level-1 utilisation (#696, #614's decision D8): from a zero
//! house every launch market admits an opening trade up to its floor and refuses past it, the total
//! floor caps the markets together, and no market starts above stress level 0.

use std::path::Path;

use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::{Event, Fill, Reject};
use engine_core::market::{MarketSpec, QuoteCurrency};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis, Side};
use engine_core::{Engine, Fixed};
use engine_node::NodeConfig;
use engine_node::config::Genesis;
use engine_node::sequencer::genesis_commands;

use crate::{DEV, access, production_genesis, utc};

/// The index every market is priced at; USD-JPY at it makes a yen-quoted market's USD notional its
/// quantity.
const MID: i64 = 100;
const TRADER: u8 = 1;
const BTC: MarketId = 1;
const ETH: MarketId = 2;
const US500: MarketId = 20;

/// #614's decision D8, in whole USD: each market's floor and the total floor.
const TOTAL_FLOOR: i64 = 61_000_000;
fn decided_floor(symbol: &str) -> i64 {
    match symbol {
        "BTC-USD" => 31_600_000,
        "ETH-USD" | "US500" | "US100" => 17_800_000,
        "SOL-USD" => 5_620_000,
        "EUR-USD" | "GBP-USD" | "USD-JPY" | "AUD-USD" | "US30" | "JP225" | "XAU-USD" | "XAG-USD" | "WTI" => 10_000_000,
        share => {
            assert!(crate::SYMBOLS[14..].contains(&share), "{share} is not a launch market");
            3_160_000
        }
    }
}

#[test]
fn the_genesis_carries_decision_d8s_floors_and_level_one() {
    let genesis = production_genesis("cap-values");
    assert_eq!(genesis.params.house_total_cap_floor, Fixed::from_int(TOTAL_FLOOR));
    assert_eq!(genesis.markets.len(), crate::SYMBOLS.len());
    for market in &genesis.markets {
        assert_eq!(market.house_cap_floor, Fixed::from_int(decided_floor(&market.symbol)), "{}", market.symbol);
    }
    let utilisation: Vec<Option<i64>> =
        genesis.params.stress_levels.iter().map(|l| l.trigger.as_ref().and_then(|t| t.utilisation_bps)).collect();
    assert_eq!(utilisation, [None, Some(8_000), Some(9_500)]);
}

#[test]
fn no_priced_launch_market_starts_above_level_zero_from_a_zero_house() {
    let mut node = LaunchNode::boot("level-zero");
    assert_eq!(node.engine.house_cash(), Fixed::ZERO);
    node.price_all();
    for market in &node.genesis.markets {
        let state = &node.engine.markets()[&market.id];
        assert!(state.mark.is_some(), "{} is priced", market.symbol);
        assert_eq!(state.stress.level, 0, "{}", market.symbol);
    }
}

#[test]
fn every_launch_market_admits_an_opening_trade_to_its_floor_and_refuses_past_it() {
    let genesis = production_genesis("floor-fills");
    for market in &genesis.markets {
        let mut node = LaunchNode::boot("floor-fills");
        node.price_all();
        assert_eq!(node.engine.house_cash(), Fixed::ZERO);
        // Level 1 fires at 80% of the cap, level 2 at 95%: 7900 stays normal where the earlier
        // 7000 would have fired, and the cap itself is admitted.
        for (bps, level) in [(7_900, 0), (8_000, 1), (9_500, 2), (10_000, 2)] {
            let target = Fixed::from_int(decided_floor(&market.symbol)).mul_bps(bps);
            let held = node.engine.markets()[&market.id].client_qty;
            let fill = node.buy(market, node.qty(market, target) - held).unwrap_or_else(|reject| {
                panic!("{} refused an opening trade at {bps} bps of its floor: {reject:?}", market.symbol)
            });
            assert_eq!(fill.qty, node.qty(market, target) - held, "{}", market.symbol);
            let state = &node.engine.markets()[&market.id];
            assert_eq!(state.stress.level, level, "{} at {bps} bps of its floor", market.symbol);
        }
        let past = node.buy(market, Fixed::ONE);
        assert_eq!(past.unwrap_err(), Reject::HouseCapacity, "{}", market.symbol);
    }
}

#[test]
fn the_total_floor_caps_the_markets_together() {
    let mut node = LaunchNode::boot("total-floor");
    node.price_all();
    let spec = |id| node.genesis.markets.iter().find(|m| m.id == id).unwrap().clone();
    let (btc, eth, us500) = (spec(BTC), spec(ETH), spec(US500));
    for market in [&btc, &eth] {
        let floor = Fixed::from_int(decided_floor(&market.symbol));
        node.buy(market, node.qty(market, floor)).expect("opens at its floor");
    }
    // $49.4M is held across the two; $11.6M is the $61M total floor's room.
    let room = Fixed::from_int(TOTAL_FLOOR) - node.engine.house_gross_exposure();
    assert_eq!(room, Fixed::from_int(11_600_000));
    let over = node.buy(&us500, node.qty(&us500, room + Fixed::from_int(100)));
    assert_eq!(over.unwrap_err(), Reject::HouseCapacity);
    node.buy(&us500, node.qty(&us500, room)).expect("the total floor's room opens");
    assert_eq!(node.engine.house_gross_exposure(), Fixed::from_int(TOTAL_FLOOR));
}

// ── Helpers ──

/// A node booted as production does, at a time every launch market is open.
struct LaunchNode {
    engine: Engine,
    genesis: Genesis,
    ts: Millis,
}

impl LaunchNode {
    /// Sequences client auth, the operator, the latest protocol and the production genesis at
    /// Tue 1 Dec 2026 15:00 UTC, which is 10:00 in New York, 09:00 in Chicago and 00:00 the next
    /// day in Tokyo during JPX's evening session, so that every launch market's venue is open. Then
    /// funds and declares the trader.
    fn boot(test: &str) -> Self {
        let genesis = production_genesis(test);
        let mut node = LaunchNode { engine: Engine::new(), genesis, ts: utc(2026, 12, 1, 15) };
        let operator = NodeConfig::load(Path::new(DEV)).unwrap().operator().unwrap();
        let activation = [
            Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
            Command::Admin(AdminCommand::SetOperator(operator)),
            Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }),
            Command::Admin(AdminCommand::SetAccessPolicy(access::policy())),
        ];
        let genesis = genesis_commands(&node.genesis);
        for cmd in activation.into_iter().chain(genesis).chain([Command::Tick]) {
            let events = node.apply(cmd);
            assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
        }
        assert!(node.engine.markets().values().all(|m| m.open), "every launch market is open");
        let declared = node.apply(access::declaration(&node.engine, TRADER));
        assert!(matches!(declared.as_slice(), [Event::EligibilityAttested { .. }]), "{declared:?}");
        let trader = access::account(TRADER);
        let funding = access::funding(&node.engine, Some(trader), Some(trader), Fixed::from_int(500_000_000), "cap");
        let funded = node.apply(funding);
        assert!(matches!(funded.as_slice(), [Event::Deposited { .. }]), "{funded:?}");
        node
    }

    fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let seq = self.engine.last_seq().map_or(0, |seq| seq + 1);
        self.engine.apply(&Sequenced { seq, ts: self.ts, cmd }).expect("in sequence")
    }

    /// Prices every market at `MID` from two mock venues, in listing order so USD-JPY precedes the
    /// yen-quoted market, a futures-priced market on the contract in front now.
    fn price_all(&mut self) {
        for market in self.genesis.markets.clone() {
            let contract = market
                .contracts
                .iter()
                .rev()
                .find(|c| c.from.timestamp_millis() <= self.ts)
                .map(|c| c.contract.clone());
            let quotes = ["mock-a", "mock-b"]
                .into_iter()
                .map(|source| SourceQuote {
                    source: source.into(),
                    mid: Fixed::from_int(MID),
                    ts: self.ts,
                    contract: contract.clone(),
                })
                .collect();
            let events = self.apply(Command::Prices { market: market.id, quotes });
            assert!(!events.iter().any(|e| matches!(e, Event::PricesRejected { .. })), "{}: {events:?}", market.symbol);
        }
    }

    /// The quantity of `market` whose notional at `MID` is `usd`.
    fn qty(&self, market: &MarketSpec, usd: Fixed) -> Fixed {
        match market.quote_currency {
            QuoteCurrency::Jpy { .. } => usd,
            QuoteCurrency::Usd => usd.div_int(MID),
            other => panic!("{}: no launch market is quoted in {other:?} here", market.symbol),
        }
    }

    /// A market buy from the trader that quotes twice the index, so the slippage guard, which compares
    /// against the quote, never stops a fill at a stress level's wider spread: its fill, or the refusal.
    fn buy(&mut self, market: &MarketSpec, qty: Fixed) -> Result<Fill, Reject> {
        let kind = OrderKind::Market { quoted: Fixed::from_int(MID * 2), max_slippage_bps: None };
        let request = OrderRequest {
            account: access::account(TRADER),
            client_id: 1,
            market: market.id,
            side: Side::Buy,
            qty,
            kind,
            reduce_only: false,
            auth: None,
        };
        let events = self.apply(access::order(&self.engine, TRADER, request));
        let outcome = events.iter().find_map(|event| match event {
            Event::Fill(fill) => Some(Ok(fill.clone())),
            Event::OrderRejected { reason, .. } => Some(Err(reason.clone())),
            _ => None,
        });
        outcome.unwrap_or_else(|| panic!("a fill or a refusal: {events:?}"))
    }
}
