//! The fallback fixer: which turn waits on its fallback, and what it sends.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::sol_types::SolCall;

use super::*;
use crate::liveness::caller::Caller;
use crate::liveness::chain::{fixFallbackCall, pending_fallback, witnessCall};
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain, RECEIVER, VAULT};
use crate::liveness::link::Copy;
use crate::test_support::{capture_alerts, capture_alerts_and_resolutions};

mod reach;

/// How often the fixer polls in these tests.
const TEST_POLL: Duration = Duration::from_millis(50);

/// A call sent: where to, and its data.
type SentCall = (Address, Vec<u8>);

/// The calls sent, every estimate passing, and how often the balance was read.
#[derive(Clone, Default)]
struct Sent(Arc<Mutex<Vec<SentCall>>>, Arc<AtomicUsize>);

impl Sending for Sent {
    async fn send(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<B256>> {
        self.0.lock().unwrap().push((to, data));
        Ok(Some(B256::ZERO))
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        self.1.fetch_add(1, Ordering::Relaxed);
        Ok(U256::from(10u64.pow(18)))
    }
}

impl Sent {
    /// How often the balance was read: once a poll, for the caller and the fixer alike.
    fn polls(&self) -> usize {
        self.1.load(Ordering::Relaxed)
    }

    fn calls(&self) -> Vec<SentCall> {
        self.0.lock().unwrap().clone()
    }

    fn count(&self, call: &SentCall) -> usize {
        self.calls().iter().filter(|sent| *sent == call).count()
    }
}

/// `state` with the turn whose draw deadline last passed waiting on its fallback, and the fix for it.
fn pending(state: ChainState) -> (ChainState, SentCall) {
    let turn = crate::seed_relay::schedule::last_final_turn(state.time);
    (ChainState { seed: (turn, B256::ZERO, B256::ZERO, 0), ..state }, (RECEIVER, fixFallbackCall { turn }.abi_encode()))
}

fn fixer<C, S>(chain: C, sending: S, sends: Arc<Sends>) -> Fixer<C, S> {
    Fixer {
        chain,
        sending,
        chain_id: 42161,
        vault: VAULT,
        idle: TEST_POLL,
        poll: TEST_POLL,
        min_balance: U256::ZERO,
        sends,
    }
}

/// The member's liveness caller over `chain`, sending through `sending` under `sends`.
fn caller<C, S>(chain: C, sending: S, sends: Arc<Sends>) -> Caller<C, S> {
    Caller {
        chain,
        sending,
        domain: DOMAIN,
        copy: Copy::default(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends,
    }
}

/// Sends that each take a second, counting the most in flight at once, from a balance the test sets;
/// sends fail while `failing` and balance reads while `unreadable`.
#[derive(Clone, Default)]
struct Slow {
    sent: Sent,
    in_flight: Arc<AtomicUsize>,
    most: Arc<AtomicUsize>,
    balance: Arc<Mutex<U256>>,
    failing: Arc<AtomicBool>,
    unreadable: Arc<AtomicBool>,
}

/// What `capture_alerts_and_resolutions` collects.
type Alerts = tokio::sync::mpsc::UnboundedReceiver<crate::test_support::Alert>;

/// The events `alerts` holds whose `key` field (`alert` or `alert_resolved`) is `name`.
fn named(alerts: &mut Alerts, key: &str, name: &str) -> Vec<crate::test_support::Alert> {
    std::iter::from_fn(|| alerts.try_recv().ok())
        .filter(|alert| alert.get(key).map(String::as_str) == Some(name))
        .collect()
}

impl Sending for Slow {
    async fn send(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<B256>> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(1)).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        anyhow::ensure!(!self.failing.load(Ordering::SeqCst), "the endpoint refuses");
        self.sent.send(to, data).await
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        anyhow::ensure!(!self.unreadable.load(Ordering::SeqCst), "the balance is unreadable");
        Ok(*self.balance.lock().unwrap())
    }
}

/// The turn whose draw deadline last passed waits on its fallback while the rotation switch is on, it has
/// no relayed seed, and the receiver has recorded no block hash for it; the block its first fix landed in
/// comes with it.
#[test]
fn a_turn_with_no_relayed_seed_waits_on_its_fallback_until_its_hash_is_recorded() {
    let read = |state: ChainState| {
        let chain = FakeChain::new(state);
        let block = futures_util::FutureExt::now_or_never(chain.latest()).unwrap().unwrap();
        futures_util::FutureExt::now_or_never(pending_fallback(&block, &DOMAIN.standby)).unwrap().unwrap()
    };
    let turn = crate::seed_relay::schedule::last_final_turn(ChainState::default().time);
    let other = ChainState { seed: (turn - 1, B256::ZERO, B256::ZERO, 0), ..Default::default() };
    assert_eq!(read(other), None, "relayed: the receiver keyed by turn answers another's");
    let (unrelayed, _) = pending(ChainState::default());
    assert_eq!(read(unrelayed.clone()), Some(PendingFallback { receiver: RECEIVER, turn, fixed_in: 0 }));
    let fixed = ChainState { seed: (turn, B256::ZERO, B256::ZERO, 90), ..unrelayed.clone() };
    assert_eq!(read(fixed), Some(PendingFallback { receiver: RECEIVER, turn, fixed_in: 90 }));
    let recorded = ChainState { seed: (turn, B256::ZERO, B256::repeat_byte(1), 90), ..unrelayed.clone() };
    assert_eq!(read(recorded), None, "a hash recorded");
    assert_eq!(read(ChainState { switch: false, ..unrelayed.clone() }), None, "rotation off: none is started");
    let off = ChainState { switch: false, seed: (turn, B256::ZERO, B256::ZERO, 90), ..unrelayed };
    assert_eq!(read(off), Some(PendingFallback { receiver: RECEIVER, turn, fixed_in: 90 }), "one fixed is finished");
}

/// Every poll sends a fix while the turn waits on its fallback, whether or not a turn names the operator,
/// and none once the hash is recorded or the rotation switch is off.
#[tokio::test(start_paused = true)]
async fn the_fixer_fixes_a_pending_fallback_until_its_hash_is_recorded() {
    let (state, fix) = pending(ChainState { generation: 0, ..Default::default() });
    let chain = Arc::new(FakeChain::new(state));
    let sent = Sent::default();
    let task = tokio::spawn(fixer(chain.clone(), sent.clone(), Arc::default()).run());
    tokio::time::sleep(TEST_POLL * 3 / 2).await;
    assert_eq!(sent.calls(), vec![fix.clone(), fix.clone()], "a fix at each of two polls");
    chain.set(|chain| chain.seed.2 = B256::repeat_byte(1));
    tokio::time::sleep(TEST_POLL * 2).await;
    assert_eq!(sent.count(&fix), 2, "no fix once the hash is recorded");
    chain.set(|chain| (chain.seed.2, chain.switch) = (B256::ZERO, false));
    tokio::time::sleep(TEST_POLL * 2).await;
    assert_eq!(sent.count(&fix), 2, "rotation off: no fallback is started");
    chain.set(|chain| chain.seed.3 = chain.number);
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    assert_eq!(sent.count(&fix), 3, "rotation off: a fallback someone fixed the block of is finished");
}

/// A fixed block past `ArbSys`'s reach is never recorded: the fixer sends nothing for it.
#[tokio::test(start_paused = true)]
async fn the_fixer_sends_nothing_for_a_block_past_the_reach() {
    let (state, fix) = pending(ChainState { number: 1_000, ..Default::default() });
    let lapsed = ChainState { seed: (state.seed.0, B256::ZERO, B256::ZERO, 1_000 - BLOCK_HASH_REACH - 1), ..state };
    let chain = Arc::new(FakeChain::new(lapsed));
    let sent = Sent::default();
    let task = tokio::spawn(fixer(chain.clone(), sent.clone(), Arc::default()).run());
    tokio::time::sleep(TEST_POLL * 3 / 2).await;
    assert!(sent.calls().is_empty(), "{:?}", sent.calls());
    chain.set(|chain| chain.seed.3 = 1_000 - BLOCK_HASH_REACH);
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    assert_eq!(sent.count(&fix), 1, "a block still in reach is recorded");
}

/// A seed receiver the fixer cannot read pages `liveness_caller_fallback_failed` once.
#[tokio::test(start_paused = true)]
async fn a_receiver_the_fixer_cannot_read_pages_once() {
    let (_guard, mut alerts) = capture_alerts();
    let (state, _) = pending(ChainState { failing: vec![("liveness_fallback_hash", 0)], ..Default::default() });
    let sent = Sent::default();
    let task = tokio::spawn(fixer(Arc::new(FakeChain::new(state)), sent.clone(), Arc::default()).run());
    tokio::time::sleep(TEST_POLL * 5 / 2).await;
    task.abort();
    assert!(sent.calls().is_empty());
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let failed = raised.iter().filter(|alert| alert["alert"] == "liveness_caller_fallback_failed").count();
    assert_eq!(failed, 1, "{raised:?}");
}

/// A reader over a fake chain holding one read session for each pinned block, as `StandbyReader` does.
struct Sessions(Arc<FakeChain>, tokio::sync::Mutex<()>);

/// A block pinned through `Sessions`, holding its session.
struct SessionBlock<'a> {
    block: crate::liveness::fake::FakeBlock<'a>,
    _session: tokio::sync::MutexGuard<'a, ()>,
}

impl Sessions {
    fn over(chain: &Arc<FakeChain>) -> Self {
        Self(chain.clone(), tokio::sync::Mutex::new(()))
    }
}

impl StandbyChain for Sessions {
    type Block<'a>
        = SessionBlock<'a>
    where
        Self: 'a;

    async fn latest(&self) -> anyhow::Result<SessionBlock<'_>> {
        let session = self.1.lock().await;
        Ok(SessionBlock { block: self.0.latest().await?, _session: session })
    }

    async fn finalized(&self) -> anyhow::Result<SessionBlock<'_>> {
        self.latest().await
    }
}

impl AgreedBlock for SessionBlock<'_> {
    fn time(&self) -> u64 {
        self.block.time()
    }

    fn number(&self) -> u64 {
        self.block.number()
    }

    fn hash(&self) -> B256 {
        self.block.hash()
    }

    async fn call(&self, operation: &'static str, to: Address, data: Bytes) -> anyhow::Result<Bytes> {
        self.block.call(operation, to, data).await
    }
}

/// The fixer reads through a reader of its own: with its calls answering in a second, the member's
/// liveness calls in ten, and each reader holding a read session for each pinned block, its fix goes out
/// after its own reads, long before the member's liveness read on its reader is through, and the two
/// send through one lock.
#[tokio::test(start_paused = true)]
async fn a_fix_waits_on_no_liveness_read() {
    let (state, fix) = pending(ChainState { latency: Duration::from_secs(1), ..Default::default() });
    let liveness = Arc::new(FakeChain::new(ChainState { latency: Duration::from_secs(10), ..state.clone() }));
    let chain = Arc::new(FakeChain::new(state));
    let (sent, sends) = (Sent::default(), Arc::new(Sends::default()));
    let caller = caller(Sessions::over(&liveness), sent.clone(), sends.clone());
    let tasks = [tokio::spawn(caller.run()), tokio::spawn(fixer(Sessions::over(&chain), sent.clone(), sends).run())];
    tokio::time::sleep(Duration::from_millis(4_500)).await;
    assert_eq!(sent.calls(), vec![fix], "the fix alone, the liveness read still under way");
    tokio::time::sleep(Duration::from_secs(20)).await;
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    assert!(sent.calls().contains(&(DOMAIN.standby.schedule, witnessCall {}.abi_encode())));
}

/// The member's liveness caller and its fixer send from one account: with each send taking a second and
/// both sending at every read, the caller's a quarter of the fake chain's 60 s interval apart, no two sends
/// are ever in flight at once, and both go out.
#[tokio::test(start_paused = true)]
async fn the_caller_and_the_fixer_send_one_call_at_a_time() {
    let (state, fix) = pending(ChainState::default());
    let chain = Arc::new(FakeChain::new(state));
    let (slow, sends) = (Slow::default(), Arc::new(Sends::default()));
    let tasks = [
        tokio::spawn(caller(chain.clone(), slow.clone(), sends.clone()).run()),
        tokio::spawn(fixer(chain, slow.clone(), sends).run()),
    ];
    tokio::time::sleep(Duration::from_secs(20)).await;
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    assert_eq!(slow.most.load(Ordering::SeqCst), 1, "never two sends in flight");
    assert!(slow.sent.count(&fix) >= 2, "{:?}", slow.sent.calls());
    assert!(slow.sent.count(&(DOMAIN.standby.schedule, witnessCall {}.abi_encode())) >= 2, "{:?}", slow.sent.calls());
}

/// The fixer reads the account's balance at every poll, with or without the member's caller, and the two
/// raise `liveness_caller_unfunded` as one episode: once while it is below the minimum, resolved once.
#[tokio::test(start_paused = true)]
async fn the_fixer_watches_the_balance_in_one_episode_with_the_caller() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let chain = Arc::new(FakeChain::new(ChainState::default()));
    let (slow, sends) = (Slow::default(), Arc::new(Sends::default()));
    let fixer = Fixer { min_balance: U256::from(1), ..fixer(chain.clone(), slow.clone(), sends.clone()) };
    let alone = tokio::spawn(fixer.run());
    tokio::time::sleep(TEST_POLL * 5 / 2).await;
    let unfunded = |alerts: &mut Alerts, key: &str| named(alerts, key, "liveness_caller_unfunded").len();
    assert_eq!(unfunded(&mut alerts, "alert"), 1, "the fixer alone pages it");
    let caller = Caller { min_balance: U256::from(1), ..caller(chain, slow.clone(), sends) };
    let beside = tokio::spawn(caller.run());
    // The caller's witnesses take a second each, between its balance reads.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(unfunded(&mut alerts, "alert"), 0, "the caller's reads repeat the fixer's episode");
    *slow.balance.lock().unwrap() = U256::from(1);
    tokio::time::sleep(Duration::from_secs(5)).await;
    alone.abort();
    beside.abort();
    assert_eq!(unfunded(&mut resolved, "alert_resolved"), 1, "funded: one resolve");
    assert_eq!(unfunded(&mut alerts, "alert"), 0);
}

/// A fixed block past the reach pages `seed_fallback_lapsed` once for its turn, until the next turn's draw
/// deadline passes and the turn is no longer pending, though that turn's own fixes fail.
#[tokio::test(start_paused = true)]
async fn a_lapsed_block_pages_once_until_its_turns_boundary() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (state, _) = pending(ChainState { number: 1_000, ..Default::default() });
    let turn = state.seed.0;
    let lapsed = ChainState { seed: (turn, B256::ZERO, B256::ZERO, 1_000 - BLOCK_HASH_REACH - 1), ..state };
    let chain = Arc::new(FakeChain::new(lapsed));
    let slow = Slow { failing: Arc::new(AtomicBool::new(true)), ..Slow::default() };
    let task = tokio::spawn(fixer(chain.clone(), slow, Arc::default()).run());
    tokio::time::sleep(TEST_POLL * 7 / 2).await;
    let raised = named(&mut alerts, "alert", "seed_fallback_lapsed");
    assert_eq!(raised.len(), 1, "{raised:?}");
    assert!(raised[0]["message"].contains(&format!("turn {turn}'s")), "{raised:?}");
    assert_eq!(raised[0]["level"], "WARN");
    assert!(named(&mut resolved, "alert_resolved", "seed_fallback_lapsed").is_empty(), "lapsed from the first read");
    let next = |chain: &mut ChainState| {
        chain.time += crate::seed_relay::schedule::TURN_SECS;
        chain.seed = (turn + 1, B256::ZERO, B256::ZERO, 0);
    };
    chain.set(next);
    tokio::time::sleep(Duration::from_secs(5)).await;
    task.abort();
    let ended = named(&mut resolved, "alert_resolved", "seed_fallback_lapsed");
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(
        ended[0].keys().filter(|key| !["alert_resolved", "message", "level"].contains(&key.as_str())).count(),
        0
    );
    let later: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let count = |name: &str| later.iter().filter(|alert| alert.get("alert").map(String::as_str) == Some(name)).count();
    assert_eq!(count("seed_fallback_lapsed"), 0, "{later:?}");
    assert_eq!(count("liveness_caller_fallback_failed"), 1, "turn {}'s fixes fail: {later:?}", turn + 1);
}

/// The lapse episode's key names no turn, so a fixer whose first read finds no lapsed turn resolves one
/// an earlier run left open, as after a restart past the lapsed turn's boundary.
#[tokio::test(start_paused = true)]
async fn a_first_read_resolves_a_lapse_an_earlier_run_left_open() {
    let (_guard, _alerts, mut resolved) = capture_alerts_and_resolutions();
    let task =
        tokio::spawn(fixer(Arc::new(FakeChain::new(ChainState::default())), Sent::default(), Arc::default()).run());
    tokio::time::sleep(TEST_POLL * 5 / 2).await;
    task.abort();
    assert_eq!(named(&mut resolved, "alert_resolved", "seed_fallback_lapsed").len(), 1, "once, at the first read");
}

/// A balance the fixer cannot read pages `liveness_caller_fallback_failed` once, though its reads and
/// fixes go through, and a read resolves it once.
#[tokio::test(start_paused = true)]
async fn an_unreadable_balance_pages_the_fixer_once() {
    let (_guard, mut alerts, mut resolved) = capture_alerts_and_resolutions();
    let (state, _) = pending(ChainState::default());
    let slow = Slow { unreadable: Arc::new(AtomicBool::new(true)), ..Slow::default() };
    let task = tokio::spawn(fixer(Arc::new(FakeChain::new(state)), slow.clone(), Arc::default()).run());
    tokio::time::sleep(Duration::from_secs(5)).await;
    let failed = named(&mut alerts, "alert", "liveness_caller_fallback_failed");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0]["message"].contains("cannot read the liveness caller's balance"), "{failed:?}");
    assert!(!slow.sent.calls().is_empty(), "the fixes go through");
    assert!(named(&mut resolved, "alert_resolved", "liveness_caller_fallback_failed").is_empty());
    slow.unreadable.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(5)).await;
    task.abort();
    assert_eq!(named(&mut resolved, "alert_resolved", "liveness_caller_fallback_failed").len(), 1);
    assert!(named(&mut alerts, "alert", "liveness_caller_fallback_failed").is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_fixer_reads_at_its_idle_interval_until_a_fix_is_under_way() {
    let idle = TEST_POLL * 10;
    let chain = Arc::new(FakeChain::new(ChainState::default()));
    let sent = Sent::default();
    let mut fixer = fixer(chain.clone(), sent.clone(), Arc::default());
    fixer.idle = idle;
    let task = tokio::spawn(fixer.run());
    tokio::time::sleep(idle * 3 + TEST_POLL).await;
    assert_eq!(sent.polls(), 4, "a read at once, then one each idle interval");
    let (state, fix) = pending(ChainState::default());
    chain.set(|chain| *chain = state);
    tokio::time::sleep(idle - TEST_POLL / 2).await;
    assert_eq!(sent.count(&fix), 1, "the next idle read sends the fix");
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    assert_eq!(sent.count(&fix), 2, "the follow-up comes a poll later, not an idle interval");
}

#[tokio::test(start_paused = true)]
async fn the_caller_reads_four_times_a_heartbeat_interval_however_long_its_read_interval() {
    // A 4 s heartbeat interval: a read every second, though the read interval is a minute.
    let chain = Arc::new(FakeChain::new(ChainState { interval: 4, ..Default::default() }));
    let sent = Sent::default();
    let mut caller = caller(chain, sent.clone(), Arc::default());
    caller.poll = Duration::from_secs(60);
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(Duration::from_millis(10_500)).await;
    task.abort();
    assert_eq!(sent.polls(), 11, "reads at 0 s to 10 s");
}

/// The share of the interval holds however short the read interval: a 4 s heartbeat interval reads every
/// second, though the read interval is a tenth of that.
#[tokio::test(start_paused = true)]
async fn the_caller_reads_four_times_a_heartbeat_interval_however_short_its_read_interval() {
    let chain = Arc::new(FakeChain::new(ChainState { interval: 4, ..Default::default() }));
    let sent = Sent::default();
    let mut caller = caller(chain, sent.clone(), Arc::default());
    caller.poll = Duration::from_millis(100);
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(Duration::from_millis(10_500)).await;
    task.abort();
    assert_eq!(sent.polls(), 11, "reads at 0 s to 10 s");
}

#[tokio::test(start_paused = true)]
async fn the_fixer_reads_at_its_interval_through_a_failed_read_and_follows_a_block_anyone_fixed() {
    let chain = Arc::new(FakeChain::new(ChainState::default()));
    let sent = Sent::default();
    let mut fixer = fixer(chain.clone(), sent.clone(), Arc::default());
    (fixer.idle, fixer.poll) = (Duration::from_secs(30), Duration::from_secs(5));
    let task = tokio::spawn(fixer.run());
    tokio::time::sleep(Duration::from_secs(1)).await;
    // The quorum is lost through the read at 30 s; meanwhile no seed is relayed, and a draw's first
    // call fixes a block before any member reads.
    chain.set(|chain| chain.down = true);
    tokio::time::sleep(Duration::from_secs(39)).await;
    let (state, fix) = pending(ChainState::default());
    chain.set(|chain| *chain = ChainState { seed: (state.seed.0, B256::ZERO, B256::ZERO, state.number), ..state });
    tokio::time::sleep(Duration::from_secs(21)).await;
    assert_eq!(sent.count(&fix), 1, "the read at 60 s, an interval after the failed one, records the hash");
    tokio::time::sleep(Duration::from_secs(5)).await;
    task.abort();
    assert_eq!(sent.count(&fix), 2, "and follows it up a poll later");
}

#[tokio::test(start_paused = true)]
async fn the_caller_retries_a_failed_first_read_within_5_s_however_long_its_read_interval() {
    let chain = Arc::new(FakeChain::new(ChainState { down: true, ..Default::default() }));
    let sent = Sent::default();
    let mut caller = caller(chain.clone(), sent.clone(), Arc::default());
    caller.poll = Duration::from_secs(60);
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(Duration::from_secs(1)).await;
    chain.set(|chain| chain.down = false);
    tokio::time::sleep(Duration::from_secs(5)).await;
    task.abort();
    assert_eq!(sent.polls(), 1, "the read at 5 s, after the one that failed at 0 s");
}

#[tokio::test(start_paused = true)]
async fn a_fix_under_way_is_followed_up_through_failed_reads() {
    let (state, fix) = pending(ChainState::default());
    let chain = Arc::new(FakeChain::new(state));
    let sent = Sent::default();
    let mut fixer = fixer(chain.clone(), sent.clone(), Arc::default());
    (fixer.idle, fixer.poll) = (Duration::from_secs(30), Duration::from_secs(5));
    let task = tokio::spawn(fixer.run());
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(sent.count(&fix), 1, "the first fix at once");
    // The quorum is lost through the reads at 5 s and 10 s, and back before the one at 15 s.
    chain.set(|chain| (chain.down, chain.seed.3) = (true, chain.number));
    tokio::time::sleep(Duration::from_secs(11)).await;
    chain.set(|chain| chain.down = false);
    tokio::time::sleep(Duration::from_secs(4)).await;
    task.abort();
    assert_eq!(sent.count(&fix), 2, "the read at 15 s records the hash");
}

#[tokio::test(start_paused = true)]
async fn the_fixer_reads_every_interval_from_each_read_start_however_slow_its_reads() {
    let chain = Arc::new(FakeChain::new(ChainState { latency: Duration::from_secs(1), ..Default::default() }));
    let sent = Sent::default();
    let mut fixer = fixer(chain, sent.clone(), Arc::default());
    (fixer.idle, fixer.poll) = (Duration::from_secs(30), Duration::from_secs(5));
    let task = tokio::spawn(fixer.run());
    tokio::time::sleep(Duration::from_secs(29)).await;
    assert_eq!(sent.polls(), 1, "the first read took seconds");
    tokio::time::sleep(Duration::from_secs(8)).await;
    task.abort();
    assert_eq!(sent.polls(), 2, "the second read starts at 30 s, not 30 s after the first ended");
}
