//! Ethereum through the relayer's own providers and its signing endpoint. Every read comes from a
//! network the host controls, so each one is a strict majority of the configured providers giving
//! the same answer, never one provider's. What the reads decide is bounded on chain as well:
//! `SeedRelay` checks the beacon root, the header and both branches itself, so a wrong answer can
//! only cost a relay that reverts. The signing endpoint, which holds the relayer key, is trusted
//! only to sign and send: a fee or a receipt it misstates makes the Inbox refuse the ticket, refund
//! it on Arbitrum, or the relay go out again after `resend_after`.

use std::future::{Future, IntoFuture};
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, B256, Bytes, U64, U256, address};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::{SolCall, SolInterface};
use alloy::transports::http::reqwest::redirect::Policy;
use alloy::transports::http::reqwest::{Certificate, Client};
use alloy::transports::{RpcError, TransportErrorKind};
use anyhow::{Context, anyhow, bail, ensure};
use futures_util::future::join_all;
use serde::Deserialize;
use tokio::time::timeout;

use crate::config::{SecretUrl, SeedRelaySection, redact};
use crate::seed_relay::abi::{IInbox, ISeedRelay};

/// The EIP-4788 beacon block root contract.
pub(crate) const BEACON_ROOTS: Address = address!("000F3df6D732807Ef1319fB7B8bB8522d0Beac02");
/// What a relay's gas limit adds to its estimate.
const GAS_MARGIN_DIVISOR: u64 = 4;

/// One endpoint, every request under a deadline and with its URL scrubbed from errors.
pub(crate) struct Endpoint {
    client: RpcClient,
    redactions: Vec<String>,
    timeout: Duration,
}

/// The one field of a block the relayer reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FeeBlock {
    base_fee_per_gas: Option<U256>,
}

/// The relay contract's deployment, as the quorum reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RelayIdentity {
    pub receiver: Address,
    pub inbox: Address,
    pub genesis: u64,
}

/// Ethereum for the relayer.
pub(crate) struct Ethereum {
    providers: Vec<(String, Endpoint)>,
    signer: Endpoint,
    chain_id: u64,
    relayer: Address,
    relay: Address,
}

impl Endpoint {
    /// A client for `url`, which trusts only the compiled-in TLS roots and follows no redirect.
    /// Error cases:
    /// - the URL is undelivered or does not parse, or the client does not build
    fn new(url: &SecretUrl, timeout: Duration) -> anyhow::Result<Self> {
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()?;
        let http = Client::builder()
            .user_agent(concat!("engine-seed-relay/", env!("CARGO_PKG_VERSION")))
            .tls_certs_only(roots)
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            client: RpcClient::new_http_with_client(http, url.expose()?),
            redactions: url.redactions()?,
            timeout,
        })
    }

    /// Waits for one JSON-RPC request, `method`'s, under the deadline.
    /// Error cases:
    /// - the transport fails, the endpoint answers an error, or the deadline passes
    async fn wait<T>(
        &self,
        method: &'static str,
        request: impl IntoFuture<Output = Result<T, RpcError<TransportErrorKind>>>,
    ) -> anyhow::Result<T> {
        match timeout(self.timeout, request.into_future()).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(self.explain(method, &error)),
            Err(_) => Err(anyhow!("{method} timed out")),
        }
    }

    /// `to`'s answer to `data` at the latest block, or `None` when the call reverts.
    /// Error cases:
    /// - the transport fails or the deadline passes
    async fn call(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<Bytes>> {
        let request = TransactionRequest::default().to(to).input(data.into());
        let call = self.client.request::<_, Bytes>("eth_call", (request, BlockNumberOrTag::Latest));
        match timeout(self.timeout, call.into_future()).await {
            Ok(Ok(value)) => Ok(Some(value)),
            // The beacon root contract reverts, with no data, for a time it holds no entry for; any other
            // error answer is the endpoint's own failure.
            Ok(Err(RpcError::ErrorResp(payload))) if payload.message.contains("revert") => Ok(None),
            Ok(Err(error)) => Err(self.explain("eth_call", &error)),
            Err(_) => Err(anyhow!("eth_call timed out")),
        }
    }

    /// An endpoint's error without its URL, and a relay's revert by name.
    fn explain(&self, method: &str, error: &RpcError<TransportErrorKind>) -> anyhow::Error {
        let reason = error
            .as_error_resp()
            .and_then(|payload| payload.as_revert_data())
            .and_then(|data| ISeedRelay::ISeedRelayErrors::abi_decode(&data).ok())
            .map(|refusal| format!(" ({refusal:?})"))
            .unwrap_or_default();
        anyhow!("{method} failed: {}{reason}", redact(&error.to_string(), &self.redactions))
    }
}

impl Ethereum {
    /// Builds the clients without a request.
    /// Error cases:
    /// - a URL is undelivered or does not parse, or a client does not build
    pub(crate) fn new(cfg: &SeedRelaySection) -> anyhow::Result<Self> {
        let providers = cfg
            .providers
            .iter()
            .map(|p| Ok((p.name.clone(), Endpoint::new(&p.rpc_url, cfg.rpc_timeout)?)))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            providers,
            signer: Endpoint::new(&cfg.rpc_url, cfg.rpc_timeout)?,
            chain_id: cfg.chain_id,
            relayer: cfg.relayer,
            relay: cfg.relay,
        })
    }

    /// The relay's receiver, inbox and genesis time, on the configured chain.
    /// Error cases:
    /// - the providers do not agree, serve another chain, or find no relay at the address
    pub(crate) async fn identity(&self) -> anyhow::Result<RelayIdentity> {
        let chain_id = self
            .agree("chain id", |rpc| async move {
                Ok(rpc.wait("eth_chainId", rpc.client.request::<_, U64>("eth_chainId", ())).await?.to::<u64>())
            })
            .await?;
        ensure!(chain_id == self.chain_id, "the providers serve chain {chain_id}, not {}", self.chain_id);
        let receiver = self.view("relay receiver", ISeedRelay::receiverCall {}).await?;
        let inbox = self.view("relay inbox", ISeedRelay::inboxCall {}).await?;
        let genesis = self.view("relay genesis", ISeedRelay::beaconGenesisTimeCall {}).await?;
        Ok(RelayIdentity { receiver, inbox, genesis })
    }

    /// The beacon root the EIP-4788 contract holds for the block at `timestamp`, or `None`.
    /// Error cases:
    /// - the providers do not agree
    pub(crate) async fn beacon_root(&self, timestamp: u64) -> anyhow::Result<Option<B256>> {
        let data = U256::from(timestamp).to_be_bytes::<32>().to_vec();
        self.agree("beacon root", |rpc| {
            let data = data.clone();
            async move {
                Ok(match rpc.call(BEACON_ROOTS, data).await? {
                    Some(root) if root.len() == 32 => Some(B256::from_slice(&root)),
                    Some(other) => bail!("the beacon root contract answered {} bytes", other.len()),
                    None => None,
                })
            }
        })
        .await
    }

    /// Whether the relay accepts states of fork `version`.
    /// Error cases:
    /// - the providers do not agree
    pub(crate) async fn accepts_fork(&self, version: [u8; 4]) -> anyhow::Result<bool> {
        self.view("accepted fork", ISeedRelay::acceptedForkCall { version: version.into() }).await
    }

    /// The Inbox's submission fee for a `length`-byte message at `base_fee` wei per gas.
    /// Error cases:
    /// - the providers do not agree
    pub(crate) async fn submission_fee(&self, inbox: Address, length: usize, base_fee: U256) -> anyhow::Result<U256> {
        let call = IInbox::calculateRetryableSubmissionFeeCall { dataLength: U256::from(length), baseFee: base_fee };
        self.view_at("submission fee", inbox, call).await
    }

    /// The relayer's balance at the providers' finalized block.
    /// Error cases:
    /// - the providers do not agree
    pub(crate) async fn balance(&self) -> anyhow::Result<U256> {
        let relayer = self.relayer;
        self.agree("relayer balance", |rpc| async move {
            rpc.wait(
                "eth_getBalance",
                rpc.client.request::<_, U256>("eth_getBalance", (relayer, BlockNumberOrTag::Finalized)),
            )
            .await
        })
        .await
    }

    /// The latest block's base fee, as the signing endpoint sees it.
    /// Error cases:
    /// - the request fails, or the block has no base fee
    pub(crate) async fn base_fee(&self) -> anyhow::Result<U256> {
        let block: Option<FeeBlock> = self
            .signer
            .wait(
                "eth_getBlockByNumber",
                self.signer.client.request("eth_getBlockByNumber", (BlockNumberOrTag::Latest, false)),
            )
            .await?;
        let block = block.context("the signing endpoint has no latest block")?;
        block.base_fee_per_gas.context("the latest block has no base fee")
    }

    /// Sends `input` to the relay with `value`, from the relayer, through the signing endpoint, at
    /// `nonce`, at twice the latest base fee plus the endpoint's priority fee. In place of `replacing`
    /// while the endpoint still finds that transaction, it outbids its fees by a quarter.
    /// Error cases:
    /// - a fee or the pending transaction cannot be read, the gas estimate reverts, naming the relay's
    ///   reason, or the send fails
    pub(crate) async fn send(
        &self,
        input: Vec<u8>,
        value: U256,
        nonce: u64,
        replacing: Option<B256>,
    ) -> anyhow::Result<B256> {
        let tip: U256 = self
            .signer
            .wait("eth_maxPriorityFeePerGas", self.signer.client.request("eth_maxPriorityFeePerGas", ()))
            .await?;
        let tip: u128 = tip.saturating_to();
        let mut fees = (self.base_fee().await?.saturating_to::<u128>().saturating_mul(2).saturating_add(tip), tip);
        if let Some(hash) = replacing {
            let pending: Option<TransactionRequest> = self
                .signer
                .wait("eth_getTransactionByHash", self.signer.client.request("eth_getTransactionByHash", (hash,)))
                .await?;
            if let Some(pending) = pending {
                fees.0 = fees.0.max(outbid(pending.max_fee_per_gas));
                fees.1 = fees.1.max(outbid(pending.max_priority_fee_per_gas));
            }
        }
        let tx = TransactionRequest::default()
            .from(self.relayer)
            .to(self.relay)
            .value(value)
            .input(input.into())
            .max_fee_per_gas(fees.0)
            .max_priority_fee_per_gas(fees.1);
        let tx = TransactionRequest { chain_id: Some(self.chain_id), nonce: Some(nonce), ..tx };
        let estimate: U64 =
            self.signer.wait("eth_estimateGas", self.signer.client.request("eth_estimateGas", (tx.clone(),))).await?;
        let estimate: u64 = estimate.to();
        let tx = TransactionRequest { gas: Some(estimate.saturating_add(estimate / GAS_MARGIN_DIVISOR)), ..tx };
        self.signer.wait("eth_sendTransaction", self.signer.client.request("eth_sendTransaction", (tx,))).await
    }

    /// The relayer's nonce at the latest block, and counting its pending transactions, at the signing
    /// endpoint.
    /// Error cases:
    /// - a request fails
    pub(crate) async fn nonces(&self) -> anyhow::Result<(u64, u64)> {
        let count = |tag: BlockNumberOrTag| {
            self.signer.wait(
                "eth_getTransactionCount",
                self.signer.client.request::<_, U64>("eth_getTransactionCount", (self.relayer, tag)),
            )
        };
        Ok((count(BlockNumberOrTag::Latest).await?.to(), count(BlockNumberOrTag::Pending).await?.to()))
    }

    /// Whether the transaction succeeded, once it is in a block.
    /// Error cases:
    /// - the request fails
    pub(crate) async fn receipt(&self, hash: B256) -> anyhow::Result<Option<bool>> {
        let receipt: Option<TransactionReceipt> = self
            .signer
            .wait("eth_getTransactionReceipt", self.signer.client.request("eth_getTransactionReceipt", (hash,)))
            .await?;
        Ok(receipt.map(|receipt| receipt.status()))
    }

    /// A view of the relay that a strict majority answers the same.
    /// Error cases:
    /// - the providers do not agree, or the answer does not decode
    async fn view<C: SolCall + Send + Sync>(&self, what: &'static str, call: C) -> anyhow::Result<C::Return>
    where
        C::Return: PartialEq + Clone,
    {
        self.view_at(what, self.relay, call).await
    }

    /// A view of `to` that a strict majority answers the same.
    /// Error cases:
    /// - the providers do not agree, a call reverts, or the answer does not decode
    async fn view_at<C: SolCall + Send + Sync>(
        &self,
        what: &'static str,
        to: Address,
        call: C,
    ) -> anyhow::Result<C::Return>
    where
        C::Return: PartialEq + Clone,
    {
        let data = call.abi_encode();
        self.agree(what, |rpc| {
            let data = data.clone();
            async move {
                let out = rpc.call(to, data).await?.with_context(|| format!("{to} reverted"))?;
                C::abi_decode_returns(&out).with_context(|| format!("decoding {what}"))
            }
        })
        .await
    }

    /// The answer a strict majority of the configured providers gives alike.
    /// Error cases:
    /// - no answer has a strict majority; the error names each provider's answer or failure
    async fn agree<'a, T, F, Fut>(&'a self, what: &str, read: F) -> anyhow::Result<T>
    where
        T: PartialEq + Clone,
        F: Fn(&'a Endpoint) -> Fut,
        Fut: Future<Output = anyhow::Result<T>> + 'a,
    {
        let answers = join_all(self.providers.iter().map(|(_, rpc)| read(rpc))).await;
        let required = self.providers.len() / 2 + 1;
        let winner = answers
            .iter()
            .flatten()
            .find(|value| answers.iter().flatten().filter(|other| other == value).count() >= required);
        if let Some(value) = winner {
            return Ok(value.clone());
        }
        let failures: Vec<String> = self
            .providers
            .iter()
            .zip(&answers)
            .map(|((name, _), answer)| match answer {
                Ok(_) => format!("{name}: answered"),
                Err(error) => format!("{name}: {error:#}"),
            })
            .collect();
        bail!(
            "no {required} of {} Ethereum providers agree on the {what}: {}",
            self.providers.len(),
            failures.join("; ")
        )
    }
}

/// A fee a quarter above `fee`, which a replacement must pay to displace the transaction paying it.
fn outbid(fee: Option<u128>) -> u128 {
    fee.map_or(0, |fee| fee.saturating_add(fee / 4).saturating_add(1))
}

#[cfg(test)]
mod tests;
