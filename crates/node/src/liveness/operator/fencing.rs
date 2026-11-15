//! The operator's fence tick: the fence read against trusted time, apart from the rule's reads.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info};

use crate::liveness::fence::Fence;
use crate::liveness::operator::FENCE_TICK;
use crate::liveness::{Gate, MAX_SLOW_PPM};
use crate::trusted_time::TrustedClock;

/// What the fence tick reads: whether the rule applies, the fence, and refused parameters.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FenceState {
    /// `None` before the first read; `Some(false)` while the rule does not apply.
    pub(super) applies: Option<bool>,
    pub(super) fence: Fence,
    /// Why the rule's parameters are refused, if they are.
    pub(super) refused: Option<&'static str>,
}

/// Aborts the fence tick when the rule stops.
pub(super) struct AbortOnDrop(pub(super) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Checks the fence against trusted time every `FENCE_TICK`, closing the gate once the enclave's
/// upper bound on true time reaches `E - DELTA` and opening it on a later `E`, and alerting on each
/// change. A read under which the rule does not apply leaves it open until the next turn boundary. The
/// gate also reads the deadline against trusted time at every check (`Gate::deadline`), so a tick the
/// host delays opens nothing past it.
pub(super) async fn fence_tick(state: Arc<Mutex<FenceState>>, gate: Gate, trusted: TrustedClock) {
    let mut tick = interval(FENCE_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut fenced = false;
    loop {
        tick.tick().await;
        let FenceState { applies, fence, refused } = *lock(&state);
        let now = match applies {
            None => true,
            Some(false) => fence.fenced(trusted.upper_bound_ms(MAX_SLOW_PPM)),
            Some(true) => refused.is_some() || fence.fenced(trusted.upper_bound_ms(MAX_SLOW_PPM)),
        };
        gate.fence(now);
        match (now, std::mem::replace(&mut fenced, now)) {
            (true, false) if applies.is_some() => error!(
                alert = "liveness_fenced",
                due_ms = ?fence.due_ms(),
                replaced = fence.replaced(),
                "the operator stops by its own clock: a promotion may pass within the stop margin; no command, \
                 answer or head is signed until a later read moves it on"
            ),
            (false, true) => info!(alert_resolved = "liveness_fenced", "a later read moved the earliest promotion on"),
            _ => {}
        }
    }
}

/// The fence state, whatever a panic left.
pub(super) fn lock(state: &Mutex<FenceState>) -> std::sync::MutexGuard<'_, FenceState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}
