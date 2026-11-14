//! The toxicity score's dev scenario offline, as `scripts/dev-toxicity.sh` runs it: loopback
//! Coinbase and Kraken stand-ins price BTC-USD through a sequencer at the latest protocol, under the
//! dev genesis, whose BTC-USD markout floor is a labelled 1 bp fixture.
//! 1. After each of a toxic client's 25 buys, the stand-ins move their books 1 bp its way. No grid
//!    interval holds more than one move, so the market's markout spread stays at the floor and each
//!    markout scores one standard error: the client pays nothing until 20 fills have scored, and
//!    then the add-on of the tier the test computes from the moves.
//! 2. An edgeless client fills 25 times on sides a fixed shuffle picks, while the books wander a bp
//!    up and down on their own, and stays at the best price.
//! 3. The toxic client makes round trips at the index between moves, which leave its score at its
//!    tier.
//! 4. The node stops, its log replays to every root, and a restart recovers the same score.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::{Event, Fill};
use engine_core::fixed::Rounding;
use engine_core::toxicity::{MIN_FILLS, ToxicityStanding};
use engine_core::types::{MarketId, Side};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::{sleep, timeout};
use tokio_tungstenite::accept_async;

use crate::clock::now_ms;
use crate::config::Venue;
use crate::feeds::tests::fake::{WAIT, coinbase_levels, fast, kraken_book, listen, next_text, spawn_feed_into, stream};
use crate::log::Manifest;
use crate::replay_path;
use crate::sequencer::{Request, Sequencer, SequencerConfig, StreamMsg};
use crate::tee::EnclaveKey;
use crate::test_support::access::{account, declaration, funding, order, policy};
use crate::test_support::{dev_config, scratch_dir, sequencer_config, trusted_now};

const BTC: MarketId = 1;
/// The access seeds of the toxic and the edgeless client.
const TOXIC: u8 = 1;
const EDGELESS: u8 = 2;
/// The dev genesis's BTC-USD markout floor, a test fixture: one basis point.
const FLOOR_BPS: i64 = 1;
/// The spread every fill here pays before its toxicity add-on: the dev genesis's first BTC-USD
/// band, 20 tenths of a bp, at the calm stress level's 1x.
const BAND_BPS: i64 = 2;
/// How long a markout waits for its score: a horizon, and the polling's slack.
const SCORED_WITHIN: Duration = Duration::from_secs(10);
/// An hour, the score's decay step, and the most of one the toxic phase may need.
const HOUR_MS: i64 = 60 * 60 * 1000;
const TOXIC_PHASE_MS: i64 = 4 * 60 * 1000;

/// The index both stand-ins quote: each book is a dollar wide around it.
type Mid = Arc<Mutex<Fixed>>;

#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-toxicity.sh"]
async fn dev_scenario_toxicity() {
    let dir = scratch_dir("feeds-toxicity");
    let key = Arc::new(EnclaveKey::generate());
    let (tx, task) = boot(&dir, &key, true);
    let clock = keep_time(&tx);
    let mid: Mid = Arc::new(Mutex::new(Fixed::from_int(80_000)));
    let venues = start_venues(&tx, &mid).await;
    wait_for_index(&tx, now(&mid)).await;
    for seed in [TOXIC, EDGELESS] {
        fund(&tx, seed).await;
    }

    // 1. Each buy's markout is the 1 bp the books then move, over the 1 bp floor: one standard error,
    // weighed by the buy's notional. The test folds the same sums the score does, which decay only at
    // the hour: the phase starts clear of one.
    let into_hour = now_ms().rem_euclid(HOUR_MS);
    if into_hour > HOUR_MS - TOXIC_PHASE_MS {
        sleep(Duration::from_millis((HOUR_MS - into_hour + 1_000) as u64)).await;
    }
    let mut model = Model::default();
    for k in 0..25 {
        let index = now(&mid);
        let buy = fill(&tx, TOXIC, Side::Buy, "0.01", index).await;
        assert_eq!(buy.index, index, "buy {k} filled at the index the stand-ins quote");
        assert_eq!(buy.spread_bps - BAND_BPS, model.add_on_bps(), "buy {k}: {model:?}");
        let moved = index + index.mul_bps(1);
        *mid.lock().unwrap() = moved;
        wait_for_fills(&tx, TOXIC, Fixed::from_int(k + 1), k).await;
        model.fold(&buy, moved);
    }
    let toxic = standing(&tx, TOXIC).await;
    assert_eq!((toxic.tier, toxic.add_on_bps, toxic.fills), (model.tier(), model.add_on_bps(), model.fills));
    assert_eq!(toxic.tier, 1, "{toxic:?}");
    model.assert_score(toxic.score);
    // `GET /v1/accounts/{address}` reads the same standing.
    let view = query(&tx, |engine| crate::api::reads::account_view(engine, account(TOXIC))).await.expect("credited");
    assert_eq!(serde_json::to_value(view).unwrap()["toxicity"], serde_json::to_value(&toxic).unwrap());
    println!("toxic client after 25 fills: {toxic:?}");

    // 2. The books wander on their own, and the edgeless client's 25 fills pay no add-on.
    let wander = wander(&mid);
    let mut state: u32 = 11;
    for k in 0..25 {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let side = if (state >> 16) & 1 == 1 { Side::Buy } else { Side::Sell };
        let filled = fill(&tx, EDGELESS, side, "0.01", now(&mid)).await;
        assert_eq!(filled.spread_bps, BAND_BPS, "edgeless fill {k}");
        sleep(Duration::from_millis(1_100)).await;
    }
    wander.abort();
    sleep(SCORED_WITHIN).await;
    let edgeless = standing(&tx, EDGELESS).await;
    assert_eq!((edgeless.tier, edgeless.add_on_bps), (0, 0), "{edgeless:?}");
    println!("edgeless client after 25 fills: {edgeless:?}");

    // 3. Round trips at the index, between moves, pay the tier's add-on and score nothing.
    let before = standing(&tx, TOXIC).await;
    for _ in 0..5 {
        let index = now(&mid);
        for side in [Side::Buy, Side::Sell] {
            assert_eq!(fill(&tx, TOXIC, side, "0.05", index).await.spread_bps, BAND_BPS + before.add_on_bps);
        }
        *mid.lock().unwrap() = index + index.mul_bps(1);
        sleep(Duration::from_secs(1)).await;
    }
    sleep(SCORED_WITHIN).await;
    let after = standing(&tx, TOXIC).await;
    // Only an hour's decay step, if the scenario crosses one, moves the sums.
    assert_eq!(after.tier, before.tier, "{before:?} -> {after:?}");
    assert!(after.fills <= before.fills && after.fills >= before.fills.mul_bps(9_900), "{before:?} -> {after:?}");
    println!("toxic client after the round trips: {after:?}");

    // 4. Stopped, the log replays to every root; restarted, the node recovers the same score.
    venues.iter().for_each(AbortHandle::abort);
    signed(&tx, |_| Command::Tick).await;
    let stopped = standing(&tx, TOXIC).await;
    clock.abort();
    task.abort();
    let replayed = replay_path(&dir).expect("the log replays");
    assert!(replayed.commands > 0 && replayed.checkpoints > 0);
    let (tx, _task) = boot(&dir, &key, false);
    assert_eq!(standing(&tx, TOXIC).await, stopped);
    println!("replayed {} commands; the restart recovered {stopped:?}", replayed.commands);
}

/// The sums the test folds from the moves, as the score folds them: each buy weighs its notional,
/// uncapped since the buys are about equal, and its markout is the move to the next index in bps of
/// its index, rounded up, over the floor.
#[derive(Debug, Default)]
struct Model {
    edge: Fixed,
    squares: Fixed,
    fills: Fixed,
}

impl Model {
    fn fold(&mut self, buy: &Fill, next: Fixed) {
        let bps = (next - buy.index).mul_div(Fixed::from_int(10_000), buy.index, Rounding::Up);
        let errors = bps.div_rounded(Fixed::from_int(FLOOR_BPS), Rounding::Up);
        let weight = buy.qty.mul(buy.price);
        self.edge += weight.mul_rounded(errors, Rounding::Up);
        self.squares += weight.mul(weight);
        self.fills += Fixed::ONE;
    }

    /// The dev genesis's tiers the sums reach: scores of 4 and 8, from 20 fills.
    fn tier(&self) -> usize {
        if self.fills < Fixed::from_int(MIN_FILLS) || !self.edge.is_positive() {
            return 0;
        }
        let reaches = |z: i64| self.edge.mul(self.edge) >= Fixed::from_int(z * z).mul(self.squares);
        [4, 8].into_iter().filter(|z| reaches(*z)).count()
    }

    fn add_on_bps(&self) -> i64 {
        [0, 2, 5][self.tier()]
    }

    /// Checks the engine's score is the edge over the root of the squares, to a millionth.
    fn assert_score(&self, score: Fixed) {
        let at = |s: Fixed| s.mul(s).mul(self.squares);
        let tolerance = Fixed::from_raw(1_000);
        let target = self.edge.mul(self.edge);
        assert!(at(score - tolerance) <= target && target <= at(score + tolerance), "{score} vs {self:?}");
    }
}

/// Recovers a sequencer on `dir` under the dev genesis and runs it, as the node does: on a fresh
/// log it records client auth, activates the latest protocol and sequences genesis first.
fn boot(
    dir: &std::path::Path,
    key: &Arc<EnclaveKey>,
    fresh: bool,
) -> (mpsc::Sender<Request>, JoinHandle<anyhow::Result<()>>) {
    let (stream_tx, _): (broadcast::Sender<Arc<StreamMsg>>, _) = broadcast::channel(65_536);
    let (withdrawals_tx, _) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let cfg = SequencerConfig {
        access_policy: Some(policy()),
        segment_commands: 1_000_000,
        checkpoint_every: 1_000,
        checkpoint_parent: Default::default(),
        checkpoints: None,
        ..sequencer_config(dir, manifest)
    };
    let mut seq = Sequencer::recover(cfg, key.clone(), stream_tx, withdrawals_tx).expect("recovers");
    if fresh {
        seq.activate_protocol().expect("protocol activation");
        seq.genesis(&dev_config().genesis).expect("genesis");
    }
    seq.trust(trusted_now()).expect("trusted time");
    let (tx, rx) = mpsc::channel(65_536);
    let run = async move {
        let ran = seq.run(rx).await;
        if let Err(error) = &ran {
            eprintln!("the sequencer stopped: {error:#}");
        }
        ran
    };
    (tx, tokio::spawn(run))
}

/// Starts a Coinbase and a Kraken stand-in quoting BTC-USD a dollar either side of `mid` every 20ms,
/// and the feeds that read them; aborting the handles stops both.
async fn start_venues(tx: &mpsc::Sender<Request>, mid: &Mid) -> Vec<AbortHandle> {
    let mut tasks = Vec::new();
    for (venue, symbol) in [(Venue::Coinbase, "BTC-USD"), (Venue::Kraken, "BTC/USD")] {
        let (listener, url) = listen().await;
        tasks.push(serve(listener, venue, mid.clone()).abort_handle());
        tasks.push(spawn_feed_into(tx.clone(), venue, &url, &[(BTC, symbol)], fast(WAIT)).abort_handle());
    }
    tasks
}

/// Serves `venue`'s book around the shared mid every 20ms, to every connection the feed makes, each
/// until the feed closes it. Each message is a whole book, so a move leaves no level behind.
fn serve(listener: TcpListener, venue: Venue, mid: Mid) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let Ok(mut ws) = accept_async(tcp).await else { continue };
            // The feed's subscription, which also answers its opening ping.
            next_text(&mut ws).await;
            let (mid, sequence) = (mid.clone(), AtomicU64::new(0));
            stream(ws, move || {
                let center = *mid.lock().unwrap();
                let (bid, ask) = ((center - Fixed::ONE).to_string(), (center + Fixed::ONE).to_string());
                let book = match venue {
                    Venue::Coinbase => {
                        let sequence = sequence.fetch_add(1, Ordering::Relaxed);
                        coinbase_levels(sequence, true, "BTC-USD", &bid, &ask, now_ms())
                    }
                    _ => kraken_book("BTC/USD", &bid, &ask, now_ms()),
                };
                vec![book]
            });
        }
    })
}

/// Gives the sequencer a trusted time sample every 10 s, as the node's Roughtime client does, until
/// aborted: past `NO_SAMPLE_GRACE` without one, sequencing halts.
fn keep_time(tx: &mpsc::Sender<Request>) -> AbortHandle {
    let tx = tx.clone();
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(10)).await;
            if tx.send(Request::TimeSample(trusted_now())).await.is_err() {
                break;
            }
        }
    })
    .abort_handle()
}

/// Moves the shared mid a bp up and back down every 700ms, on its own, until aborted.
fn wander(mid: &Mid) -> JoinHandle<()> {
    let mid = mid.clone();
    tokio::spawn(async move {
        let mut up = true;
        loop {
            sleep(Duration::from_millis(700)).await;
            let mut center = mid.lock().unwrap();
            *center = if up { *center + center.mul_bps(1) } else { *center - center.mul_bps(1) };
            up = !up;
        }
    })
}

/// The shared mid now.
fn now(mid: &Mid) -> Fixed {
    *mid.lock().unwrap()
}

/// Declares and funds the client with access seed `seed`.
async fn fund(tx: &mpsc::Sender<Request>, seed: u8) {
    let declared = signed(tx, move |engine| declaration(engine, seed)).await;
    assert!(matches!(declared.as_slice(), [Event::EligibilityAttested { .. }]), "{declared:?}");
    let reference = format!("dev-toxicity-{seed}");
    let account = Some(account(seed));
    let funded =
        signed(tx, move |engine| funding(engine, account, account, Fixed::from_int(100_000), &reference)).await;
    assert!(funded.iter().any(|event| matches!(event, Event::Deposited { .. })), "{funded:?}");
}

/// The client's market order of `qty` quoted at `index`, which must fill.
async fn fill(tx: &mpsc::Sender<Request>, seed: u8, side: Side, qty: &str, index: Fixed) -> Fill {
    let request = OrderRequest {
        account: account(seed),
        client_id: 1,
        market: BTC,
        side,
        qty: qty.parse().expect("a quantity"),
        kind: OrderKind::Market { quoted: index, max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let events = signed(tx, move |engine| order(engine, seed, request)).await;
    events.into_iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None }).expect("a fill")
}

/// Builds a command from the engine's state, sequences it and returns its events.
async fn signed(
    tx: &mpsc::Sender<Request>,
    build: impl FnOnce(&engine_core::Engine) -> Command + Send + 'static,
) -> Vec<Event> {
    let cmd = query(tx, build).await;
    let (reply, events) = oneshot::channel();
    tx.send(Request::Submit { cmd, reply: Some(reply) }).await.expect("sequencer up");
    timeout(WAIT, events).await.expect("reply in time").expect("reply")
}

/// Reads `read` from the engine.
async fn query<T: Send + 'static>(
    tx: &mpsc::Sender<Request>,
    read: impl FnOnce(&engine_core::Engine) -> T + Send + 'static,
) -> T {
    let (reply, answer) = oneshot::channel();
    tx.send(Request::Query(Box::new(move |engine| _ = reply.send(read(engine))))).await.expect("sequencer up");
    timeout(WAIT, answer).await.expect("answer in time").expect("answer")
}

/// The client's score as of the engine's sequenced time.
async fn standing(tx: &mpsc::Sender<Request>, seed: u8) -> ToxicityStanding {
    query(tx, move |engine| engine.toxicity_standing(account(seed))).await.expect("the latest protocol scores")
}

/// Waits until the engine's index for BTC-USD is `index`.
async fn wait_for_index(tx: &mpsc::Sender<Request>, index: Fixed) {
    let found = async {
        while query(tx, |engine| engine.markets()[&BTC].index(engine.now()).ok().map(|i| i.mid)).await != Some(index) {
            sleep(Duration::from_millis(50)).await;
        }
    };
    timeout(WAIT, found).await.expect("the stand-ins price BTC-USD");
}

/// Waits until the client's score counts `fills`, after its buy `k`.
async fn wait_for_fills(tx: &mpsc::Sender<Request>, seed: u8, fills: Fixed, k: i64) {
    let scored = async {
        while standing(tx, seed).await.fills < fills {
            sleep(Duration::from_millis(100)).await;
        }
    };
    let waited = timeout(SCORED_WITHIN, scored).await;
    assert!(waited.is_ok(), "buy {k}'s markout did not score within {SCORED_WITHIN:?}");
}
