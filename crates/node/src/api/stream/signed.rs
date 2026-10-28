//! Signed stream connections (`signed=1`): each message goes out wrapped in a frame the enclave key
//! signs (`engine_wire::SignedFrame`), numbered from 0 on its connection and bound to the client's
//! challenge, and a signed heartbeat naming the log head goes out at least every
//! `api.heartbeat_interval`, so a client can tell a quiet log from a held-back one.

use std::sync::Arc;

use engine_core::types::Millis;
use engine_wire::{FRAME_VERSION, SignedFrame};
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::{Interval, MissedTickBehavior, interval};

use crate::api::{ApiState, Signing};
use crate::log::{Manifest, Offset};

/// One signed connection's framing state.
pub(super) struct Framing {
    signing: Arc<Signing>,
    challenge: [u8; 32],
    clock: fn() -> Millis,
    manifest: watch::Receiver<Manifest>,
    /// The next frame's place on the connection.
    n: u64,
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
    /// Framing for a connection opened with `challenge`; its first heartbeat is due at once.
    pub(super) fn new(s: &ApiState, challenge: [u8; 32]) -> Self {
        let mut heartbeats = interval(s.signing.heartbeat_interval);
        // A subscriber that was slow to read gets one heartbeat, not a burst of missed ones.
        heartbeats.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let (signing, clock, manifest) = (s.signing.clone(), s.clock, s.manifest.clone());
        Self { signing, challenge, clock, manifest, n: 0, heartbeats }
    }

    /// `msg` as the connection's next frame, signed over its place, the node's time, and the
    /// challenge.
    pub(super) fn frame(&mut self, msg: String) -> String {
        let time = u64::try_from((self.clock)()).expect("the node's clock reads after the epoch");
        let sig = self.signing.sign_frame(&self.challenge, self.n, time, &msg);
        let frame = SignedFrame { v: FRAME_VERSION, n: self.n, time, msg, sig };
        self.n += 1;
        serde_json::to_string(&frame).expect("a frame of numbers and strings encodes")
    }

    /// The next heartbeat frame, once it is due. Cancel-safe: the frame is made in the poll that
    /// finds the heartbeat due.
    pub(super) async fn next_heartbeat(&mut self) -> String {
        self.heartbeats.tick().await;
        let head = self.manifest.borrow().flushed;
        self.frame(serde_json::to_string(&Heartbeat { head }).expect("a heartbeat encodes"))
    }
}

#[cfg(test)]
mod tests;
