//! The liveness caller: what a read calls for, and what it sends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;

use super::*;
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain};
use crate::log_heads::{Head, HeadV2, Hold};

/// How often the caller polls in these tests.
const TEST_POLL: Duration = Duration::from_millis(50);
/// How often it reads once it knows the fake chain's 60 s heartbeat interval: a quarter of it.
const SLOT_READ: Duration = Duration::from_secs(15);

mod gas;

/// A call sent: where to, and its data.
type SentCall = (Address, Vec<u8>);

/// The calls sent, and what the endpoint answers.
#[derive(Clone, Default)]
struct Sent {
    calls: Arc<Mutex<Vec<SentCall>>>,
    balance: Arc<Mutex<U256>>,
    /// Estimates refused: the chain would refuse every call.
    refused: Arc<Mutex<bool>>,
    /// Estimates refused with `KeyHolderUnknown`, as the schedule does while the newest head's release is no
    /// candidate's.
    unowned: Arc<Mutex<bool>>,
    /// The endpoint fails every request, as one down or not signing for the caller does.
    down: Arc<Mutex<bool>>,
    /// The balance reads asked for, one a chain read.
    balance_reads: Arc<Mutex<u32>>,
}

impl Sending for Sent {
    async fn send(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<B256>> {
        anyhow::ensure!(!*self.down.lock().unwrap(), "liveness RPC failed: connection refused");
        if *self.unowned.lock().unwrap() {
            return Err(KeyHolderRefused.into());
        }
        if *self.refused.lock().unwrap() {
            return Ok(None);
        }
        self.calls.lock().unwrap().push((to, data));
        Ok(Some(B256::ZERO))
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        *self.balance_reads.lock().unwrap() += 1;
        anyhow::ensure!(!*self.down.lock().unwrap(), "liveness RPC failed: connection refused");
        Ok(*self.balance.lock().unwrap())
    }
}

#[test]
fn a_read_calls_for_a_witness_while_the_slot_is_unmarked_and_a_promotion_once_due() {
    let mut view = view(ChainState::default());
    // The block's slot is 30,000,000; the latest mark is the slot before it.
    assert_eq!(calls(&view, None, 0), vec![Call::Witness]);
    view.marked_slot = view.time / 60;
    assert!(calls(&view, None, 0).is_empty(), "marked already");
    view.due_at = view.time;
    assert_eq!(calls(&view, None, 0), vec![Call::Promote]);
    view.generation = 0;
    assert!(calls(&view, None, 0).is_empty(), "no turn names the operator");
}

/// A skip once `skipDueAt` passes, by the later of the block's time and the caller's clock (#596); a
/// latched assignment reads as never due.
#[test]
fn a_read_calls_for_a_skip_once_the_take_window_has_passed() {
    let mut view = view(ChainState::default());
    view.marked_slot = view.time / 60;
    assert!(calls(&view, None, 0).is_empty(), "a latched assignment is never skipped");
    view.skip_due_at = view.time + 1;
    assert!(calls(&view, None, 0).is_empty(), "the window has a second to run");
    assert_eq!(calls(&view, None, view.time + 1), vec![Call::Skip], "due by the caller's clock");
    view.skip_due_at = view.time;
    assert_eq!(calls(&view, None, 0), vec![Call::Skip]);
    view.due_at = view.time;
    assert_eq!(calls(&view, None, 0), vec![Call::Promote, Call::Skip], "the chain takes whichever passes");
    view.generation = 0;
    assert!(calls(&view, None, 0).is_empty(), "no turn names the operator");
}

#[test]
fn a_witness_follows_the_current_slot_across_an_interval_change_and_a_quiet_chain() {
    let mut view = view(ChainState::default());
    // Marked under a 30 s interval: the latest mark names a slot twice the current one's number.
    view.marked_slot = view.time / 30;
    assert_eq!(calls(&view, None, 0), vec![Call::Witness], "an increase leaves the latest mark in other units");
    view.marked_slot = view.time / 90;
    assert_eq!(calls(&view, None, 0), vec![Call::Witness], "a decrease does too");
    view.marked_slot = view.time / 60;
    assert!(calls(&view, None, 0).is_empty(), "the current slot is marked");
    // No transaction since: the latest block stays in the marked slot while the caller's clock moves on.
    assert_eq!(calls(&view, None, view.time + 60), vec![Call::Witness], "the next slot needs its mark");
    assert!(calls(&view, None, view.time - 600).is_empty(), "a clock behind the chain judges by the block");
    view.due_at = view.time + 30;
    assert_eq!(calls(&view, None, view.time + 30), vec![Call::Promote], "due by the caller's clock");
}

/// The caller's clock is trusted time's lower bound, never the host's: a host an hour ahead of the
/// chain sends no witness for the slot the chain marked, which would do nothing and cost gas.
#[test]
fn a_host_clock_ahead_of_the_chain_sends_no_witness() {
    use crate::clock::Clock as _;
    let mut view = view(ChainState::default());
    view.marked_slot = view.time / 60;
    let block_ms = i64::try_from(view.time).unwrap() * 1000;
    let host = Arc::new(crate::clock::test_clock::TestClock::at(block_ms + 3_600_000));
    let sample = crate::trusted_time::Sample { at: host.instant(), earliest_ms: block_ms, latest_ms: block_ms + 1_000 };
    let (_samples, receiver) = tokio::sync::watch::channel(Some(sample));
    let reached = Reached(crate::trusted_time::TrustedClock::new(host, receiver, Duration::from_secs(5)));
    let now = u64::try_from(reached.now_ms() / 1000).unwrap();
    assert_eq!(now, view.time, "the caller's time is the sample's lower bound, not the host's");
    assert!(calls(&view, None, now).is_empty(), "a witness for a slot the chain marked");
}

/// Only a revert is the chain refusing a call: an endpoint that cannot estimate for another reason has
/// failed, which the caller pages, rather than passed over as a call the chain would refuse.
#[tokio::test]
async fn only_a_revert_in_an_estimate_is_the_chain_refusing_the_call() {
    let endpoint = |url: String| {
        let section =
            format!("rpc_url = '{url}'\nrelayer = '{}'\nretry_interval = '1s'\nrpc_timeout = '2s'", Address::ZERO);
        let checkpoints: crate::config::CheckpointSection = toml::from_str(&section).unwrap();
        let liveness = crate::config::LivenessSection {
            caller: Address::repeat_byte(0x22),
            min_balance: U256::from(1),
            read_interval: TEST_POLL,
            fallback_interval: TEST_POLL,
        };
        Endpoint::new(&checkpoints, &liveness, 31_337).unwrap()
    };
    let reverting =
        endpoint(rpc_answering(serde_json::json!({"code": 3, "message": "execution reverted: not due"})).await);
    assert_eq!(reverting.send(Address::ZERO, Vec::new()).await.unwrap(), None, "a revert is the chain refusing");
    let failing = endpoint(rpc_answering(serde_json::json!({"code": -32000, "message": "unknown account"})).await);
    assert!(failing.send(Address::ZERO, Vec::new()).await.is_err(), "the endpoint's own failure passed as a refusal");
}

/// A JSON-RPC endpoint answering every request with `error`; returns its `http://` address.
async fn rpc_answering(error: serde_json::Value) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let answer = move |axum::Json(request): axum::Json<serde_json::Value>| {
        let error = error.clone();
        async move { axum::Json(serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "error": error})) }
    };
    let app = axum::Router::new().fallback(axum::routing::post(answer));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

#[test]
fn a_read_calls_for_a_relay_once_the_chains_head_is_stale_and_a_newer_one_was_signed() {
    // Five sixths of the 60 s interval: past the operator's own heads, due every 30 s.
    let mut view = view(ChainState { head: (7, 1_000, 1_800_000_000 - 49), ..Default::default() });
    view.marked_slot = view.time / 60;
    let newer = signed(1, 8);
    assert!(calls(&view, Some(newer.clone()), 0).is_empty(), "posted 49 s ago: not yet stale");
    view.head.posted_at = view.time - 50;
    assert_eq!(calls(&view, Some(newer.clone()), 0), vec![Call::Relay(Box::new(newer))]);
    assert!(calls(&view, Some(signed(1, 7)), 0).is_empty(), "the head the chain holds already");
    assert!(calls(&view, Some(signed(2, 9)), 0).is_empty(), "another epoch's head counts for no one");
    let mut held = signed(1, 7);
    held.head.signed_at = 2_000;
    held.head.hold = Hold::BenchmarkSync;
    assert_eq!(calls(&view, Some(held.clone()), 0), vec![Call::Relay(Box::new(held))], "a held repeat, signed later");
}

#[tokio::test(start_paused = true)]
async fn the_caller_witnesses_promotes_and_relays_from_its_own_account() {
    let state = ChainState { due_at: 1_800_000_000, skip_due_at: 1_800_000_000, ..Default::default() };
    let chain = Arc::new(FakeChain::new(state));
    let copy = Copy::default();
    let relay = signed(1, 3);
    copy.signed_head(relay.clone());
    let sent = Sent { balance: Arc::new(Mutex::new(U256::from(10u64.pow(18)))), ..Default::default() };
    let caller = Caller {
        chain,
        sending: sent.clone(),
        domain: DOMAIN,
        copy,
        min_balance: U256::from(10u64.pow(16)),
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    let calls = sent.calls.lock().unwrap().clone();
    assert!(calls.contains(&(DOMAIN.standby.schedule, witnessCall {}.abi_encode())), "{calls:?}");
    assert!(calls.contains(&(DOMAIN.standby.schedule, promoteCall {}.abi_encode())));
    assert!(calls.contains(&(DOMAIN.standby.schedule, skipCall {}.abi_encode())), "{calls:?}");
    let relayed = relay.head.calldata(&relay.signature).unwrap();
    assert!(calls.contains(&(DOMAIN.log_heads, relayed)), "the stream's head goes to LogHeads");
}

/// A head the stream brings once the chain's head is stale for a relay is read for and relayed at once,
/// not at the next read a quarter of an interval on, which could come after its signing time has left the
/// head tolerance; one brought while the chain's head is fresh waits for the next read.
#[tokio::test(start_paused = true)]
async fn a_head_the_stream_brings_once_the_chains_is_stale_is_relayed_at_once() {
    // Posted five sixths of the 60 s interval before the block.
    let time = 1_800_000_000;
    let chain = Arc::new(FakeChain::new(ChainState { head: (7, 1_000, time - 50), ..Default::default() }));
    let copy = Copy::default();
    let sent = Sent::default();
    let caller = Caller {
        chain: chain.clone(),
        sending: sent.clone(),
        domain: DOMAIN,
        copy: copy.clone(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    };
    let reads = || *sent.balance_reads.lock().unwrap();
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(SLOT_READ / 3).await;
    assert_eq!(reads(), 1, "the read at 0 s, with no head to relay");
    let newer = signed(1, 8);
    copy.signed_head(newer.clone());
    tokio::time::sleep(TEST_POLL).await;
    assert_eq!(reads(), 2, "read at once");
    let relayed = newer.head.calldata(&newer.signature).unwrap();
    assert!(sent.calls.lock().unwrap().contains(&(DOMAIN.log_heads, relayed)), "and relayed");
    // The chain takes a head; the read at 15 s sees it fresh, and a head the stream brings then waits.
    chain.set(|chain| chain.head = (8, 1_000, time));
    tokio::time::sleep(SLOT_READ).await;
    assert_eq!(reads(), 3);
    copy.signed_head(signed(1, 9));
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    assert_eq!(reads(), 3, "no read for a head while the chain's is fresh");
}

/// A head fresh at the last read turns stale by the caller's clock between reads: a head the stream
/// brings a second before that waits, and one brought at it is read for and relayed at once.
#[tokio::test(start_paused = true)]
async fn a_head_turning_stale_by_the_clock_between_reads_wakes_the_caller() {
    // Posted 40 s before the block: stale for a relay 10 s on, by the clock alone.
    let time = 1_800_000_000;
    let chain = Arc::new(FakeChain::new(ChainState { head: (7, 1_000, time - 40), ..Default::default() }));
    let copy = Copy::default();
    let sent = Sent::default();
    let clock = Arc::new(crate::clock::test_clock::TestClock::at(i64::try_from(time).unwrap() * 1000));
    let caller = Caller {
        chain,
        sending: sent.clone(),
        domain: DOMAIN,
        copy: copy.clone(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: clock.clone(),
        sends: Arc::default(),
    };
    let reads = || *sent.balance_reads.lock().unwrap();
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL).await;
    assert_eq!(reads(), 1, "the read at 0 s");
    clock.advance(Duration::from_secs(9));
    copy.signed_head(signed(1, 8));
    tokio::time::sleep(TEST_POLL).await;
    assert_eq!(reads(), 1, "a second before the head turns stale: no read");
    clock.advance(Duration::from_secs(1));
    let newer = signed(1, 9);
    copy.signed_head(newer.clone());
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    assert_eq!(reads(), 2, "read at once");
    let relayed = newer.head.calldata(&newer.signature).unwrap();
    assert!(sent.calls.lock().unwrap().contains(&(DOMAIN.log_heads, relayed)), "and relayed");
}

#[tokio::test(start_paused = true)]
async fn a_caller_below_its_minimum_balance_pages_once_until_funded() {
    let (_guard, mut alerts, mut resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let chain = Arc::new(FakeChain::new(ChainState::default()));
    let sent = Sent::default();
    let caller = Caller {
        chain,
        sending: sent.clone(),
        domain: DOMAIN,
        copy: Copy::default(),
        min_balance: U256::from(10),
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    };
    let task = tokio::spawn(caller.run());
    // Reads at 0, 15 and 30 s find it unfunded.
    tokio::time::sleep(SLOT_READ * 5 / 2).await;
    assert_eq!(*sent.balance_reads.lock().unwrap(), 3);
    *sent.balance.lock().unwrap() = U256::from(10);
    tokio::time::sleep(SLOT_READ).await;
    task.abort();
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let unfunded = raised.iter().filter(|alert| alert["alert"] == "liveness_caller_unfunded").count();
    assert_eq!(unfunded, 1, "{raised:?}");
    let resolved: Vec<_> = std::iter::from_fn(|| resolutions.try_recv().ok()).collect();
    assert!(resolved.iter().any(|alert| alert["alert_resolved"] == "liveness_caller_unfunded"), "{resolved:?}");
}

/// A caller whose endpoint fails, as one down or not signing for its account does, pages
/// `liveness_caller_send_failed` once for the episode, unfunded or not, and resolves it once a call goes
/// through again.
#[tokio::test(start_paused = true)]
async fn a_caller_whose_endpoint_fails_pages_once_until_a_call_goes_through() {
    let (_guard, mut alerts, mut resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let chain = Arc::new(FakeChain::new(ChainState::default()));
    let sent = Sent { down: Arc::new(Mutex::new(true)), ..Default::default() };
    let caller = Caller {
        chain,
        sending: sent.clone(),
        domain: DOMAIN,
        copy: Copy::default(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    };
    let task = tokio::spawn(caller.run());
    // Reads at 0, 15 and 30 s find the endpoint down.
    tokio::time::sleep(SLOT_READ * 5 / 2).await;
    assert_eq!(*sent.balance_reads.lock().unwrap(), 3);
    *sent.down.lock().unwrap() = false;
    tokio::time::sleep(SLOT_READ).await;
    task.abort();
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let failed = raised.iter().filter(|alert| alert["alert"] == "liveness_caller_send_failed").count();
    assert_eq!(failed, 1, "{raised:?}");
    assert!(!sent.calls.lock().unwrap().is_empty(), "no call went through once the endpoint came back");
    let resolved: Vec<_> = std::iter::from_fn(|| resolutions.try_recv().ok()).collect();
    assert!(resolved.iter().any(|alert| alert["alert_resolved"] == "liveness_caller_send_failed"), "{resolved:?}");
}

#[tokio::test(start_paused = true)]
async fn a_call_the_chain_would_refuse_is_not_sent() {
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
    let sent = Sent { refused: Arc::new(Mutex::new(true)), ..Default::default() };
    let caller = Caller {
        chain,
        sending: sent.clone(),
        domain: DOMAIN,
        copy: Copy::default(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL * 2).await;
    task.abort();
    assert!(sent.calls.lock().unwrap().is_empty());
}

/// The head the caller reads on chain is checked against the copy as a stream head is: a copy holding
/// another link at its seq confirms nothing under the epoch.
#[tokio::test(start_paused = true)]
async fn a_head_on_chain_at_another_link_than_the_copy_holds_stops_its_confirmations() {
    let chain = Arc::new(FakeChain::new(ChainState { epoch: 1, head: (4, 1_000, 0), ..Default::default() }));
    let copy = Copy::default();
    let held = Head { seq: 4, link: B256::repeat_byte(0xee) };
    copy.linked(held.seq, held.link, None);
    copy.durable(held, None);
    assert!(copy.status(1).is_ok(), "the copy confirms before the read");
    let caller = Caller {
        chain,
        sending: Sent::default(),
        domain: DOMAIN,
        copy: copy.clone(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL * 2).await;
    task.abort();
    assert_eq!(copy.conflicted(1), Some(4), "the chain's head at seq 4 names another link");
    assert!(copy.status(1).is_err(), "no confirmation under the epoch");
}

// ── Helpers ──

/// The view `state` reads.
fn view(state: ChainState) -> View {
    let chain = FakeChain::new(state);
    let block = futures_util::FutureExt::now_or_never(chain.latest()).unwrap().unwrap();
    futures_util::FutureExt::now_or_never(crate::liveness::chain::view(&block, &DOMAIN)).unwrap().unwrap()
}

/// A head at `seq` signed under `epoch`, at signing time 1,000.
pub(crate) fn signed(epoch: u64, seq: u64) -> SignedHead {
    let head = HeadV2 {
        head: Head { seq, link: B256::repeat_byte(u8::try_from(seq).unwrap()) },
        parent: None,
        turn: 0,
        position: 0,
        release: B256::ZERO,
        generation: 0,
        stamp: 0,
        signed_at: 1_000,
        hold: Hold::Live,
        confirmer: Address::ZERO,
        block_number: 0,
        block_hash: B256::ZERO,
    };
    SignedHead { epoch, head, signature: format!("0x{}", "11".repeat(65)) }
}

fn unowned_caller(chain: Arc<FakeChain>, sent: &Sent) -> Caller<Arc<FakeChain>, Sent> {
    Caller {
        chain,
        sending: sent.clone(),
        domain: DOMAIN,
        copy: Copy::default(),
        min_balance: U256::ZERO,
        poll: TEST_POLL,
        clock: Arc::new(crate::clock::test_clock::TestClock::at(0)),
        sends: Arc::default(),
    }
}

/// A promotion the schedule refuses with `KeyHolderUnknown` pages `liveness_key_holder_unknown` once, naming the
/// newest head's release, and the page resolves under the key it was raised with once a call is not refused.
#[tokio::test(start_paused = true)]
async fn a_promotion_refused_for_an_unowned_release_pages_once_until_a_call_passes() {
    let (_guard, mut alerts, mut resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
    let sent = Sent { unowned: Arc::new(Mutex::new(true)), ..Default::default() };
    let task = tokio::spawn(unowned_caller(chain, &sent).run());
    // Reads at 0, 15 and 30 s find the promotion refused.
    tokio::time::sleep(SLOT_READ * 5 / 2).await;
    *sent.unowned.lock().unwrap() = false;
    tokio::time::sleep(SLOT_READ).await;
    task.abort();
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let unowned: Vec<_> = raised.iter().filter(|alert| alert["alert"] == "liveness_key_holder_unknown").collect();
    assert_eq!(unowned.len(), 1, "{raised:?}");
    assert!(unowned[0]["message"].contains(&B256::repeat_byte(0xee).to_string()), "{unowned:?}");
    assert!(!raised.iter().any(|alert| alert["alert"] == "liveness_caller_send_failed"), "{raised:?}");
    let resolved: Vec<_> = std::iter::from_fn(|| resolutions.try_recv().ok()).collect();
    let resolution = resolved.iter().find(|alert| alert["alert_resolved"] == "liveness_key_holder_unknown");
    assert!(crate::alerts::resolution::ends_episode(unowned[0], resolution.expect("a resolution")), "{resolved:?}");
}

/// The refusal ends once no skip or promotion is due, though the chain still refuses them.
#[tokio::test(start_paused = true)]
async fn the_unowned_release_page_resolves_when_no_skip_or_promotion_is_due() {
    let (_guard, mut alerts, mut resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
    let sent = Sent { unowned: Arc::new(Mutex::new(true)), ..Default::default() };
    let task = tokio::spawn(unowned_caller(chain.clone(), &sent).run());
    tokio::time::sleep(SLOT_READ / 2).await;
    chain.set(|state| state.due_at = u64::MAX);
    tokio::time::sleep(SLOT_READ).await;
    task.abort();
    assert!(std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "liveness_key_holder_unknown"));
    let resolved: Vec<_> = std::iter::from_fn(|| resolutions.try_recv().ok()).collect();
    assert!(resolved.iter().any(|alert| alert["alert_resolved"] == "liveness_key_holder_unknown"), "{resolved:?}");
}

/// Any other revert, a promotion not yet due or no qualified position, keeps its debug log and pages nothing.
#[tokio::test(start_paused = true)]
async fn a_refusal_for_another_reason_pages_nothing() {
    let (_guard, mut alerts, _resolutions) = crate::test_support::capture_alerts_and_resolutions();
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
    let sent = Sent { refused: Arc::new(Mutex::new(true)), ..Default::default() };
    let task = tokio::spawn(unowned_caller(chain, &sent).run());
    tokio::time::sleep(SLOT_READ * 5 / 2).await;
    task.abort();
    assert_eq!(std::iter::from_fn(|| alerts.try_recv().ok()).count(), 0);
}
