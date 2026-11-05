//! Sending a scenario transaction through Anvil's unlocked accounts and waiting for its receipt.

use std::time::Duration;

use alloy::primitives::B256;
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use tokio::time::sleep;

/// The gas limit given a transaction that sets none. Anvil gives one without a limit the block's
/// whole 30M, which no other transaction can share a block with: when a second is ready at the same
/// moment (the checkpoint poster's, say), the auto-miner mines one and leaves the other pending until
/// a later transaction arrives, so a receipt wait can last its whole bound. A transaction this size
/// shares a block with any other scenario transaction, and still covers the largest deployment.
const SCENARIO_GAS: u64 = 10_000_000;

/// Sends through Anvil's unlocked account and polls its receipt under the scenario deadline, which
/// bounds a hang on a loaded runner, not a transaction Anvil leaves unmined (see `SCENARIO_GAS`).
pub(crate) async fn transact(provider: &RpcClient, transaction: TransactionRequest) -> TransactionReceipt {
    let transaction = TransactionRequest { gas: transaction.gas.or(Some(SCENARIO_GAS)), ..transaction };
    let hash: B256 = provider.request("eth_sendTransaction", (transaction,)).await.unwrap();
    crate::test_support::wait_for("the transaction receipt", async {
        loop {
            if let Some(receipt) =
                provider.request::<_, Option<TransactionReceipt>>("eth_getTransactionReceipt", (hash,)).await.unwrap()
            {
                return receipt;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::process::{Command as Process, Stdio};

    use alloy::primitives::{Address, U64};

    use super::*;
    use crate::checkpoints::scenario::anvil::Anvil;

    /// Anvil's block gas limit, and so the gas it gives a transaction that sets none.
    const BLOCK_GAS: u64 = 30_000_000;

    #[tokio::test]
    #[ignore = "requires Anvil; scripts/dev-checkpoints.sh runs it"]
    async fn two_transactions_sent_together_share_a_block_instead_of_one_waiting_unmined() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let _anvil = Anvil(
            Process::new("anvil")
                .args(["--port", &port.to_string(), "--silent"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Anvil installed"),
        );
        let provider = RpcClient::new_http(format!("http://127.0.0.1:{port}").parse().unwrap());
        crate::test_support::wait_for("Anvil to answer eth_chainId", async {
            while provider.request::<_, U64>("eth_chainId", ()).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await;
        let accounts: Vec<Address> = provider.request("eth_accounts", ()).await.unwrap();
        // Calldata makes Anvil's own gas choice the block's whole limit; the pair is sent at once.
        let send = |from: Address| {
            let request = TransactionRequest::default().from(from).to(accounts[2]).input(vec![1u8; 64].into());
            transact(&provider, request)
        };
        let both = async { tokio::join!(send(accounts[0]), send(accounts[9])) };
        let (first, second) = tokio::time::timeout(Duration::from_secs(20), both)
            .await
            .expect("both transactions are mined, not one left pending");
        for receipt in [first, second] {
            let sent: serde_json::Value =
                provider.request("eth_getTransactionByHash", (receipt.transaction_hash,)).await.unwrap();
            let gas = u64::from_str_radix(sent["gas"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
            assert!(gas <= BLOCK_GAS / 2, "a transaction sharing a block holds {gas} of its {BLOCK_GAS} gas");
        }
    }
}
