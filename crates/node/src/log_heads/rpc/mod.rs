//! The LogHeads chain access: posts through the checkpoint endpoint under the poster's own
//! relayer, reads through the custody RPC quorum. The posting endpoint is the operator's to run;
//! the reads are strict-majority, so the host running one endpoint cannot forge a head.

use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, B256, U64};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::http::reqwest::redirect::Policy;
use alloy::transports::http::reqwest::{Certificate, Client};
use anyhow::{Context, anyhow, ensure};
use tokio::time::timeout;
use tracing::warn;

use super::submission::{Reserve, SubmissionStore};
use super::{Head, HeadsChain, PostOutcome};
use crate::config::{CheckpointSection, LogHeadsSection, redact};
use crate::custody::VaultReader;
use crate::custody::{Quorum, VaultRpc};
use crate::log::CheckpointDomain;

// The LogHeads contract's ABI, as `contracts/src/LogHeads.sol` declares it, and the vault's signer
// history, as `contracts/src/Vault.sol` declares it.
sol! {
    function post(uint64 seq, bytes32 link, uint64 parentSeq, bytes32 parentLink, bytes signature) external;
    function heads(uint64 epoch) external view returns (uint64 seq, bytes32 link, uint64 signedAt, uint64 boundBlock, uint64 postedAt);
    function signerAtEpoch(uint64 epoch) external view returns (address signer);
}

/// The vault's current signer epoch and its newest posted head, read through the custody RPC
/// quorum at a strict-majority agreed latest block (`Quorum::posted_head`) — never at the custody
/// finality tag, whose lag would silently extend the rollback-detection window.
/// Error cases:
/// - identity admission or the quorum read fails
pub(crate) async fn posted<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    contract: Address,
    chain_id: u64,
) -> anyhow::Result<(u64, Option<Head>)> {
    let identity = quorum.identity().await?;
    ensure!(identity.chain_id == chain_id, "the custody quorum serves another chain");
    ensure!(identity.token.is_some(), "no vault answers on the custody quorum's chain");
    let (posted, _) = quorum.posted_head(contract).await?;
    Ok((posted.epoch, posted.head.map(|(seq, link)| Head { seq, link })))
}

/// What startup checks the log against, all read at one quorum-agreed block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartupHeads {
    /// The vault's current signer epoch.
    pub epoch: u64,
    /// The current epoch's posted head: the parent the poster's first post names.
    pub current: Option<Head>,
    /// The newest head the chain holds over this log for the starting key, with the epoch that
    /// posted it: the current epoch's or, while it holds none, the newest of the earlier epochs
    /// that key signed back to the first epoch another key signed.
    pub newest: Option<(u64, Head)>,
}

/// `posted`, and, while the current epoch holds no head and `signer` is its signer, the walk back
/// through the epochs before it while `signer` signed them, to the newest head one holds, all at the
/// agreed block: `setSigner` starts a new, empty epoch even when it reinstalls the same key, whose
/// earlier heads still bind this log. Each step is a vault signer change, so the walk is as long as
/// that key's unbroken run of epochs.
/// Error cases:
/// - identity admission or a quorum read fails, or an answer does not decode
pub(crate) async fn startup_heads<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    contract: Address,
    vault: Address,
    signer: Address,
    chain_id: u64,
) -> anyhow::Result<StartupHeads> {
    let identity = quorum.identity().await?;
    ensure!(identity.chain_id == chain_id, "the custody quorum serves another chain");
    ensure!(identity.token.is_some(), "no vault answers on the custody quorum's chain");
    let (posted, at) = quorum.posted_head(contract).await?;
    let signer_at = |epoch| async move {
        let out = quorum.call_as("signer_at_epoch", at, vault, signerAtEpochCall { epoch }.abi_encode().into()).await?;
        signerAtEpochCall::abi_decode_returns(&out).context("decoding the vault's signer at an epoch")
    };
    let current = posted.head.map(|(seq, link)| Head { seq, link });
    let (mut newest, mut epoch) = (current.map(|head| (posted.epoch, head)), posted.epoch);
    if newest.is_none() && signer_at(epoch).await? == signer {
        while newest.is_none() && epoch > 1 {
            epoch -= 1;
            if signer_at(epoch).await? != signer {
                break;
            }
            let out = quorum.call_as("earlier_log_head", at, contract, headsCall { epoch }.abi_encode().into()).await?;
            let held = headsCall::abi_decode_returns(&out).context("decoding an earlier epoch's log head")?;
            // Seq 0 is the unattested empty log, as in the current epoch's read.
            newest = (held.seq > 0).then_some((epoch, Head { seq: held.seq, link: held.link }));
        }
    }
    Ok(StartupHeads { epoch: posted.epoch, current, newest })
}

/// What a post's gas limit adds to its estimate; an estimate is exact only for the state it ran on.
const POST_GAS_MARGIN_DIVISOR: u64 = 4;

pub(crate) struct HeadsRpc {
    client: RpcClient,
    /// The endpoint URL and key in every form, kept only to scrub them from errors.
    redactions: Vec<String>,
    domain: CheckpointDomain,
    contract: Address,
    relayer: Address,
    timeout: Duration,
    /// The posting interval: twice it bounds the in-memory wait on a pending send before its
    /// stored hash is reconciled against the chain.
    interval: Duration,
    /// When this process last stored a pending send's hash: the monotonic deadline the wait is
    /// bounded by. `None` — a reservation persisted by a previous run, or planted by the host —
    /// reconciles at once; the persisted `stored_at` is a hint, never the bound.
    pending_since: std::sync::Mutex<Option<std::time::Instant>>,
    submissions: SubmissionStore,
    /// One permit: held by the reservation operation on the blocking pool, until it completes even
    /// after its caller's bound elapses, so a stalled file holds one worker, never one per attempt.
    store_slot: Arc<tokio::sync::Semaphore>,
    quorum: Arc<Quorum<VaultRpc>>,
}

impl HeadsRpc {
    /// Builds clients without accessing the network; existing nonce storage is parsed and checked.
    /// Error cases:
    /// - the URL, certificates, or client cannot be built, or an existing reservation is invalid
    pub(crate) fn new(
        checkpoints: &CheckpointSection,
        cfg: &LogHeadsSection,
        domain: CheckpointDomain,
        log_dir: &std::path::Path,
        quorum: Arc<Quorum<VaultRpc>>,
    ) -> anyhow::Result<Self> {
        let url = checkpoints.rpc_url.expose()?;
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()?;
        let http = Client::builder()
            .user_agent(concat!("engine-log-heads/", env!("CARGO_PKG_VERSION")))
            .tls_certs_only(roots)
            // A redirect would leave the origin the attested config fixes, carrying the key with it.
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            client: RpcClient::new_http_with_client(http, url),
            redactions: checkpoints.rpc_url.redactions()?,
            domain,
            contract: cfg.contract,
            relayer: cfg.relayer,
            timeout: checkpoints.rpc_timeout,
            interval: cfg.interval,
            pending_since: std::sync::Mutex::new(None),
            submissions: SubmissionStore::pending(log_dir, cfg.relayer, cfg.contract, domain.chain_id)?,
            store_slot: Arc::new(tokio::sync::Semaphore::new(1)),
            quorum,
        })
    }

    /// `startup_heads` for this deployment's contract, vault, and chain.
    /// Error cases:
    /// - as `startup_heads`
    pub(crate) async fn startup_heads(&self, signer: Address) -> anyhow::Result<StartupHeads> {
        let vault = Address::from(self.domain.vault.0);
        startup_heads(&self.quorum, self.contract, vault, signer, self.domain.chain_id).await
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
                Err(anyhow::anyhow!("log-head RPC failed: {}", redact(&error.to_string(), &self.redactions)))
            }
            Err(_) => Err(anyhow::anyhow!("log-head RPC timed out")),
        }
    }

    /// The relayer's transaction count at `tag`.
    /// Error cases:
    /// - the RPC fails, times out, or returns a malformed quantity
    async fn relayer_nonce(&self, tag: BlockNumberOrTag) -> anyhow::Result<u64> {
        let nonce: U64 = self.deadline(self.client.request("eth_getTransactionCount", (self.relayer, tag))).await?;
        Ok(nonce.to())
    }

    /// Whether the relayer's transaction is known to the endpoint: a stored hash is a hint, not
    /// proof — a send the endpoint dropped, or one the host fabricated in the unauthenticated
    /// reservation file, lookup-misses and is replaced on the next send.
    /// Error cases:
    /// - the RPC fails or times out
    async fn transaction_known(&self, hash: B256) -> anyhow::Result<bool> {
        let found: Option<serde_json::Value> =
            self.deadline(self.client.request("eth_getTransactionByHash", (hash,))).await?;
        Ok(found.is_some())
    }

    /// Adds the gas margin to the endpoint's estimate.
    /// Error cases:
    /// - the estimate fails or times out
    async fn with_gas(&self, tx: TransactionRequest) -> anyhow::Result<TransactionRequest> {
        let estimate: U64 = self.deadline(self.client.request("eth_estimateGas", (tx.clone(),))).await?;
        let estimate: u64 = estimate.to();
        Ok(TransactionRequest { gas: Some(estimate.saturating_add(estimate / POST_GAS_MARGIN_DIVISOR)), ..tx })
    }

    /// The exact signed call at `nonce`. The endpoint fills the gas price and the poster
    /// re-estimates the gas limit on each send, so relaying it again is the same call at the same
    /// nonce: the identical transaction when neither moved, else a same-nonce replacement.
    fn post_request(
        &self,
        head: Head,
        parent: Option<Head>,
        signature: &str,
        nonce: u64,
    ) -> anyhow::Result<TransactionRequest> {
        signed_call(self.contract, self.relayer, self.domain.chain_id, head, parent, signature, nonce)
    }

    /// The pending send's own call, rebuilt from the reservation: rebroadcasting it repeats that
    /// call at its nonce, never a newer head. Replacing a live transaction with a newer head at the same nonce — a fee-bumped
    /// replacement — is out of scope for this change: a known pending transaction is rebroadcast
    /// until it mines or the endpoint drops it (the dropped case reconciles at once, see
    /// `pending_due`).
    fn rebroadcast_request(&self, pending: &super::submission::Reservation) -> anyhow::Result<TransactionRequest> {
        rebroadcast_call(self.contract, self.relayer, self.domain.chain_id, pending)
    }

    /// Whether a pending send may keep waiting or must reconcile now, by the in-memory deadline.
    fn pending_action(&self) -> bool {
        let since = self.pending_since.lock().expect("pending-since lock poisoned");
        pending_due(*since, self.interval.saturating_mul(2))
    }

    /// Persists the reservation on the blocking-pool worker, bounded by `timeout`. The store's
    /// lock is taken inside the worker and held until the write completes: a caller that times
    /// out abandons the wait, never the lock, so a stalled writer keeps exclusive access, and a
    /// retry while it is outstanding is refused rather than queued behind it.
    /// Error cases:
    /// - an earlier operation is still outstanding, the bound elapses, or the write fails
    async fn persist_update(&self, reservation: super::submission::Reservation) -> anyhow::Result<()> {
        let store = self.submissions.clone();
        bounded_store(&self.store_slot, self.timeout, move || store.update(&reservation)).await
    }
}

/// Whether a pending send's wait has run out: a send this process made waits out the bound from
/// when it was stored; a reservation this process has not sent (persisted by an earlier run, or
/// planted by the host) reconciles immediately. The monotonic in-memory clock, never the
/// host-written `stored_at`, is the bound.
fn pending_due(since: Option<std::time::Instant>, bound: Duration) -> bool {
    since.is_none_or(|sent_at| sent_at.elapsed() >= bound)
}

/// Runs one reservation-store operation on the blocking-pool worker, bounded by `timeout`: the
/// poster loop — and the stall alarm running in it — never waits on a host-held filesystem
/// operation beyond the bound. A timed-out operation still completes in the background, holding
/// `slot`'s one permit until it does; the store's validate-and-replace makes the late write
/// idempotent, and the next attempt reconciles against what it re-reads.
/// Error cases:
/// - an earlier operation is still outstanding, the bound elapses, or the operation fails
async fn bounded_store<T: Send + 'static>(
    slot: &Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    op: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let permit = slot
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow!("an earlier reservation IO operation is still outstanding"))?;
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        op()
    });
    match tokio::time::timeout(timeout, work).await {
        Ok(done) => done.context("the reservation IO worker panicked")?,
        Err(_) => Err(anyhow!("reservation IO did not complete within {timeout:?}")),
    }
}

/// The pending send's own call: the reserved head's bytes at the reserved nonce, whatever newer
/// head this attempt was made for.
fn rebroadcast_call(
    contract: Address,
    relayer: Address,
    chain_id: u64,
    pending: &super::submission::Reservation,
) -> anyhow::Result<TransactionRequest> {
    let (_, head) = pending.head.context("a pending reservation carries its head")?;
    let signature = pending.signature.as_deref().context("a pending reservation carries its call")?;
    let parent = (pending.parent_seq != 0).then_some(Head { seq: pending.parent_seq, link: pending.parent_link });
    signed_call(contract, relayer, chain_id, head, parent, signature, pending.nonce)
}

/// The exact signed `post` call at `nonce`.
fn signed_call(
    contract: Address,
    relayer: Address,
    chain_id: u64,
    head: Head,
    parent: Option<Head>,
    signature: &str,
    nonce: u64,
) -> anyhow::Result<TransactionRequest> {
    let (parent_seq, parent_link) = parent.map(|parent| (parent.seq, parent.link)).unwrap_or((0, B256::ZERO));
    let tx = TransactionRequest::default().to(contract).from(relayer).nonce(nonce).input(
        postCall {
            seq: head.seq,
            link: head.link,
            parentSeq: parent_seq,
            parentLink: parent_link,
            signature: signature.parse()?,
        }
        .abi_encode()
        .into(),
    );
    Ok(TransactionRequest { chain_id: Some(chain_id), ..tx })
}

impl HeadsChain for HeadsRpc {
    /// The signer epoch and newest posted head, agreed by the custody quorum at one block.
    /// Error cases:
    /// - identity admission or the quorum read fails
    async fn posted(&self) -> anyhow::Result<(u64, Option<Head>)> {
        posted(&self.quorum, self.contract, self.domain.chain_id).await
    }

    /// Reserves the relayer nonce durably, then relays the exact signed call. A send pending at the
    /// outstanding nonce waits out an in-memory bound — twice the posting interval from when this
    /// process stored it, never from the host-written file — then its stored hash is reconciled
    /// against the chain: unknown (dropped, or planted by the host) it is cleared so this or a
    /// newer head re-sends at the nonce; present it is rebroadcast — the reserved head's own call
    /// at the same nonce.
    /// Error cases:
    /// - nonce reservation, the transaction lookup, gas estimation, the submission, or a bounded
    ///   reservation-store operation fails or outruns its bound
    async fn post(&self, epoch: u64, head: Head, parent: Option<Head>, signature: &str) -> anyhow::Result<PostOutcome> {
        let latest = self.relayer_nonce(BlockNumberOrTag::Latest).await?;
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs());
        let store = self.submissions.clone();
        let reserve = move || store.reserve(latest, epoch, head, now);
        let mut reservation = match bounded_store(&self.store_slot, self.timeout, reserve).await? {
            Reserve::Pending(_) if !self.pending_action() => return Ok(PostOutcome::Waiting),
            Reserve::Pending(mut pending) => {
                let hash = pending.tx_hash.context("a pending reservation carries its hash")?;
                if self.transaction_known(hash).await? {
                    let tx = self.rebroadcast_request(&pending)?;
                    let tx = self.with_gas(tx).await?;
                    let _: B256 = self.deadline(self.client.request("eth_sendTransaction", (tx,))).await?;
                    *self.pending_since.lock().expect("pending-since lock poisoned") = Some(std::time::Instant::now());
                    pending.stored_at = now;
                    self.persist_update(pending).await?;
                    return Ok(PostOutcome::Waiting);
                }
                warn!(nonce = pending.nonce, %hash,
                    "the stored log-head send is unknown to the endpoint: clearing it to re-send at the nonce");
                // The replacement is the call now being sent: rewrite the record to it, so a
                // later reconciliation rebroadcasts exactly these bytes — never a newer
                // signature against the older head.
                pending.tx_hash = None;
                pending.signature = None;
                pending.record_send(epoch, head, parent);
                pending
            }
            Reserve::Nonce(mut reservation) => {
                reservation.record_send(epoch, head, parent);
                reservation
            }
        };
        // The complete call — epoch, head, parent, and signature — is durable before the
        // broadcast: the record never names a nonce in flight without the call that may land at
        // it, whatever a crash in the broadcast window leaves unanswered.
        reservation.signature = Some(signature.to_string());
        reservation.stored_at = now;
        self.persist_update(reservation.clone()).await?;
        let tx = self.post_request(head, parent, signature, reservation.nonce)?;
        let tx = self.with_gas(tx).await?;
        let hash: B256 = self.deadline(self.client.request("eth_sendTransaction", (tx,))).await?;
        reservation.tx_hash = Some(hash);
        *self.pending_since.lock().expect("pending-since lock poisoned") = Some(std::time::Instant::now());
        self.persist_update(reservation).await?;
        Ok(PostOutcome::Accepted)
    }
}

#[cfg(test)]
mod tests;
