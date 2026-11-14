//! The liveness caller: what a read calls for, and what it sends.

use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;

use super::*;
use crate::liveness::fake::{ChainState, DOMAIN, FakeChain};
use crate::log_heads::{Head, HeadV2, Hold};
use crate::test_support::capture_alerts;

/// A call sent: where to, and its data.
type SentCall = (Address, Vec<u8>);

/// The calls sent, and what the endpoint answers.
#[derive(Clone, Default)]
struct Sent {
    calls: Arc<Mutex<Vec<SentCall>>>,
    balance: Arc<Mutex<U256>>,
    /// Estimates refused: the chain would refuse every call.
    refused: Arc<Mutex<bool>>,
}

impl Sending for Sent {
    async fn send(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<B256>> {
        if *self.refused.lock().unwrap() {
            return Ok(None);
        }
        self.calls.lock().unwrap().push((to, data));
        Ok(Some(B256::ZERO))
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        Ok(*self.balance.lock().unwrap())
    }
}

#[test]
fn a_read_calls_for_a_witness_while_the_slot_is_unmarked_and_a_promotion_once_due() {
    let mut view = view(ChainState::default());
    // The block's slot is 30,000,000; the latest mark is the slot before it.
    assert_eq!(calls(&view, None), vec![Call::Witness]);
    view.marked_slot = view.time / 60;
    assert!(calls(&view, None).is_empty(), "marked already");
    view.due_at = view.time;
    assert_eq!(calls(&view, None), vec![Call::Promote]);
    view.generation = 0;
    assert!(calls(&view, None).is_empty(), "no turn names the operator");
}

#[test]
fn a_read_calls_for_a_relay_once_the_chains_head_is_stale_and_a_newer_one_was_signed() {
    let mut view = view(ChainState { head: (7, 1_000, 1_800_000_000 - 39), ..Default::default() });
    view.marked_slot = view.time / 60;
    let newer = signed(1, 8);
    assert!(calls(&view, Some(newer.clone())).is_empty(), "posted 39 s ago: not yet stale");
    view.head.posted_at = view.time - 40;
    assert_eq!(calls(&view, Some(newer.clone())), vec![Call::Relay(Box::new(newer))]);
    assert!(calls(&view, Some(signed(1, 7))).is_empty(), "the head the chain holds already");
    assert!(calls(&view, Some(signed(2, 9))).is_empty(), "another epoch's head counts for no one");
    let mut held = signed(1, 7);
    held.head.signed_at = 2_000;
    held.head.hold = Hold::BenchmarkSync;
    assert_eq!(calls(&view, Some(held.clone())), vec![Call::Relay(Box::new(held))], "a held repeat, signed later");
}

#[tokio::test(start_paused = true)]
async fn the_caller_witnesses_promotes_and_relays_from_its_own_account() {
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
    let copy = Copy::default();
    let relay = signed(1, 3);
    copy.signed_head(relay.clone());
    let sent = Sent { balance: Arc::new(Mutex::new(U256::from(10u64.pow(18)))), ..Default::default() };
    let caller = Caller { chain, sending: sent.clone(), domain: DOMAIN, copy, min_balance: U256::from(10u64.pow(16)) };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(POLL).await;
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
    let caller =
        Caller { chain, sending: sent.clone(), domain: DOMAIN, copy: Copy::default(), min_balance: U256::from(10) };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(POLL * 3).await;
    *sent.balance.lock().unwrap() = U256::from(10);
    tokio::time::sleep(POLL * 2).await;
    task.abort();
    let raised: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
    let unfunded = raised.iter().filter(|alert| alert["alert"] == "liveness_caller_unfunded").count();
    assert_eq!(unfunded, 1, "{raised:?}");
}

#[tokio::test(start_paused = true)]
async fn a_call_the_chain_would_refuse_is_not_sent() {
    let chain = Arc::new(FakeChain::new(ChainState { due_at: 1_800_000_000, ..Default::default() }));
    let sent = Sent { refused: Arc::new(Mutex::new(true)), ..Default::default() };
    let caller =
        Caller { chain, sending: sent.clone(), domain: DOMAIN, copy: Copy::default(), min_balance: U256::ZERO };
    let task = tokio::spawn(caller.run());
    tokio::time::sleep(POLL * 2).await;
    task.abort();
    assert!(sent.calls.lock().unwrap().is_empty());
}

// ── Helpers ──

/// The view `state` reads.
fn view(state: ChainState) -> View {
    let chain = FakeChain::new(state);
    let block = futures_util::FutureExt::now_or_never(chain.latest()).unwrap().unwrap();
    futures_util::FutureExt::now_or_never(crate::liveness::chain::view(&block, &DOMAIN)).unwrap().unwrap()
}

/// A head at `seq` signed under `epoch`, at signing time 1,000.
fn signed(epoch: u64, seq: u64) -> SignedHead {
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
