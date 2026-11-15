//! The liveness caller: what a read calls for, and what it sends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;

use super::*;
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain};
use crate::log_heads::{Head, HeadV2, Hold};
use crate::test_support::capture_alerts;

/// How often the caller polls in these tests.
const TEST_POLL: Duration = Duration::from_millis(50);

/// A call sent: where to, and its data.
type SentCall = (Address, Vec<u8>);

/// The calls sent, and what the endpoint answers.
#[derive(Clone, Default)]
struct Sent {
    calls: Arc<Mutex<Vec<SentCall>>>,
    balance: Arc<Mutex<U256>>,
    /// Estimates refused: the chain would refuse every call.
    refused: Arc<Mutex<bool>>,
    /// The endpoint fails every request, as one down or not signing for the caller does.
    down: Arc<Mutex<bool>>,
}

impl Sending for Sent {
    async fn send(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<B256>> {
        anyhow::ensure!(!*self.down.lock().unwrap(), "liveness RPC failed: connection refused");
        if *self.refused.lock().unwrap() {
            return Ok(None);
        }
        self.calls.lock().unwrap().push((to, data));
        Ok(Some(B256::ZERO))
    }

    async fn balance(&self) -> anyhow::Result<U256> {
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

#[test]
fn a_read_calls_for_a_relay_once_the_chains_head_is_stale_and_a_newer_one_was_signed() {
    let mut view = view(ChainState { head: (7, 1_000, 1_800_000_000 - 39), ..Default::default() });
    view.marked_slot = view.time / 60;
    let newer = signed(1, 8);
    assert!(calls(&view, Some(newer.clone()), 0).is_empty(), "posted 39 s ago: not yet stale");
    view.head.posted_at = view.time - 40;
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
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
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
    };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL).await;
    task.abort();
    let calls = sent.calls.lock().unwrap().clone();
    assert!(calls.contains(&(DOMAIN.standby.schedule, witnessCall {}.abi_encode())), "{calls:?}");
    assert!(calls.contains(&(DOMAIN.standby.schedule, promoteCall {}.abi_encode())));
    let relayed = relay.head.calldata(&relay.signature).unwrap();
    assert!(calls.contains(&(DOMAIN.log_heads, relayed)), "the stream's head goes to LogHeads");
}

#[tokio::test(start_paused = true)]
async fn a_caller_below_its_minimum_balance_pages_once_until_funded() {
    let (_guard, mut alerts) = capture_alerts();
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
    };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL * 3).await;
    *sent.balance.lock().unwrap() = U256::from(10);
    tokio::time::sleep(TEST_POLL * 2).await;
    task.abort();
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let unfunded = raised.iter().filter(|alert| alert["alert"] == "liveness_caller_unfunded").count();
    assert_eq!(unfunded, 1, "{raised:?}");
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
    };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(TEST_POLL * 3).await;
    *sent.down.lock().unwrap() = false;
    tokio::time::sleep(TEST_POLL * 2).await;
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
