//! What one large log entry costs to put on every signed subscriber's stream: the daily rollover's
//! entry is about 48 MB of events at #905's launch book. "Per subscriber" encodes, hashes and escapes
//! it for each connection, as the stream did before; "shared" does that once and frames it per
//! connection. Run it in a quiet CPU window:
//! `cargo test -p engine-node --lib api::stream::signed::bench -- --ignored --nocapture`.

use super::*;
use std::hint::black_box;
use std::time::Instant;

use engine_wire::Connection;
use tokio::sync::watch;

use crate::log::Manifest;
use crate::log::Offset;
use crate::sequencer::{StreamBody, StreamMsg};
use crate::test_support::{TestSequencer, deposit, log_messages, scratch_dir, signing};

const TARGET_BYTES: usize = 48 << 20;

/// A command entry whose events repeat the first logged command's until its JSON is `TARGET_BYTES`.
async fn rollover_sized() -> StreamMsg {
    let dir = scratch_dir("stream-frame-bench");
    let node = TestSequencer::start_legacy(&dir, 64);
    node.submit(deposit(1, "dep")).await;
    let message = log_messages(&dir, Offset::start_of(0)).into_iter().find(|m| m.contains(r#""events":[{"#)).unwrap();
    let StreamMsg { offset, body: StreamBody::Command(mut entry), .. } = serde_json::from_str(&message).unwrap() else {
        unreachable!("the first message with events is a command")
    };
    let events = entry.events.take().unwrap();
    let one = serde_json::to_string(&events).unwrap().len();
    entry.events = Some(events.iter().cloned().cycle().take(events.len() * (TARGET_BYTES / one + 1)).collect());
    StreamMsg::from_body(offset, StreamBody::Command(entry))
}

fn framing() -> Framing {
    let (_, manifest) = watch::channel(Manifest::default());
    Framing::new(signing(), Connection::new([7; 32], "/v1/stream?signed=1"), manifest, false)
}

#[tokio::test]
#[ignore = "benchmark: run in a quiet CPU window"]
async fn rollover_frame_cost() {
    let msg = rollover_sized().await;
    for subscribers in [1usize, 256] {
        let mut framings: Vec<Framing> = (0..subscribers).map(|_| framing()).collect();
        let began = Instant::now();
        let mut bytes = 0;
        for framing in &mut framings {
            let json = serde_json::to_string(&msg).unwrap();
            bytes = framing.frame(json).unwrap().len();
            black_box(&bytes);
        }
        let per_subscriber = began.elapsed();

        let mut framings: Vec<Framing> = (0..subscribers).map(|_| framing()).collect();
        let shared_msg = StreamMsg::from_body(msg.offset, msg.body.clone());
        let began = Instant::now();
        for framing in &mut framings {
            let framed = shared_msg.framed().unwrap();
            black_box(framing.frame_parts(&framed.sha256, &framed.escaped).unwrap());
        }
        let shared = began.elapsed();
        println!(
            "FRAME rollover subscribers={subscribers} json_bytes={} frame_bytes={bytes} per_subscriber_ms={} shared_ms={}",
            shared_msg.json().unwrap().len(),
            per_subscriber.as_millis(),
            shared.as_millis()
        );
    }
}
