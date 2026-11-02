//! The node tasks the checkpoint scenario runs, and waits that report why the poster made no
//! progress.

use std::time::Duration;

use alloy::primitives::Address;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::checkpoints::{CheckpointChain, CheckpointRpc, Poster, View};
use crate::log::Checkpoint;
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
                self.ensure_running().await;
                match check(&raised).await {
                    Ok(()) => return,
                    Err(state) => missing = state,
                }
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

/// Awaits canonical inclusion explicitly; transaction RPC acknowledgment is not a mined receipt.
/// The post is retried while awaiting, exactly as the poster worker retries, and a failed
/// attempt mines one block: a post blocked on confirmation depth — a reverted re-send the
/// recovery must reprove finalized, a predecessor still shy of the finalized tag — only
/// finalizes while the chain advances, which a production chain's traffic does continuously.
pub(super) async fn include_checkpoint(url: &str, poster: &Poster<CheckpointRpc>, checkpoint: &Checkpoint) {
    let deadline = std::time::Instant::now() + LOADED_WAIT;
    let mut last_error = String::new();
    loop {
        if poster.chain.state(View::Latest).await.unwrap().checkpoint == Some((checkpoint.seq, checkpoint.root.clone()))
        {
            return;
        }
        if let Err(error) = poster.chain.post(checkpoint).await {
            last_error = format!("{error:#}");
            mine_block(url).await;
        }
        assert!(std::time::Instant::now() < deadline, "the checkpoint was not included: {last_error}");
        sleep(Duration::from_millis(20)).await;
    }
}

/// Mines one empty block on the scenario's Anvil; a wait that is not itself sending keeps the
/// confirmation depth a blocked post depends on moving.
async fn mine_block(url: &str) {
    let response: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"anvil_mine","params":["0x1"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
}

/// Requires every transaction `relayer` has mined to have succeeded: a post that runs out of gas
/// spends a nonce and commits nothing.
pub(super) async fn assert_no_post_reverted(url: &str, relayer: Address) {
    let head = local_rpc(url, "eth_blockNumber", serde_json::json!([])).await;
    let head = u64::from_str_radix(head.as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
    for number in 1..=head {
        let block = local_rpc(url, "eth_getBlockByNumber", serde_json::json!([format!("{number:#x}"), true])).await;
        for tx in block["transactions"].as_array().unwrap() {
            if tx["from"].as_str().unwrap().parse::<Address>().unwrap() == relayer {
                let receipt = local_rpc(url, "eth_getTransactionReceipt", serde_json::json!([tx["hash"]])).await;
                assert_eq!(receipt["status"], "0x1", "a checkpoint post reverted: {tx} {receipt}");
            }
        }
    }
}

/// Executes only local Anvil scenario controls and rejects JSON-RPC errors.
pub(super) async fn local_rpc(url: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let response: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response.get("result").unwrap().clone()
}
