//! The load generator: the run's clients, in a process of their own, so their signing, TLS and frame
//! reading take nothing from the node's figures. The node's side starts this test binary again with
//! `LOAD_PLAN` holding the plan (`plan::Plan`), reads each line it prints after `LOADGEN `, and
//! answers each leg's end with `next` or `stop` on its standard input.
//!
//! Each trader is an account of the book with a session key it registers first. Traders share client
//! addresses, a connection each, which the node takes through its trusted PROXY protocol peer, so its
//! per-client limits count each address. The schedule is open-loop: requests arrive as a Poisson
//! process at the leg's rate, each handed to a trader free to send it, whether or not earlier replies
//! have come, and a request's time is counted from when it was due, so a stalled node shows as
//! latency, not as a lower rate. No trader sends more than `plan::ACCOUNT_RATE` requests a second.

mod client;
mod record;
mod streams;
mod traders;

use std::collections::BTreeMap;
use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use engine_core::market::QuoteCurrency;
use engine_core::types::{MarketId, Side};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinSet;
use tokio::time::sleep_until;

use crate::common::launch_book::{Rng, market_weights, production_harness};
use crate::load_scenario::plan::{
    ACCOUNT_RATE, CANCELS, LAUNCH_BTC_FILLS_PER_SECOND, LAUNCH_FILLS_PER_SECOND, LIMIT_ORDERS, Leg, Line,
    MARKET_ORDERS, Plan, requests_per_second,
};
use record::{Recorder, Samples, take_all};
use streams::Subscription;
use traders::{Client, Holding, Job, Kind, Trader, units};

/// The generator's runtime threads.
const WORKERS: usize = 4;
/// BTC-USD, which takes `LAUNCH_BTC_FILLS_PER_SECOND` of launch flow's fills.
const BTC: MarketId = 1;
/// How long a leg's replies may take to come in after its last request.
const DRAIN: Duration = Duration::from_secs(60);

/// A trader the scheduler can hand a request: its client, its place there, what it holds, and when
/// it may next send.
struct Slot {
    client: usize,
    trader: usize,
    holding: Arc<std::sync::Mutex<Holding>>,
    free_at: Instant,
}

/// The open-loop schedule's state.
struct Scheduler {
    rng: Rng,
    slots: Vec<Slot>,
    cursor: usize,
    /// Every market but BTC-USD, by its weight in the cap study's market mix.
    weights: Vec<(MarketId, u64)>,
    jobs: Vec<UnboundedSender<Job>>,
    in_flight: Arc<AtomicU64>,
}

#[test]
#[ignore = "the load generator, which load_scenario::dev_scenario_load starts in a process of its own"]
fn load_generator() {
    let Ok(plan) = std::env::var("LOAD_PLAN") else { return };
    let plan: Plan = serde_json::from_str(&plan).expect("the plan");
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(WORKERS).enable_all().build().unwrap();
    runtime.block_on(run(plan));
}

/// Prints `line` for the node's side.
fn report(line: &Line) {
    println!("LOADGEN {}", serde_json::to_string(line).expect("a line encodes"));
}

/// The address of the `index`th client of a kind: traders' clients from 10.1.0.1, subscribers' from
/// 10.2.0.1.
fn address(kind: u8, index: u32) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, kind, (index / 250) as u8, (index % 250 + 1) as u8))
}

/// Runs the plan.
async fn run(plan: Plan) {
    let harness = production_harness();
    let markets: BTreeMap<MarketId, QuoteCurrency> =
        harness.engine.markets().iter().map(|(id, market)| (*id, market.spec.quote_currency)).collect();
    let symbols: Vec<(MarketId, String)> =
        harness.engine.markets().iter().map(|(id, market)| (*id, market.spec.symbol.clone())).collect();
    let weights: Vec<(MarketId, u64)> = market_weights(&symbols).into_iter().filter(|(id, _)| *id != BTC).collect();
    let units = Arc::new(units(&markets));
    let tls = client::tls_config();

    // The traders, a client address for every `accounts_per_client` of them, each session key
    // registered.
    let numbers: Vec<u32> = (plan.first_key..plan.first_key + plan.traders).collect();
    let mut clients: Vec<Client> = numbers
        .chunks(plan.accounts_per_client as usize)
        .enumerate()
        .map(|(index, chunk)| Client {
            api: plan.api,
            address: address(1, index as u32),
            tls: tls.clone(),
            traders: chunk.iter().map(|number| Trader::new(*number)).collect(),
            units: units.clone(),
            recorder: Recorder::default(),
        })
        .collect();
    let mut registering = JoinSet::new();
    for mut client in clients.drain(..) {
        registering.spawn(async move { client.register().await.map(|()| client) });
    }
    while let Some(registered) = registering.join_next().await {
        clients.push(registered.expect("a registration task").expect("every session key registers"));
    }
    clients.sort_by_key(|client| client.address);

    // The subscribers: every other one signed, and a share of those quoting.
    let accounts: Vec<_> =
        clients.iter().flat_map(|client| client.traders.iter().map(|trader| trader.account)).collect();
    let quoted: Vec<MarketId> = {
        let mut by_weight = market_weights(&symbols);
        by_weight.sort_by_key(|(_, weight)| std::cmp::Reverse(*weight));
        by_weight.into_iter().take(plan.quote_markets).map(|(id, _)| id).collect()
    };
    let mut stream_recorders = Vec::new();
    let signed = plan.subscribers.div_ceil(2);
    let quoting = signed * plan.quoting_per_cent / 100;
    for index in 0..plan.subscribers {
        let recorder = Recorder::default();
        stream_recorders.push(recorder.clone());
        let signs = index % 2 == 0;
        let challenge = signs.then(|| {
            let mut challenge = [0x5a; 32];
            challenge[28..].copy_from_slice(&index.to_be_bytes());
            challenge
        });
        let quotes =
            (signs && index / 2 < quoting).then(|| (accounts[index as usize % accounts.len()], quoted.clone()));
        let subscription = Subscription { signed: challenge, quotes };
        tokio::spawn(streams::subscribe(plan.api, address(2, index), tls.clone(), subscription, recorder));
    }

    // Each client sends what the scheduler hands it.
    let in_flight = Arc::new(AtomicU64::new(0));
    let mut slots = Vec::new();
    let mut jobs = Vec::new();
    let mut client_recorders = Vec::new();
    for (index, client) in clients.into_iter().enumerate() {
        for (trader, held) in client.traders.iter().enumerate() {
            slots.push(Slot { client: index, trader, holding: held.holding.clone(), free_at: Instant::now() });
        }
        client_recorders.push(client.recorder.clone());
        let (tx, rx) = mpsc::unbounded_channel();
        jobs.push(tx);
        tokio::spawn(client.run(rx, in_flight.clone()));
    }
    let mut scheduler = Scheduler { rng: Rng::new(906), slots, cursor: 0, weights, jobs, in_flight };
    tokio::time::sleep(Duration::from_secs(3)).await;
    take_all(&client_recorders);
    take_all(&stream_recorders);
    report(&Line::Ready { traders: plan.traders, subscribers: plan.subscribers });

    let recorders: Vec<Recorder> = client_recorders.into_iter().chain(stream_recorders).collect();
    let mut stdin = std::io::stdin().lock().lines();
    for leg in plan.steps() {
        report(&Line::Start { leg: leg.name.clone(), multiple: leg.multiple });
        let figures = scheduler.drive(&leg, plan.windows, &recorders).await;
        report(&Line::End { leg: leg.name.clone(), multiple: leg.multiple, figures: figures.figures() });
        match stdin.next() {
            Some(Ok(answer)) if answer.trim() == "next" => {}
            _ => break,
        }
    }
}

impl Scheduler {
    /// Runs `leg`'s schedule, reporting each of its `windows` as it ends, and returns the leg's
    /// samples once its replies are in, or `DRAIN` has passed.
    async fn drive(&mut self, leg: &Leg, windows: u32, recorders: &[Recorder]) -> Samples {
        let rate = requests_per_second(leg.multiple);
        let start = Instant::now();
        let end = start + Duration::from_secs(leg.secs);
        let window = Duration::from_secs(leg.secs) / windows.max(1);
        let (mut due, mut index, mut next_window) = (start, 0, start + window);
        let mut all = Samples::default();
        let close = |index: u32, all: &mut Samples| {
            let samples = take_all(recorders);
            report(&Line::Window { leg: leg.name.clone(), index, figures: samples.figures() });
            all.merge(samples);
        };
        loop {
            let now = Instant::now();
            if now >= next_window && index + 1 < windows {
                close(index, &mut all);
                (index, next_window) = (index + 1, next_window + window);
                continue;
            }
            if due < end && due <= now {
                self.dispatch(due);
                due += Duration::from_secs_f64(self.gap(rate));
                continue;
            }
            if now >= end && due >= end {
                break;
            }
            sleep_until(due.min(next_window).min(end).into()).await;
        }
        let drained = Instant::now() + DRAIN;
        while self.in_flight.load(Ordering::Acquire) > 0 && Instant::now() < drained {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        close(index, &mut all);
        all
    }

    /// The time to the next arrival of a Poisson process at `rate` a second.
    fn gap(&mut self, rate: f64) -> f64 {
        let uniform = (self.rng.next() >> 11) as f64 / (1u64 << 53) as f64;
        -(1.0 - uniform).ln() / rate
    }

    /// Hands the request due at `due` to a trader free to send it.
    fn dispatch(&mut self, due: Instant) {
        let kind = match self.rng.below(100) {
            draw if draw < MARKET_ORDERS => Kind::Market,
            draw if draw < MARKET_ORDERS + LIMIT_ORDERS => Kind::Limit,
            draw if draw < MARKET_ORDERS + LIMIT_ORDERS + CANCELS => Kind::Cancel,
            _ if self.rng.below(2) == 0 => Kind::CancelAll,
            _ => Kind::CloseAll,
        };
        let market = self.market();
        let side = if self.rng.below(2) == 0 { Side::Buy } else { Side::Sell };
        let (slot, kind) = self.pick(kind, due);
        let slot = &mut self.slots[slot];
        slot.free_at = due.max(slot.free_at) + Duration::from_secs_f64(1.0 / ACCOUNT_RATE);
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let job = Job { trader: slot.trader, kind, market, side, due, dispatched: Instant::now().max(due) };
        if self.jobs[slot.client].send(job).is_err() {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// A market for an order: BTC-USD at its share of launch flow's fills, else one drawn by the cap
    /// study's market mix.
    fn market(&mut self) -> MarketId {
        let btc = (LAUNCH_BTC_FILLS_PER_SECOND / LAUNCH_FILLS_PER_SECOND * 1_000.0) as u64;
        if self.rng.below(1_000) < btc {
            return BTC;
        }
        let total: u64 = self.weights.iter().map(|(_, weight)| weight).sum();
        let mut at = self.rng.below(total);
        for (id, weight) in &self.weights {
            match at.checked_sub(*weight) {
                Some(rest) => at = rest,
                None => return *id,
            }
        }
        unreachable!("a draw below the total lands on a market")
    }

    /// The next trader in turn free at `due` that `kind` can act on: one resting an order for a cancel
    /// or a cancel-all, one holding a position for a close-all. With none, a market order by the next
    /// free trader; with no trader free, the one free soonest.
    fn pick(&mut self, kind: Kind, due: Instant) -> (usize, Kind) {
        let count = self.slots.len();
        let acts = |slot: &Slot, kind: Kind| {
            let holding = slot.holding.lock().expect("a holding");
            match kind {
                Kind::Cancel | Kind::CancelAll => !holding.resting.is_empty(),
                Kind::CloseAll => !holding.positions.is_empty(),
                Kind::Market | Kind::Limit => true,
            }
        };
        for kind in [kind, Kind::Market] {
            let found = (0..count)
                .map(|step| (self.cursor + step) % count)
                .find(|at| self.slots[*at].free_at <= due && acts(&self.slots[*at], kind));
            if let Some(at) = found {
                self.cursor = (at + 1) % count;
                return (at, kind);
            }
        }
        let soonest = (0..count).min_by_key(|at| self.slots[*at].free_at).expect("a trader");
        (soonest, Kind::Market)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trader_gets_only_what_it_can_act_on_and_no_more_than_its_rate() {
        // Two clients of two traders each; only the second client's first trader rests an order.
        let holdings: Vec<Arc<std::sync::Mutex<Holding>>> = (0..4).map(|_| Arc::default()).collect();
        holdings[2].lock().unwrap().resting.push(7);
        let start = Instant::now();
        let slots = (0..4)
            .map(|at| Slot { client: at / 2, trader: at % 2, holding: holdings[at].clone(), free_at: start })
            .collect();
        let (channels, mut received): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::unbounded_channel()).unzip();
        let in_flight = Arc::new(AtomicU64::new(0));
        let weights = vec![(2, 1)];
        let mut scheduler = Scheduler { rng: Rng::new(1), slots, cursor: 0, weights, jobs: channels, in_flight };
        // Five requests a second among four traders, each free to send one every 1 / ACCOUNT_RATE.
        for step in 0..200 {
            scheduler.dispatch(start + Duration::from_millis(200 * step));
        }
        assert_eq!(scheduler.in_flight.load(Ordering::Acquire), 200);
        let mut last: BTreeMap<(usize, usize), Instant> = BTreeMap::new();
        let (mut cancels, mut markets) = (0, 0);
        for (client, jobs) in received.iter_mut().enumerate() {
            while let Ok(job) = jobs.try_recv() {
                if let Some(before) = last.insert((client, job.trader), job.due) {
                    let spacing = job.due.duration_since(before).as_secs_f64();
                    assert!(
                        spacing >= 1.0 / ACCOUNT_RATE - 1e-9,
                        "trader {client}/{} sent {spacing}s apart",
                        job.trader
                    );
                }
                match job.kind {
                    Kind::Cancel | Kind::CancelAll => {
                        cancels += 1;
                        assert_eq!((client, job.trader), (1, 0), "only the trader resting an order cancels");
                    }
                    Kind::CloseAll => panic!("no trader holds a position to close"),
                    Kind::Market => markets += 1,
                    Kind::Limit => {}
                }
            }
        }
        assert!(cancels > 0 && markets > 0, "{cancels} cancels, {markets} market orders");
    }
}
