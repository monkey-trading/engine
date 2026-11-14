//! When a seat published its commands, on the wall clock: a switch's window runs from the
//! incumbent's last command to the successor's `TurnTaken`, while the scenario's test clock is warped.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use engine_core::command::Command;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::task::JoinHandle;

use crate::sequencer::{StreamBody, StreamMsg};

/// When the seat last published a command, and its first `TurnTaken`.
#[derive(Clone, Copy, Debug, Default)]
pub(in super::super) struct Stamps {
    pub(in super::super) last_command: Option<Instant>,
    pub(in super::super) turn_taken: Option<Instant>,
}

/// Follows `stream` from now, recording the stamps as its commands are published.
pub(super) fn track(stream: &broadcast::Sender<Arc<StreamMsg>>) -> (Arc<Mutex<Stamps>>, JoinHandle<()>) {
    let mut published = stream.subscribe();
    let stamps = Arc::new(Mutex::new(Stamps::default()));
    let recorded = stamps.clone();
    let task = tokio::spawn(async move {
        loop {
            match published.recv().await {
                Ok(message) => {
                    if let StreamBody::Command(command) = &message.body {
                        let (now, mut stamps) = (Instant::now(), recorded.lock().unwrap());
                        stamps.last_command = Some(now);
                        if matches!(command.sequenced.cmd, Command::TurnTaken { .. }) && stamps.turn_taken.is_none() {
                            stamps.turn_taken = Some(now);
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
    (stamps, task)
}
