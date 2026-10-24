//! Checkpoint relaying through an operator-funded transaction RPC. The RPC cannot forge enclave
//! commitments. Reads currently trust this endpoint's canonical chain; RPC quorum is separate.

use std::future::IntoFuture;
use std::path::Path;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, Bytes};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::transports::http::reqwest::{Certificate, Client, Url};
use anyhow::{Context, anyhow};
use tokio::time::timeout;

use crate::checkpoints::{AcceptedCheckpoint, ApprovalId, CheckpointChain, Observation, VaultState, View};
use crate::config::{CheckpointSection, FinalityTag};
use crate::log::{Checkpoint, CheckpointDomain};

mod authority;
mod observation;
mod submission;
use crate::checkpoints::rpc::submission::SubmissionStore;

sol! {
    function checkpointSigner() external view returns (address);
    function hasCheckpoint() external view returns (bool);
    function latestSeq() external view returns (uint64);
    function latestRoot() external view returns (bytes32);
    function depositCount() external view returns (uint256);
    function signerEpoch() external view returns (uint64);
    function signerAtEpoch(uint64 epoch) external view returns (address);
    function totalWithdrawn() external view returns (uint256);
    function usedNonces(address account, uint64 nonce) external view returns (bool);
    function hasCheckpointAt(uint64 seq) external view returns (bool);
    function checkpointRoots(uint64 seq) external view returns (bytes32);
    function checkpointSignerAt(uint64 seq) external view returns (address);
    function checkpointEpochAt(uint64 seq) external view returns (uint64);
    function usdc() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
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
    submissions: Option<SubmissionStore>,
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
        let mut client = Self::read_only(cfg, domain, token, finality, confirmations)?;
        client.submissions = Some(SubmissionStore::pending(log_dir, domain, cfg.relayer, signer)?);
        Ok(client)
    }

    /// Builds an audit client without opening or trusting any local posting journal.
    /// Error cases:
    /// - URL, certificate, or HTTP client construction fails
    pub(crate) fn read_only(
        cfg: &CheckpointSection,
        domain: CheckpointDomain,
        token: Address,
        finality: FinalityTag,
        confirmations: u64,
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
            submissions: None,
        })
    }

    /// Grants posting access only to clients constructed with a durable reservation store.
    /// Error cases:
    /// - the client was constructed for read-only audit
    fn submissions(&self) -> anyhow::Result<&SubmissionStore> {
        self.submissions.as_ref().context("read-only checkpoint client cannot access posting reservations")
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
    fn reserved_checkpoint(&self) -> anyhow::Result<Option<Checkpoint>> {
        Ok(self.submissions()?.read()?.map(|reservation| reservation.checkpoint))
    }

    /// Reads checkpoint state and collateral from one pinned canonical block.
    /// Error cases:
    /// - the endpoint is on the wrong chain or token, or an RPC/decoding error occurs
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        Ok(self.read_observation(view, &[]).await?.state)
    }

    async fn observe(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        self.read_observation(view, approvals).await
    }

    async fn accepted_checkpoints(
        &self,
        at: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
        self.read_accepted_checkpoints(at, sequences).await
    }

    async fn epoch_signers(
        &self,
        at: &Observation,
        epochs: &[u64],
    ) -> anyhow::Result<Vec<engine_core::types::AccountId>> {
        self.read_epoch_signers(at, epochs).await
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
