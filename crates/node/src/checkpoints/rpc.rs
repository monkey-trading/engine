//! Checkpoint relaying through an operator-funded transaction RPC. The RPC cannot forge enclave
//! commitments. Reads currently trust this endpoint's canonical chain; RPC quorum is separate.

use std::future::IntoFuture;
use std::path::Path;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, Bytes, U64};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{Block, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::http::reqwest::{Certificate, Client, Url};
use anyhow::{Context, anyhow, ensure};
use engine_core::emissions::TokenUnits;
use tokio::time::timeout;

use crate::checkpoints::emission_budget::budget_from_cap;
use crate::checkpoints::{CheckpointChain, VaultState, View};
use crate::config::{CheckpointSection, FinalityTag};
use crate::custody::usdc_to_fixed;
use crate::log::{Checkpoint, CheckpointDomain};

mod submission;
use crate::checkpoints::rpc::submission::SubmissionStore;

sol! {
    function checkpointSigner() external view returns (address);
    function hasCheckpoint() external view returns (bool);
    function latestSeq() external view returns (uint64);
    function latestRoot() external view returns (bytes32);
    function depositCount() external view returns (uint256);
    function usdc() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function monkey() external view returns (address);
    function cap() external view returns (uint256);
    function postCheckpoint(uint64 seq, bytes32 root, bool hasParent, uint64 parentSeq, bytes32 parentRoot, bytes signature) external;
}

pub(crate) struct CheckpointRpc {
    client: RpcClient,
    url: Url,
    domain: CheckpointDomain,
    token: Address,
    relayer: Address,
    timeout: Duration,
    finality_tag: BlockNumberOrTag,
    confirmations: u64,
    submissions: SubmissionStore,
}

impl CheckpointRpc {
    /// Builds a client and checks existing nonce storage without requests or filesystem writes.
    /// Error cases:
    /// - the URL/certificates/client or an existing submission reservation is invalid
    pub fn new(
        cfg: &CheckpointSection,
        domain: CheckpointDomain,
        token: Address,
        finality: FinalityTag,
        confirmations: u64,
        log_dir: &Path,
        signer: engine_core::types::AccountId,
    ) -> anyhow::Result<Self> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()?;
        let http = Client::builder().tls_certs_only(roots).build()?;
        let url = cfg.rpc_url.expose()?;
        let client = RpcClient::new_http_with_client(http, url.clone());
        Ok(Self {
            client,
            url,
            domain,
            token,
            relayer: cfg.relayer,
            timeout: cfg.rpc_timeout,
            finality_tag: match finality {
                FinalityTag::Latest => BlockNumberOrTag::Latest,
                FinalityTag::Safe => BlockNumberOrTag::Safe,
                FinalityTag::Finalized => BlockNumberOrTag::Finalized,
            },
            confirmations,
            submissions: SubmissionStore::load(log_dir, domain, cfg.relayer, signer)?,
        })
    }

    /// Executes a read at one canonical block hash.
    /// Error cases:
    /// - transport, timeout, EVM, or canonical-block errors
    async fn call(&self, to: Address, input: Vec<u8>, block: BlockId) -> anyhow::Result<Bytes> {
        self.deadline(
            self.client.request("eth_call", (TransactionRequest::default().to(to).input(input.into()), block)),
        )
        .await
    }

    /// Bounds requests and redacts transport credentials from their errors.
    /// Error cases:
    /// - transport failure or timeout
    async fn deadline<T, E>(&self, future: impl IntoFuture<Output = Result<T, E>>) -> anyhow::Result<T>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match timeout(self.timeout, future.into_future()).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                let secrets = [
                    Some(self.url.as_str()),
                    Some(self.url.path()).filter(|path| path.len() > 1),
                    self.url.query(),
                    self.url.password(),
                ];
                let message = secrets
                    .into_iter()
                    .flatten()
                    .fold(error.to_string(), |message, secret| message.replace(secret, "<redacted>"));
                Err(anyhow!("checkpoint RPC failed: {message}"))
            }
            Err(_) => Err(anyhow!("checkpoint RPC timed out")),
        }
    }
}

impl CheckpointChain for CheckpointRpc {
    /// The emission period budget the vault's MONKEY cap was derived from. Both are immutable, so
    /// the latest block serves.
    /// Error cases:
    /// - transport, timeout, EVM, or decoding errors, as from a vault with no MONKEY token
    /// - a cap that no period budget the engine accepts gives (see `budget_from_cap`)
    async fn emission_budget(&self) -> anyhow::Result<TokenUnits> {
        let at = BlockId::Number(BlockNumberOrTag::Latest);
        let vault = Address::from(self.domain.vault.0);
        let monkey = monkeyCall::abi_decode_returns(&self.call(vault, monkeyCall {}.abi_encode(), at).await?)?;
        let cap = capCall::abi_decode_returns(&self.call(monkey, capCall {}.abi_encode(), at).await?)?;
        budget_from_cap(cap)
    }

    fn reserved_checkpoint(&self) -> anyhow::Result<Option<Checkpoint>> {
        Ok(self.submissions.read()?.map(|reservation| reservation.checkpoint))
    }

    /// Reads checkpoint state and collateral from one pinned canonical block.
    /// Error cases:
    /// - the endpoint is on the wrong chain or token, or an RPC/decoding error occurs
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        let chain_id: u64 = self.deadline(self.client.request::<_, U64>("eth_chainId", ())).await?.to();
        ensure!(chain_id == self.domain.chain_id, "checkpoint endpoint is on another chain");
        let tag = if view == View::Latest { BlockNumberOrTag::Latest } else { self.finality_tag };
        let tagged = self
            .deadline(self.client.request::<_, Option<Block>>("eth_getBlockByNumber", (tag, false)))
            .await?
            .context("checkpoint RPC has no finality block")?;
        // Follow immutable parent hashes from the exact sampled tag. A by-number read could
        // switch forks between requests and give an unconfirmed replacement the old tag's depth.
        let mut block = tagged;
        if view == View::Finalized {
            ensure!(block.header.number >= self.confirmations, "checkpoint chain has insufficient confirmation depth");
            for _ in 0..self.confirmations {
                let parent = self
                    .deadline(
                        self.client
                            .request::<_, Option<Block>>("eth_getBlockByHash", (block.header.parent_hash, false)),
                    )
                    .await?
                    .context("checkpoint confirmation ancestor is missing")?;
                ensure!(
                    parent.header.hash == block.header.parent_hash
                        && parent.header.number.checked_add(1) == Some(block.header.number),
                    "checkpoint confirmation ancestry is inconsistent"
                );
                block = parent;
            }
        }
        let at = BlockId::hash_canonical(block.header.hash);
        let vault = Address::from(self.domain.vault.0);
        let token = usdcCall::abi_decode_returns(&self.call(vault, usdcCall {}.abi_encode(), at).await?)?;
        ensure!(token == self.token, "checkpoint vault holds another token");
        let signer = checkpointSignerCall::abi_decode_returns(
            &self.call(vault, checkpointSignerCall {}.abi_encode(), at).await?,
        )?;
        let has =
            hasCheckpointCall::abi_decode_returns(&self.call(vault, hasCheckpointCall {}.abi_encode(), at).await?)?;
        let checkpoint = if has {
            let seq = latestSeqCall::abi_decode_returns(&self.call(vault, latestSeqCall {}.abi_encode(), at).await?)?;
            let root =
                latestRootCall::abi_decode_returns(&self.call(vault, latestRootCall {}.abi_encode(), at).await?)?;
            Some((seq, format!("{root:#x}")))
        } else {
            None
        };
        let balance = balanceOfCall::abi_decode_returns(
            &self.call(token, balanceOfCall { account: vault }.abi_encode(), at).await?,
        )?;
        let deposits =
            depositCountCall::abi_decode_returns(&self.call(vault, depositCountCall {}.abi_encode(), at).await?)?;
        Ok(VaultState {
            height: block.header.number,
            signer: engine_core::types::AccountId(signer.into()),
            checkpoint,
            balance: usdc_to_fixed(balance).context("vault balance exceeds Fixed")?,
            deposits: u64::try_from(deposits).context("vault deposit count exceeds u64")?,
        })
    }

    /// Relays at a durable explicit nonce; only later canonical state reads confirm inclusion.
    /// Error cases:
    /// - authorization, reservation durability/ownership, or an RPC request fails
    async fn post(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        self.post_reserved(cp).await
    }
}

#[cfg(test)]
mod tests;
