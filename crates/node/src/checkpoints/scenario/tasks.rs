//! The node tasks the checkpoint scenario runs, and waits that report why the poster made no
//! progress.

use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::test_support::{Alert, LOADED_WAIT};

/// The sequencer, checkpoint poster, and custody tasks every later wait depends on.
pub(super) struct Tasks {
    pub sequencer: JoinHandle<anyhow::Result<()>>,
    pub poster: JoinHandle<anyhow::Result<()>>,
    pub custody: JoinHandle<anyhow::Result<()>>,
}

impl Tasks {
    /// Panics with the outcome of a task that has ended: nothing restarts it, so a wait on its
    /// work would otherwise run out its whole ceiling.
    async fn ensure_running(&mut self) {
        for (name, task) in [
            ("sequencer", &mut self.sequencer),
            ("checkpoint poster", &mut self.poster),
            ("custody", &mut self.custody),
        ] {
            if task.is_finished() {
                panic!("the {name} task ended: {:?}", task.await);
            }
        }
    }

    /// Polls `check` every 40 ms with the alerts raised since its last poll until it returns
    /// `Ok`. A task that ends fails the wait at once; a timeout reports what `check` last found
    /// missing and the poster's last checkpoint or collateral alert.
    pub async fn wait(
        &mut self,
        what: &str,
        alerts: &mut UnboundedReceiver<Alert>,
        mut check: impl AsyncFnMut(&[Alert]) -> Result<(), String>,
    ) {
        let (mut missing, mut poster) = (String::new(), None);
        let waited = timeout(LOADED_WAIT, async {
            loop {
                let raised: Vec<Alert> = std::iter::from_fn(|| alerts.try_recv().ok()).collect();
                let latest = raised.iter().rev().find(|alert| {
                    alert
                        .get("alert")
                        .is_some_and(|name| name.starts_with("checkpoint_") || name.starts_with("collateral_"))
                });
                poster = latest.cloned().or(poster.take());
                match check(&raised).await {
                    Ok(()) => return,
                    Err(state) => missing = state,
                }
                self.ensure_running().await;
                sleep(Duration::from_millis(40)).await;
            }
        })
        .await;
        assert!(
            waited.is_ok(),
            "gave up after {LOADED_WAIT:?} waiting for {what}: {missing}; the poster's last alert: {poster:?}"
        );
    }
}
