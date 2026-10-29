//! Signed stream connections (`signed=1`): each message goes out wrapped in a frame the enclave key
//! signs (`engine_wire::SignedFrame`), numbered from 0 on its connection, bound to the client's
//! challenge and the request that opened the connection, and chained to the frame before it. A
//! signed heartbeat naming the log head goes out at least every `api.heartbeat_interval`, so a
//! client can tell a quiet log from a held-back one. A frame is signed only at a time trusted time
//! bounds; with none, the connection closes, and the client can resume it.

use std::sync::Arc;

use engine_wire::{Connection, FRAME_VERSION, SignedFrame};
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::{Interval, MissedTickBehavior, interval};

use crate::api::Signing;
use crate::log::{Manifest, Offset};

/// One signed connection's framing state.
pub(super) struct Framing {
    signing: Arc<Signing>,
    connection: Connection,
    manifest: watch::Receiver<Manifest>,
    /// The next frame's place on the connection.
    n: u64,
    /// The digest of the last frame sent; zero before the first.
    previous: [u8; 32],
    heartbeats: Interval,
}

/// The message a heartbeat frame carries: the last log entry the node has flushed, or `null` before
/// the first.
#[derive(Serialize)]
#[serde(tag = "type", rename = "heartbeat")]
struct Heartbeat {
    head: Option<Offset>,
}

impl Framing {
    /// Framing for `connection`, with heartbeats naming the head `manifest` publishes; its first
    /// heartbeat is due at once.
    pub(super) fn new(signing: Arc<Signing>, connection: Connection, manifest: watch::Receiver<Manifest>) -> Self {
        let mut heartbeats = interval(signing.heartbeat_interval);
        // A subscriber that was slow to read gets one heartbeat, not a burst of missed ones.
        heartbeats.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self { signing, connection, manifest, n: 0, previous: [0; 32], heartbeats }
    }

    /// `msg` as the connection's next frame, signed over its place, the node's time, the
    /// connection, and the frame before it; `None`, leaving the place unused, while trusted time
    /// does not bound the host clock.
    pub(super) fn frame(&mut self, msg: String) -> Option<String> {
        let time = self.signing.time_ms()?;
        let (digest, sig) = self.signing.sign_frame(&self.connection, &self.previous, self.n, time, &msg);
        let frame = SignedFrame { v: FRAME_VERSION, n: self.n, time, msg, sig };
        (self.previous, self.n) = (digest, self.n + 1);
        Some(serde_json::to_string(&frame).expect("a frame of numbers and strings encodes"))
    }

    /// The next heartbeat frame, once it is due, or `None` as `frame`. Cancel-safe: the frame is
    /// made in the poll that finds the heartbeat due.
    pub(super) async fn next_heartbeat(&mut self) -> Option<String> {
        self.heartbeats.tick().await;
        let head = self.manifest.borrow().flushed;
        self.frame(serde_json::to_string(&Heartbeat { head }).expect("a heartbeat encodes"))
    }
}

#[cfg(test)]
mod tests;
