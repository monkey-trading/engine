//! The traders' requests: `load_scenario`'s request mix from the book's named traders, each signed by
//! its account and screened as the API forwards it, with what each trader holds kept from the events
//! its requests produce, so a cancel names an order it rests and a close-all a position it holds.

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::auth::RequestAuth;
use engine_core::command::{CloseAllRequest, CloseEntry, Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::types::{MarketId, Millis, OrderId, Side};

use crate::common::Client;
use crate::common::close_spread::evidence_at;
use crate::common::launch_book::{Rng, market_weights};
use crate::load_scenario::generator::traders::{LIMIT_BUY, LIMIT_SELL, quoted, units};
use crate::load_scenario::plan::{
    CANCELS, LAUNCH_BTC_FILLS_PER_SECOND, LAUNCH_FILLS_PER_SECOND, LIMIT_ORDERS, MARKET_ORDERS, requests_per_second,
};
use crate::sequencer::recovery_scale::flow::Node;

/// BTC-USD, which takes `LAUNCH_BTC_FILLS_PER_SECOND` of launch flow's fills.
const BTC: MarketId = 1;
/// How long a signed request stays valid.
const REQUEST_LIFE_MS: Millis = 60_000;

/// What a trader holds: the orders it rests, newest last, and each market it holds a position in, with
/// the side that closes it.
#[derive(Default)]
struct Holding {
    resting: Vec<OrderId>,
    positions: BTreeMap<MarketId, Side>,
}

/// What one request did: the fills it made, and whether the engine rejected it.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Sent {
    pub fills: u64,
    pub rejected: bool,
}

/// The traders, the draws that schedule and shape their requests, and each market's order size.
pub(super) struct Traders {
    clients: Vec<(Client, Holding)>,
    rng: Rng,
    cursor: usize,
    /// Every market but BTC-USD, by its weight in the cap study's market mix.
    weights: Vec<(MarketId, u64)>,
    units: BTreeMap<MarketId, Fixed>,
}

impl Traders {
    /// The book's named traders `clients`, trading the markets of `engine`.
    pub(super) fn new(clients: Vec<Client>, engine: &engine_core::Engine) -> Self {
        let symbols: Vec<(MarketId, String)> =
            engine.markets().iter().map(|(id, market)| (*id, market.spec.symbol.clone())).collect();
        let weights = market_weights(&symbols).into_iter().filter(|(id, _)| *id != BTC).collect();
        let currencies = engine.markets().iter().map(|(id, market)| (*id, market.spec.quote_currency)).collect();
        let units = units(&currencies);
        let clients = clients.into_iter().map(|client| (client, Holding::default())).collect();
        Traders { clients, rng: Rng::new(0x7265_636f_7665_7279), cursor: 0, weights, units }
    }

    /// The time to the next request of a Poisson process at `multiple` times launch flow's rate.
    pub(super) fn gap_us(&mut self, multiple: f64) -> u64 {
        let uniform = (self.rng.next() >> 11) as f64 / (1u64 << 53) as f64;
        (-(1.0 - uniform).ln() / requests_per_second(multiple) * 1_000_000.0) as u64
    }

    /// Sends the next request of the mix, stamped at `ts`, through `sequence`, and returns what it did.
    pub(super) fn send(
        &mut self,
        node: &mut Node,
        ts: Millis,
        sequence: impl FnOnce(&mut Node, Command) -> Vec<Event>,
    ) -> Sent {
        // A trader orders, and closes, only in a market whose venue is open, as a client can.
        let schedules: BTreeMap<MarketId, bool> =
            node.seq.engine.markets().iter().map(|(id, market)| (*id, market.spec.schedule.is_open(ts))).collect();
        let open = |market: &MarketId| schedules[market];
        let draw = self.rng.below(100);
        let market = self.market(open);
        let side = if self.rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let close_all = self.rng.below(2) == 0;
        // The next trader the request can act on: one resting an order for a cancel or a cancel-all,
        // one holding a position in an open market for a close-all; with none, a market order by the
        // next trader.
        let wants = |holding: &Holding| match draw {
            d if d < MARKET_ORDERS + LIMIT_ORDERS => true,
            d if d < MARKET_ORDERS + LIMIT_ORDERS + CANCELS => !holding.resting.is_empty(),
            _ if close_all => holding.positions.keys().any(open),
            _ => !holding.resting.is_empty(),
        };
        let count = self.clients.len();
        let found = (0..count).map(|step| (self.cursor + step) % count).find(|at| wants(&self.clients[*at].1));
        let (at, draw) = found.map_or((self.cursor, 0), |at| (at, draw));
        self.cursor = (at + 1) % count;
        let (client, holding) = &self.clients[at];
        let expires_at = ts + REQUEST_LIFE_MS;
        let client =
            Client { key: client.key.clone(), account: client.account, domain: client.domain.clone(), expires_at };
        let engine = &node.seq.engine;
        let nonce = engine.next_request_nonce(client.account);
        let auth =
            |hash| RequestAuth { nonce, expires_at: client.expires_at, signature: eip712::sign(&client.key, &hash) };
        let cmd = if draw < MARKET_ORDERS + LIMIT_ORDERS {
            let kind = match (draw < MARKET_ORDERS, side) {
                (true, _) => OrderKind::Market { quoted: quoted(side), max_slippage_bps: None },
                (false, Side::Buy) => OrderKind::Limit { price: Fixed::from_int(LIMIT_BUY) },
                (false, Side::Sell) => OrderKind::Limit { price: Fixed::from_int(LIMIT_SELL) },
            };
            let mut request = OrderRequest {
                account: client.account,
                client_id: nonce,
                market,
                side,
                qty: self.units[&market],
                kind,
                reduce_only: false,
                take_profit: None,
                stop_loss: None,
                auth: None,
            };
            let hash = eip712::order_hash(&client.domain, &request, nonce, client.expires_at);
            request.auth = Some(auth(hash));
            Command::AccessOrder { request, evidence: evidence_at(engine, ts, hash, Some(client.account), None) }
        } else if draw < MARKET_ORDERS + LIMIT_ORDERS + CANCELS {
            let order_id = *holding.resting.last().expect("a trader resting an order");
            client.cancel(nonce, order_id)
        } else if close_all {
            let closes = holding
                .positions
                .iter()
                .filter(|(market, _)| open(market))
                .map(|(market, side)| CloseEntry { market: *market, quoted: quoted(*side) });
            let mut request = CloseAllRequest {
                account: client.account,
                closes: closes.collect(),
                max_slippage_bps: None,
                auth: None,
            };
            let hash = eip712::close_all_hash(&client.domain, &request, nonce, client.expires_at);
            request.auth = Some(auth(hash));
            Command::AccessCloseAll { request, evidence: evidence_at(engine, ts, hash, Some(client.account), None) }
        } else {
            let hash = eip712::cancel_all_hash(&client.domain, client.account, nonce, client.expires_at);
            Command::CancelAll { account: client.account, auth: Some(auth(hash)) }
        };
        let (account, limit) = (client.account, (MARKET_ORDERS..MARKET_ORDERS + LIMIT_ORDERS).contains(&draw));
        let events = sequence(node, cmd);
        let holding = &mut self.clients[at].1;
        let mut sent = Sent::default();
        for event in &events {
            match event {
                // A market order is accepted too, then fills: only a limit rests.
                Event::OrderAccepted { account: by, order_id, .. } if *by == account && limit => {
                    holding.resting.push(*order_id)
                }
                Event::OrderRejected { account: by, .. } if *by == account => sent.rejected = true,
                Event::OrderCancelled { account: by, order_id, .. } if *by == account => {
                    holding.resting.retain(|id| id != order_id)
                }
                Event::Fill(fill) if fill.account == account => {
                    sent.fills += 1;
                    holding.resting.retain(|id| *id != fill.order_id);
                    match fill.position.cmp(&Fixed::ZERO) {
                        std::cmp::Ordering::Equal => holding.positions.remove(&fill.market),
                        std::cmp::Ordering::Greater => holding.positions.insert(fill.market, Side::Sell),
                        std::cmp::Ordering::Less => holding.positions.insert(fill.market, Side::Buy),
                    };
                }
                _ => {}
            }
        }
        sent
    }

    /// A market for an order: BTC-USD, which trades around the clock, at its share of launch flow's
    /// fills, else one drawn by the cap study's market mix from those `open` says are open.
    fn market(&mut self, open: impl Fn(&MarketId) -> bool) -> MarketId {
        let btc = (LAUNCH_BTC_FILLS_PER_SECOND / LAUNCH_FILLS_PER_SECOND * 1_000.0) as u64;
        let weights: Vec<(MarketId, u64)> = self.weights.iter().copied().filter(|(id, _)| open(id)).collect();
        let total: u64 = weights.iter().map(|(_, weight)| weight).sum();
        if self.rng.below(1_000) < btc || total == 0 {
            return BTC;
        }
        let mut at = self.rng.below(total);
        for (id, weight) in &weights {
            match at.checked_sub(*weight) {
                Some(rest) => at = rest,
                None => return *id,
            }
        }
        unreachable!("a draw below the total lands on a market")
    }
}
