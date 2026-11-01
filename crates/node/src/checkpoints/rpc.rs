//! Checkpoint relaying through an operator-funded transaction RPC. The RPC cannot forge enclave
//! commitments. Reads currently trust this endpoint's canonical chain; RPC quorum is separate.

use std::future::IntoFuture;
use std::path::Path;
use std::time::Duration;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, B256, Bytes};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::http::reqwest::{Certificate, Client, Url};
use anyhow::{Context, anyhow, ensure};
use engine_core::emissions::TokenUnits;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::checkpoints::emission_budget::budget_from_cap;
use crate::checkpoints::{
    AcceptedCheckpoint, ApprovalId, ApprovalStatus, CheckpointChain, CurrentRelease, HandoverSource, LifecycleEvidence,
    Observation, VaultState, View,
};
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
    function withdrawalsPaused() external view returns (bool);
    function totalWithdrawn() external view returns (uint256);
    function totalDeposited() external view returns (uint256);
    function depositedThrough(uint256 id) external view returns (uint256);
    function usedNonces(address account, uint64 nonce) external view returns (bool);
    function hasCheckpointAt(uint64 seq) external view returns (bool);
    function checkpointRoots(uint64 seq) external view returns (bytes32);
    function checkpointSignerAt(uint64 seq) external view returns (address);
    function checkpointEpochAt(uint64 seq) external view returns (uint64);
    function distributionsPaid() external view returns (uint256);
    function usdc() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function monkey() external view returns (address);
    function cap() external view returns (uint256);
    function exitStartedAt() external view returns (uint64);
    function postCheckpoint(uint64 seq, bytes32 root, bool hasParent, uint64 parentSeq, bytes32 parentRoot, bytes signature) external;
    function currentRelease() external view returns (bytes32);
    function releaseGeneration() external view returns (uint64);
    function syncedRelease() external view returns (bytes32);
    function registry() external view returns (address);
    function rotationEnabled() external view returns (bool);
    function lastCheckpointAt() external view returns (uint64);
    function pendingElection()
        external
        view
        returns (uint64 epoch, address candidate, address adminKey, bytes32 release, uint64 effectiveAt);
    function operatorInEffect() external view returns (address candidate, address adminKey, bytes32 release, uint64 since);
    function pendingPromotion() external view returns (bool);
    function handoverSource()
        external
        view
        returns (address candidate, bytes32 release, string apiHostname, string handoverEndpoint);
    function candidateOf(address candidate)
        external
        view
        returns (
            address adminKey,
            address enclave,
            uint256 bond,
            bytes32 release,
            uint64 registration,
            string apiHostname,
            string handoverEndpoint
        );
    function parameters()
        external
        view
        returns (
            uint64 epochLength,
            uint64 minTerm,
            uint64 finalityMargin,
            uint16 marginBps,
            uint16 eligibilityBps,
            uint16 quorumBps,
            uint16 maxCandidates,
            uint256 minSelfBond,
            uint64 livenessTimeout
        );
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

/// Why a standby takes over: the vault's latest accepted checkpoint — the anchor the published
/// log must extend — and the signer the vault recorded there, whose later published checkpoints
/// the copy keeps. Serialized into the copy's `takeover.json` marker so an interrupted
/// finalization repairs on restart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Takeover {
    pub seq: u64,
    pub root: String,
    pub signer: engine_core::types::AccountId,
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
        let http = Client::builder()
            // A provider can refuse a request without one, and the client sends none by default.
            .user_agent(concat!("engine-checkpoints/", env!("CARGO_PKG_VERSION")))
            .tls_certs_only(roots)
            .build()?;
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
    /// Why a standby takes over: the registry named `candidate`'s key as the pending or current
    /// operator after the vault has had no checkpoint for the liveness timeout. Every read is
    /// pinned to one finalized block, so the decision to delete local history can never rest on
    /// a reorgable or self-inconsistent view. The signal names the vault's latest checkpoint —
    /// the anchor the published log must extend — and the signer the vault recorded there, whose
    /// later published checkpoints the copy keeps.
    /// Error cases:
    /// - an RPC call or decoding fails
    pub(crate) async fn takeover(&self, candidate: engine_core::types::AccountId) -> anyhow::Result<Option<Takeover>> {
        // One canonical block pins every read below, including the liveness clock.
        let block: serde_json::Value =
            self.deadline(self.client.request("eth_getBlockByNumber", (BlockNumberOrTag::Finalized, false))).await?;
        let hash: B256 = block
            .get("hash")
            .and_then(|value| value.as_str())
            .context("the finalized block has no hash")?
            .parse()
            .context("decoding the finalized block hash")?;
        let time = u64::from_str_radix(
            block.get("timestamp").and_then(|value| value.as_str()).context("no timestamp")?.trim_start_matches("0x"),
            16,
        )?;
        let at = BlockId::hash(hash);
        let vault = Address::from(self.domain.vault.0);
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), at).await?)?;
        if registry.is_zero()
            || !rotationEnabledCall::abi_decode_returns(
                &self.call(registry, rotationEnabledCall {}.abi_encode(), at).await?,
            )?
        {
            return Ok(None);
        }
        let pending = pendingElectionCall::abi_decode_returns(
            &self.call(registry, pendingElectionCall {}.abi_encode(), at).await?,
        )?;
        let in_effect = operatorInEffectCall::abi_decode_returns(
            &self.call(registry, operatorInEffectCall {}.abi_encode(), at).await?,
        )?;
        if (pending.effectiveAt == 0 || pending.adminKey != candidate.0) && in_effect.adminKey != candidate.0 {
            return Ok(None);
        }
        // The incumbent still checkpointing means a normal election, which hands the keys over
        // (#44); the standby takes over only after the liveness timeout, as `promote` on chain
        // already required.
        let liveness =
            parametersCall::abi_decode_returns(&self.call(registry, parametersCall {}.abi_encode(), at).await?)?
                .livenessTimeout;
        let last = lastCheckpointAtCall::abi_decode_returns(
            &self.call(vault, lastCheckpointAtCall {}.abi_encode(), at).await?,
        )?;
        if time < last.saturating_add(liveness) {
            return Ok(None);
        }
        let has =
            hasCheckpointCall::abi_decode_returns(&self.call(vault, hasCheckpointCall {}.abi_encode(), at).await?)?;
        ensure!(has, "the vault holds no checkpoint; a takeover has nothing to resume from");
        let seq = latestSeqCall::abi_decode_returns(&self.call(vault, latestSeqCall {}.abi_encode(), at).await?)?;
        let root = latestRootCall::abi_decode_returns(&self.call(vault, latestRootCall {}.abi_encode(), at).await?)?;
        let signer = checkpointSignerAtCall::abi_decode_returns(
            &self.call(vault, checkpointSignerAtCall { seq }.abi_encode(), at).await?,
        )?;
        Ok(Some(Takeover {
            seq,
            root: format!("0x{}", hex::encode(root.0)),
            signer: engine_core::types::AccountId(signer.0.0),
        }))
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

    async fn deposited_through(&self, at: &Observation, count: u64) -> anyhow::Result<engine_core::Fixed> {
        self.read_deposited_through(at, count).await
    }

    async fn current_release(&self, at: &Observation) -> anyhow::Result<CurrentRelease> {
        self.read_current_release(at).await
    }

    fn reads_release_fold(&self) -> bool {
        true
    }

    async fn synced_release(&self, at: &Observation) -> anyhow::Result<[u8; 32]> {
        self.read_synced_release(at).await
    }

    async fn claims(&self, at: &LifecycleEvidence, approvals: &[ApprovalId]) -> anyhow::Result<Vec<ApprovalStatus>> {
        self.read_claims(BlockId::hash_canonical(at.block_hash), approvals).await
    }

    /// Reads the vault's exit start at the latest block.
    /// Error cases:
    /// - an RPC call or decoding fails
    async fn exit_started(&self) -> anyhow::Result<bool> {
        let vault = Address::from(self.domain.vault.0);
        let input = exitStartedAtCall {}.abi_encode();
        let started = exitStartedAtCall::abi_decode_returns(&self.call(vault, input, BlockId::latest()).await?)?;
        Ok(started != 0)
    }

    /// Reads whether the vault's registry has rotation on, at the latest block; a vault with no
    /// registry set reads `false`.
    /// Error cases:
    /// - an RPC call or decoding fails
    async fn rotation_enabled(&self) -> anyhow::Result<bool> {
        let vault = Address::from(self.domain.vault.0);
        let at = BlockId::latest();
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), at).await?)?;
        if registry.is_zero() {
            return Ok(false);
        }
        Ok(rotationEnabledCall::abi_decode_returns(
            &self.call(registry, rotationEnabledCall {}.abi_encode(), at).await?,
        )?)
    }

    /// The handover source the vault's registry recorded when the election in effect was made.
    /// Error cases:
    /// - an RPC call or decoding fails, or the vault has no registry
    async fn handover_source(&self) -> anyhow::Result<HandoverSource> {
        let vault = Address::from(self.domain.vault.0);
        let at = BlockId::latest();
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), at).await?)?;
        ensure!(!registry.is_zero(), "the vault follows no operator registry");
        let source = handoverSourceCall::abi_decode_returns(
            &self.call(registry, handoverSourceCall {}.abi_encode(), at).await?,
        )?;
        Ok(HandoverSource {
            candidate: engine_core::types::AccountId(source.candidate.0.0),
            release: source.release.0,
            api_hostname: source.apiHostname,
            handover_endpoint: source.handoverEndpoint,
        })
    }

    /// The API hostname the candidate the registry names as the operator in effect registered.
    /// Error cases:
    /// - an RPC call or decoding fails, or the vault has no registry
    async fn operator_hostname(&self) -> anyhow::Result<String> {
        let vault = Address::from(self.domain.vault.0);
        let at = BlockId::latest();
        let registry = registryCall::abi_decode_returns(&self.call(vault, registryCall {}.abi_encode(), at).await?)?;
        ensure!(!registry.is_zero(), "the vault follows no operator registry");
        let in_effect = operatorInEffectCall::abi_decode_returns(
            &self.call(registry, operatorInEffectCall {}.abi_encode(), at).await?,
        )?;
        Ok(candidateOfCall::abi_decode_returns(
            &self.call(registry, candidateOfCall { candidate: in_effect.candidate }.abi_encode(), at).await?,
        )?
        .apiHostname)
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
