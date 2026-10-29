use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use engine_core::{Engine, Fixed};
use tokio::task::JoinHandle;
use tokio::time::{advance, timeout};

use crate::sequencer::genesis_commands;
use crate::test_support::dev_config;

pub(super) const T0: Millis = 1_792_504_800_000;
const BTC: MarketId = 1;
const INTERVAL: Duration = Duration::from_millis(100);
pub(super) const TRADER: AccountId = AccountId([7; 20]);
/// An account with 150 USD.
pub(super) const SMALL: AccountId = AccountId([8; 20]);

/// A subscription over a stand-in sequencer that answers queries from an engine the test changes,
/// and counts them.
struct Rig {
    quotes: Quotes,
    /// The channel's only strong sender, as the node's API state holds it.
    tx: Option<mpsc::Sender<sequencer::Request>>,
    sequencer: JoinHandle<()>,
    /// Closed, the stand-in sequencer holds each query until it opens.
    open: watch::Sender<bool>,
    inbox: watch::Sender<String>,
    head: watch::Sender<Manifest>,
    engine: Arc<Mutex<Engine>>,
    queries: Arc<AtomicUsize>,
    /// Milliseconds the stand-in sequencer takes to answer a query.
    latency: Arc<AtomicU64>,
    seq: u64,
}

impl Rig {
    fn new() -> Self {
        Self::at(Duration::ZERO)
    }

    /// A subscription that reads `phase` after it starts, and every interval after.
    fn at(phase: Duration) -> Self {
        let engine = Arc::new(Mutex::new(engine()));
        let queries = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel(4);
        let latency = Arc::new(AtomicU64::new(0));
        let (served, counted, delay) = (engine.clone(), queries.clone(), latency.clone());
        let (open, mut opened) = watch::channel(true);
        let sequencer = tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let sequencer::Request::Query(query) = request else { panic!("only queries") };
                counted.fetch_add(1, Ordering::SeqCst);
                opened.wait_for(|open| *open).await.expect("the rig outlives its sequencer");
                tokio::time::sleep(Duration::from_millis(delay.load(Ordering::SeqCst))).await;
                query(&served.lock().unwrap());
            }
        });
        let (inbox, requests) = watch::channel(String::new());
        let (head, manifest) = watch::channel(Manifest::default());
        let quotes = Quotes::new(tx.downgrade(), manifest, requests, INTERVAL, 2, phase);
        Self { quotes, tx: Some(tx), sequencer, open, inbox, head, engine, queries, latency, seq: 0 }
    }

    fn send(&self, text: &str) {
        self.inbox.send_replace(text.to_owned());
    }

    fn subscribe(&self, markets: &str) {
        self.subscribe_as(TRADER, markets);
    }

    fn subscribe_as(&self, account: AccountId, markets: &str) {
        self.send(&format!(r#"{{"op":"quotes","account":"{account}","markets":{markets}}}"#));
    }

    /// Changes the engine and moves the log head, as a sequenced command does.
    fn sequence(&mut self, cmd: Command) {
        let mut engine = self.engine.lock().unwrap();
        let seq = engine.last_seq().map_or(0, |seq| seq + 1);
        engine.apply(&Sequenced { seq, ts: T0 + seq as i64, cmd }).unwrap();
        drop(engine);
        self.move_head();
    }

    fn move_head(&mut self) {
        self.seq += 1;
        self.head.send_modify(|manifest| manifest.flushed = Some(Offset::start_of(self.seq)));
    }

    /// The messages the next poll yields, or none if it yields nothing within `wait`.
    async fn within(&mut self, wait: Duration) -> Option<Vec<serde_json::Value>> {
        let texts = timeout(wait, self.quotes.next()).await.ok()?;
        Some(texts.iter().map(|text| serde_json::from_str(text).unwrap()).collect())
    }

    fn queries(&self) -> usize {
        self.queries.load(Ordering::SeqCst)
    }
}

/// The engine as a node sequences genesis, with BTC priced and `TRADER` funded.
pub(super) fn engine() -> Engine {
    engine_with(0)
}

/// `engine`, with `extra` more markets listed, each a copy of the first and priced.
pub(super) fn engine_with(extra: u32) -> Engine {
    let cfg = dev_config();
    let mut engine = Engine::new();
    let activation = [
        AdminCommand::SetClientAuth(cfg.client_auth().unwrap()),
        AdminCommand::SetOperator(cfg.operator().unwrap()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 },
    ];
    let copies = (1000..).take(extra as usize).map(|id| {
        let template = cfg.genesis.markets[0].clone();
        Command::Admin(AdminCommand::UpsertMarket(engine_core::market::MarketSpec {
            id,
            symbol: format!("COPY-{id}"),
            ..template
        }))
    });
    // The operator signs admin commands once funds are credited, so the copies come before genesis's
    // house capital, its last command.
    let mut genesis = genesis_commands(&cfg.genesis);
    let capital = genesis.pop().expect("genesis credits house capital");
    let commands = activation.into_iter().map(Command::Admin).chain(genesis).chain(copies).chain([capital]);
    let commands = commands
        .chain([("0xd", TRADER, 50_000_000), ("0xs", SMALL, 150)].map(|(chain_ref, account, amount)| {
            Command::Deposit { account, amount: Fixed::from_int(amount), chain_ref: chain_ref.into() }
        }))
        .chain([prices(100_000)]);
    for (seq, cmd) in commands.enumerate() {
        engine.apply(&Sequenced { seq: seq as u64, ts: T0, cmd }).unwrap();
    }
    engine
}

fn prices(mid: i64) -> Command {
    let quotes = ["a", "b"]
        .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(mid), ts: T0, contract: None })
        .to_vec();
    Command::Prices { market: BTC, quotes }
}

#[tokio::test(start_paused = true)]
async fn a_subscription_gets_every_market_it_names_then_only_what_changed() {
    let mut rig = Rig::new();
    rig.subscribe("[2,1,1]");
    let first = rig.within(INTERVAL).await.expect("the first quotes");
    // One message per distinct market, in market order.
    let markets: Vec<_> = first.iter().map(|quote| quote["market"].as_u64().unwrap()).collect();
    assert_eq!(markets, [1, 2]);
    let btc = &first[0];
    assert_eq!(btc["type"], "quote");
    assert_eq!(btc["open"], true);
    // ETH has no price yet.
    assert_eq!(first[1]["unavailable"]["kind"], "no_price");
    assert_eq!(first[1]["ask"], serde_json::json!([]));
    let steps = btc["ask"].as_array().unwrap();
    // A step per spread tier top (20 and 100 BTC), and between them the most the house's exposure
    // cap lets the account buy (30% of 10m is 30 BTC), which the 100 BTC step is declined for.
    let reach: Vec<Fixed> = steps.iter().map(|step| step["qty"].as_str().unwrap().parse().unwrap()).collect();
    assert_eq!((reach.len(), reach[0]), (3, Fixed::from_int(20)), "{btc}");
    assert!(reach[1] > Fixed::from_int(30) - Fixed::from_raw(100_000) && reach[1] <= Fixed::from_int(30), "{btc}");
    assert_eq!(reach[2], Fixed::from_int(100));
    assert!(steps[..2].iter().all(|step| step.get("declined").is_none()), "{btc}");
    assert_eq!(steps[2]["declined"]["kind"], "house_capacity");
    assert_eq!(btc["account"], TRADER.to_string());
    let ask_prices: Vec<Fixed> = steps.iter().map(|step| step["price"].as_str().unwrap().parse().unwrap()).collect();
    assert!(ask_prices[0] < ask_prices[1] && ask_prices[1] <= ask_prices[2], "{ask_prices:?}");
    assert_eq!(btc["close"], serde_json::Value::Null);

    // Nothing moved: the head moving reads again but sends nothing.
    rig.move_head();
    assert_eq!(rig.within(Duration::from_secs(1)).await, None);
    assert_eq!(rig.queries(), 2);

    // The index moves: only BTC is sent again, at the new mid.
    rig.sequence(prices(101_000));
    let second = rig.within(Duration::from_secs(1)).await.expect("a changed quote");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["market"], 1);
    assert_ne!(second[0]["ask"], btc["ask"]);
}

#[tokio::test(start_paused = true)]
async fn quotes_are_read_only_when_the_head_moves_and_never_twice_within_an_interval() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("the first quotes");
    assert_eq!(rig.queries(), 1);

    // No head movement, no reads, however long it is quiet.
    assert_eq!(rig.within(Duration::from_secs(60)).await, None);
    assert_eq!(rig.queries(), 1);

    // A burst of changes inside one interval is one read, and its message waits for the interval.
    let moved = tokio::time::Instant::now();
    for mid in [101_000, 102_000, 103_000] {
        rig.sequence(prices(mid));
    }
    let burst = rig.within(Duration::from_secs(1)).await.expect("the changed quote");
    assert_eq!(rig.queries(), 2);
    assert_eq!(burst[0]["mark"], "103000");
    let sent = tokio::time::Instant::now();
    assert!(sent - moved <= INTERVAL, "a read after quiet is not delayed");

    // The next change is held until an interval after the last message.
    rig.sequence(prices(104_000));
    assert_eq!(rig.within(INTERVAL - Duration::from_millis(1)).await, None);
    assert!(rig.within(Duration::from_millis(2)).await.is_some());
    assert!(tokio::time::Instant::now() - sent >= INTERVAL);
}

#[tokio::test(start_paused = true)]
async fn pushes_never_come_closer_than_the_interval_while_the_index_moves_constantly() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("the first quotes");
    let mut sent = vec![tokio::time::Instant::now()];
    for round in 0..20 {
        rig.sequence(prices(101_000 + round));
        advance(Duration::from_millis(7)).await;
        if rig.within(Duration::from_millis(1)).await.is_some() {
            sent.push(tokio::time::Instant::now());
        }
    }
    assert!(sent.len() > 1, "{sent:?}");
    assert!(sent.windows(2).all(|pair| pair[1] - pair[0] >= INTERVAL), "{sent:?}");
}

#[tokio::test(start_paused = true)]
async fn a_changing_index_is_pushed_every_interval_whatever_a_read_takes() {
    for latency in [1, 5, 50, 99] {
        let mut rig = Rig::new();
        rig.latency.store(latency, Ordering::SeqCst);
        rig.subscribe("[1]");
        rig.within(INTERVAL * 2).await.expect("the first quotes");
        let mut sent = vec![tokio::time::Instant::now()];
        for round in 0..10 {
            // The index has moved again by the time the last message is out.
            rig.sequence(prices(101_000 + round));
            rig.within(INTERVAL * 3).await.expect("a changed quote");
            sent.push(tokio::time::Instant::now());
        }
        let gaps: Vec<_> = sent.windows(2).map(|pair| pair[1] - pair[0]).collect();
        // Never closer than an interval, and not an interval late for each read that takes a moment.
        assert!(
            gaps.iter().all(|gap| *gap >= INTERVAL && *gap <= INTERVAL + Duration::from_millis(latency)),
            "{latency}: {gaps:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_account_with_a_position_gets_a_closing_price() {
    let mut rig = Rig::new();
    rig.sequence(Command::PlaceOrder(engine_core::command::OrderRequest {
        account: TRADER,
        client_id: 1,
        market: BTC,
        side: engine_core::types::Side::Buy,
        qty: Fixed::ONE,
        kind: engine_core::command::OrderKind::Market { quoted: Fixed::from_int(100_000), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    }));
    rig.subscribe("[1]");
    let quote = rig.within(INTERVAL).await.expect("quotes").remove(0);
    // The first tier ends at 20 BTC of position: 19 more to buy, and 1 to close plus 20 to sell.
    assert_eq!(quote["ask"][0]["qty"], "19");
    assert_eq!(quote["bid"][0]["qty"], "21");
    assert_eq!(quote["close"]["side"], "sell");
    assert_eq!(quote["close"]["qty"], "1");
    assert!(quote["close"]["price"].as_str().is_some());
}

#[tokio::test(start_paused = true)]
async fn what_it_cannot_quote_is_refused_in_a_message_and_ends_the_subscription() {
    let mut rig = Rig::new();
    for (text, error) in [
        (
            r#"{"op":"quotes","account":"0x0909090909090909090909090909090909090909","markets":[1]}"#.to_owned(),
            "never been credited",
        ),
        (format!(r#"{{"op":"quotes","account":"{TRADER}","markets":[404]}}"#), "unknown market 404"),
        (format!(r#"{{"op":"quotes","account":"{TRADER}","markets":[1,2,3]}}"#), "at most 2 markets"),
        ("not json".to_owned(), "not a quotes op"),
        (r#"{"op":"orders"}"#.to_owned(), "not a quotes op"),
        (format!(r#"{{"op":"quotes","account":"{TRADER}","markets":[1],"extra":1}}"#), "not a quotes op"),
    ] {
        rig.send(&text);
        let refusal = rig.within(Duration::from_secs(1)).await.expect(&text).remove(0);
        assert_eq!(refusal["type"], "quote_error");
        assert!(refusal["error"].as_str().unwrap().contains(error), "{refusal}");
    }
    // None of them left a subscription behind.
    rig.move_head();
    assert_eq!(rig.within(Duration::from_secs(5)).await, None);
}

#[tokio::test(start_paused = true)]
async fn asking_again_replaces_the_subscription_and_an_empty_list_ends_it() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    assert_eq!(rig.within(INTERVAL).await.expect("quotes").len(), 1);
    // The same ask sends the quotes again, as the client may have lost them.
    rig.subscribe("[1]");
    assert_eq!(rig.within(Duration::from_secs(1)).await.expect("quotes again").len(), 1);
    rig.subscribe("[]");
    rig.move_head();
    assert_eq!(rig.within(Duration::from_secs(5)).await, None);
    let reads = rig.queries();
    rig.move_head();
    assert_eq!(rig.within(Duration::from_secs(5)).await, None);
    assert_eq!(rig.queries(), reads);
}

#[tokio::test(start_paused = true)]
async fn a_small_accounts_ladder_ends_at_the_largest_size_it_can_open() {
    let mut rig = Rig::new();
    rig.subscribe_as(SMALL, "[1]");
    let quote = rig.within(INTERVAL).await.expect("quotes").remove(0);
    // 150 USD of equity at 10% margin opens 0.015 BTC of 100,000, though the first tier ends at 20.
    for side in ["ask", "bid"] {
        let steps = quote[side].as_array().unwrap();
        let fitting: Vec<_> = steps.iter().filter(|step| step.get("declined").is_none()).collect();
        let largest: Fixed = fitting.last().expect("a size it can open")["qty"].as_str().unwrap().parse().unwrap();
        assert!(largest > Fixed::from_raw(14_990_000) && largest <= Fixed::from_raw(15_000_000), "{side}: {quote}");
        // The tier tops past it are priced and declined for margin.
        assert!(steps[fitting.len()..].iter().all(|step| step["declined"]["kind"] == "insufficient_margin"), "{quote}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_replaced_subscription_ignores_the_reply_to_the_one_before() {
    let mut rig = Rig::new();
    rig.open.send_replace(false);
    // A never-credited account's read is in flight, held by the sequencer, when the client corrects it.
    rig.subscribe_as(AccountId([9; 20]), "[1]");
    assert_eq!(rig.within(INTERVAL).await, None);
    assert_eq!(rig.queries(), 1);
    rig.subscribe("[1]");
    rig.open.send_replace(true);
    let quotes = rig.within(Duration::from_secs(1)).await.expect("the new subscription's quotes");
    assert_eq!(quotes.len(), 1);
    assert_eq!(
        (quotes[0]["type"].as_str(), quotes[0]["account"].as_str()),
        (Some("quote"), Some(TRADER.to_string().as_str()))
    );
    // And an ended subscription gets nothing from a read still in flight.
    rig.open.send_replace(false);
    rig.move_head();
    assert_eq!(rig.within(Duration::from_secs(1)).await, None);
    rig.subscribe("[]");
    rig.open.send_replace(true);
    assert_eq!(rig.within(Duration::from_secs(5)).await, None);
}

#[tokio::test(start_paused = true)]
async fn an_open_subscription_does_not_keep_the_sequencers_channel_open() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("quotes");
    drop(rig.tx.take());
    // With the API's senders gone the sequencer drains, whatever streams are open.
    timeout(Duration::from_secs(1), &mut rig.sequencer).await.expect("the sequencer drains").unwrap();
    rig.move_head();
    assert_eq!(rig.within(Duration::from_secs(5)).await, None);
}

#[tokio::test(start_paused = true)]
async fn a_client_that_floods_refused_ops_gets_one_refusal_per_interval() {
    let mut rig = Rig::new();
    let mut refusals = 0;
    for _ in 0..30 {
        rig.send("x");
        if let Some(messages) = rig.within(Duration::from_millis(10)).await {
            assert_eq!(messages[0]["type"], "quote_error");
            refusals += messages.len();
        }
    }
    // Thirty tries over 300 ms: one at once, then one an interval.
    assert!((3..=4).contains(&refusals), "{refusals}");
}

#[tokio::test(start_paused = true)]
async fn a_refused_op_ends_the_subscription_before_it() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("quotes");
    rig.subscribe("[1,2,3]");
    let refusal = rig.within(Duration::from_secs(1)).await.expect("a refusal").remove(0);
    assert_eq!(refusal["type"], "quote_error");
    rig.sequence(prices(101_000));
    assert_eq!(rig.within(Duration::from_secs(5)).await, None);
}

#[tokio::test(start_paused = true)]
async fn a_connection_reads_only_at_its_own_phase_of_the_interval() {
    let mut rig = Rig::at(Duration::from_millis(30));
    let began = tokio::time::Instant::now();
    rig.subscribe("[1]");
    assert_eq!(rig.within(Duration::from_millis(29)).await, None);
    assert!(rig.within(Duration::from_millis(2)).await.is_some());
    assert_eq!(began.elapsed(), Duration::from_millis(30));
    // A change at 150 ms, past the gate, waits for the connection's next slot, 230 ms in.
    advance(Duration::from_millis(120)).await;
    rig.sequence(prices(101_000));
    assert_eq!(rig.within(Duration::from_millis(79)).await, None);
    assert!(rig.within(Duration::from_millis(2)).await.is_some());
    assert_eq!(began.elapsed(), Duration::from_millis(230));
}

#[test]
fn the_largest_size_search_keeps_a_known_size_nothing_larger_fits_and_finds_one_far_below_the_top() {
    let floor = Fixed::from_raw(100_000);
    let up_to = |limit: Fixed| move |qty: Fixed| (qty <= limit).then_some(qty);
    let size = |step: Option<ladder::Step>| step.map(|step| step.qty);
    // A known size of 2 against 4: the doubling's last success is the answer, as the limit is 2 exactly.
    assert_eq!(
        size(ladder::largest(&up_to(Fixed::from_int(2)), floor, Fixed::from_int(2), Fixed::from_int(4))),
        Some(Fixed::from_int(2))
    );
    // Nothing known against a top 20,000 times the limit: halved to it, then bisected to within a thousandth of it.
    let limit = Fixed::from_raw(1_000_000);
    let found = size(ladder::largest(&up_to(limit), floor, Fixed::ZERO, Fixed::from_int(20))).expect("a size");
    assert!(found <= limit && found > limit - Fixed::from_raw(1_000), "{found}");
    // No size at or above the floor fits.
    assert_eq!(
        size(ladder::largest(&up_to(floor - Fixed::from_raw(1)), floor, Fixed::ZERO, Fixed::from_int(20))),
        None
    );
    // Sizes below a minimum are refused too, so the search starts at it and does not halve past it.
    let between = |least: Fixed, most: Fixed| move |qty: Fixed| (qty >= least && qty <= most).then_some(qty);
    let (least, most) = (Fixed::from_raw(40_000_000), Fixed::from_raw(50_000_000));
    let found = size(ladder::largest(&between(least, most), least, Fixed::ZERO, Fixed::from_int(20))).expect("a size");
    assert!(found <= most && found > most - Fixed::from_raw(100_000), "{found}");
}

/// Counts how often a future wakes itself, which a loop that never waits does.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl std::task::Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Polls `rig`'s quotes once, and returns how many times they woke themselves. A loop that never
/// waits runs out of the runtime's budget, which defers its wake to when the runtime next yields.
async fn poll_once(rig: &mut Rig) -> usize {
    let wakes = Arc::new(Wakes::default());
    let waker = std::task::Waker::from(wakes.clone());
    let mut next = Box::pin(rig.quotes.next());
    assert!(next.as_mut().poll(&mut std::task::Context::from_waker(&waker)).is_pending());
    drop(next);
    tokio::task::yield_now().await;
    wakes.0.load(Ordering::SeqCst)
}

#[tokio::test(start_paused = true)]
async fn a_full_sequencer_queue_leaves_the_read_for_the_next_interval() {
    let mut rig = Rig::new();
    rig.open.send_replace(false);
    let tx = rig.tx.clone().unwrap();
    // The stand-in holds one query and the channel four more.
    while tx.try_send(sequencer::Request::Query(Box::new(|_: &Engine| {}))).is_ok() {}
    rig.subscribe("[1]");
    // The first poll takes the op and finds the read due; the read itself falls due a moment later.
    let before = tokio::time::Instant::now();
    poll_once(&mut rig).await;
    advance(Duration::from_millis(1)).await;
    assert_eq!(poll_once(&mut rig).await, 0, "the read does not retry in a loop");
    // The next try waits for the next interval, not for the slot that just fired.
    assert!(rig.quotes.next_read >= before + INTERVAL && rig.quotes.read_slot >= Some(before + INTERVAL));
    // Once the queue drains, the read goes out and its quotes arrive.
    rig.open.send_replace(true);
    assert!(rig.within(Duration::from_secs(1)).await.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_stopped_log_leaves_an_idle_subscription_waiting_rather_than_spinning() {
    let mut rig = Rig::new();
    rig.subscribe("[1]");
    rig.within(INTERVAL).await.expect("quotes");
    // Idle: caught up with the log, whose writer then goes away.
    drop(std::mem::replace(&mut rig.head, watch::channel(Manifest::default()).0));
    assert_eq!(poll_once(&mut rig).await, 0);
}
