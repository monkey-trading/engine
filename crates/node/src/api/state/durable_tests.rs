//! What a client is shown waits for the log to hold the state it was read from. The log's head is a
//! watch the test moves, so no test sleeps to order events.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

use super::*;
use crate::api::tests::api_state;
use crate::log::{Manifest, Offset};
use crate::stats::tests::Rig;

/// A state whose log head is `seq`, with the sender that moves it.
fn state_at(seq: u64) -> (ApiState, mpsc::Receiver<sequencer::Request>, watch::Sender<Manifest>) {
    let (mut state, requests) = api_state(true);
    let (head, manifest) = watch::channel(Manifest::default());
    head.send_modify(|log| log.flushed = Some(Offset { seq, index: 0 }));
    state.manifest = manifest;
    (state, requests, head)
}

/// Whether `pending` has no answer yet, after the others have had every chance to give one.
async fn unanswered<T>(pending: &mut tokio::task::JoinHandle<T>) -> bool {
    timeout(Duration::from_millis(150), pending).await.is_err()
}

#[tokio::test]
async fn an_answer_waits_until_the_log_holds_the_state_it_read() {
    let (state, _requests, head) = state_at(3);
    let mut waiting = tokio::spawn(async move { state.durable(Some(5)).await });
    assert!(unanswered(&mut waiting).await, "the log is at 3");
    head.send_modify(|log| log.flushed = Some(Offset { seq: 5, index: 1 }));
    waiting.await.unwrap().expect("answered once the log holds seq 5");
}

#[tokio::test]
async fn a_query_view_and_a_fallback_view_wait_for_the_engine_state_they_ran_on() {
    let engine = Rig::new().engine;
    let seq = engine.last_seq().expect("the rig has applied commands");
    let (state, mut requests, head) = state_at(seq - 1);
    // The sequencer, answering every query from an engine a batch ahead of the log.
    tokio::spawn(async move {
        while let Some(sequencer::Request::Query(query)) = requests.recv().await {
            query(&engine);
        }
    });
    let (proof, view) = (state.clone(), state);
    let mut proved = tokio::spawn(async move { proof.query_durable(|e| e.last_seq()).await });
    let mut viewed = tokio::spawn(async move { view.read(|e| e.last_seq()).await });
    assert!(unanswered(&mut proved).await, "a proof waits");
    assert!(unanswered(&mut viewed).await, "a view that fell back to the sequencer waits");
    head.send_modify(|log| log.flushed = Some(Offset { seq, index: 0 }));
    assert_eq!(proved.await.unwrap().ok(), Some(Some(seq)));
    assert_eq!(viewed.await.unwrap().ok(), Some(Some(seq)));
}
