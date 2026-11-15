//! A member's liveness caller (#595): from a funded account of its own, apart from the log-head and
//! seed relayers, each `POLL` while a turn names the operator it
//! - witnesses the current slot once no witness has marked it (`OperatorSchedule.witness`);
//! - promotes once the block time reaches `promotionDueAt` (`OperatorSchedule.promote`), naming the
//!   next member in the turn's order;
//! - relays the operator's newest signed head from its stream when the chain's head is older than two
//!   thirds of an interval, so a head the operator's own relayer fails to post still counts for the
//!   enclave that signed it.
//!
//! It judges time by the later of the latest block's and its own clock's: a chain with no
//! transactions makes no blocks, so its latest block time can stand still while slots pass, and the
//! caller's own witness is what moves it. The chain checks every call at its own block time either way.
//! Its own clock is trusted time's lower bound (`Reached`), never the host's: a witness of a slot the
//! chain has marked does nothing yet does not revert, so a host clock ahead of the chain would pay for
//! one at every poll.
//!
//! Each call is estimated first, so one the chain would refuse (a promotion not yet due, a head no
//! longer fresh) costs nothing. A balance below `[liveness] min_balance` raises
//! `liveness_caller_unfunded`, as does a send the endpoint refuses for funds.

use std::future::IntoFuture;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::utils::format_ether;
use alloy::primitives::{Address, B256, U64, U256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use alloy::transports::http::reqwest::redirect::Policy;
use alloy::transports::http::reqwest::{Certificate, Client};
use anyhow::Context as _;
use futures_util::future::BoxFuture;
use tokio::time::{MissedTickBehavior, interval, timeout};
use tracing::{debug, info, warn};

use crate::config::{CheckpointSection, LivenessSection, redact};
use crate::liveness::chain::{LivenessDomain, View, promoteCall, view, witnessCall};
use crate::liveness::link::Copy;
use crate::log_heads::{Head, SignedHead};
use crate::standby_key::chain::StandbyChain;

/// How often the caller reads the chain.
pub(crate) const POLL: Duration = Duration::from_secs(5);
/// What a call's gas limit adds to its estimate.
const GAS_MARGIN_DIVISOR: u64 = 4;

/// What the caller decides to send after a read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Witness,
    Promote,
    /// A signed head, relayed.
    Relay(Box<SignedHead>),
}

/// How the caller sends: estimated first, then sent from its own account.
pub(crate) trait Sending: Send + Sync {
    /// Sends `data` to `to` once its estimate succeeds, returning the transaction hash, or `None` when
    /// the estimate shows the chain would refuse it.
    /// Error cases:
    /// - the send fails or outruns its bound
    fn send(&self, to: Address, data: Vec<u8>) -> impl Future<Output = anyhow::Result<Option<B256>>> + Send;

    /// The caller's balance, in wei.
    /// Error cases:
    /// - the read fails or outruns its bound
    fn balance(&self) -> impl Future<Output = anyhow::Result<U256>> + Send;
}

/// The caller's endpoint: the checkpoint endpoint, which signs for the caller's account.
pub(crate) struct Endpoint {
    client: RpcClient,
    redactions: Vec<String>,
    caller: Address,
    chain_id: u64,
    timeout: Duration,
}

/// A member's liveness caller.
pub(crate) struct Caller<C, S> {
    pub chain: C,
    pub sending: S,
    pub domain: LivenessDomain,
    /// What the member's following saw of the operator's signed heads.
    pub copy: Copy,
    /// Below this many wei, `liveness_caller_unfunded` fires.
    pub min_balance: U256,
    /// How often it reads the chain: `POLL`.
    pub poll: Duration,
    /// The caller's own clock, which it judges slots by when it is ahead of the latest block: `Reached`
    /// over the member's trusted time.
    pub clock: std::sync::Arc<dyn crate::clock::Clock>,
}

impl Endpoint {
    /// Builds the client without accessing the network, trusting the binary's built-in TLS roots.
    /// Error cases:
    /// - the URL, the roots or the client cannot be built
    pub(crate) fn new(checkpoints: &CheckpointSection, cfg: &LivenessSection, chain_id: u64) -> anyhow::Result<Self> {
        let url = checkpoints.rpc_url.expose()?;
        // The enclave image holds no system certificate store: the roots are the binary's own.
        let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .map(|cert| Certificate::from_der(cert))
            .collect::<Result<Vec<_>, _>>()
            .context("loading the built-in TLS roots")?;
        let http = Client::builder()
            .user_agent(concat!("engine-liveness/", env!("CARGO_PKG_VERSION")))
            .tls_certs_only(roots)
            // A redirect would leave the origin the attested config fixes, carrying the key with it.
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            client: RpcClient::new_http_with_client(http, url),
            redactions: checkpoints.rpc_url.redactions()?,
            caller: cfg.caller,
            chain_id,
            timeout: checkpoints.rpc_timeout,
        })
    }

    /// Bounds a request and redacts the endpoint's credentials from its error.
    /// Error cases:
    /// - transport failure or timeout
    async fn deadline<T, E>(&self, future: impl IntoFuture<Output = Result<T, E>>) -> anyhow::Result<T>
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        match timeout(self.timeout, future.into_future()).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                Err(anyhow::anyhow!("liveness RPC failed: {}", redact(&error.to_string(), &self.redactions)))
            }
            Err(_) => Err(anyhow::anyhow!("liveness RPC timed out")),
        }
    }
}

impl Sending for Endpoint {
    async fn send(&self, to: Address, data: Vec<u8>) -> anyhow::Result<Option<B256>> {
        let tx = TransactionRequest::default().to(to).from(self.caller).input(data.into());
        let tx = TransactionRequest { chain_id: Some(self.chain_id), ..tx };
        let estimate: U64 = match self.deadline(self.client.request("eth_estimateGas", (tx.clone(),))).await {
            Ok(estimate) => estimate,
            // Only a revert is the chain refusing the call; a transport or endpoint failure is not.
            Err(e) if format!("{e:#}").to_ascii_lowercase().contains("revert") => {
                debug!("the call would be refused: {e:#}");
                return Ok(None);
            }
            Err(e) => return Err(e.context("estimating the liveness call's gas")),
        };
        let estimate: u64 = estimate.to();
        let tx = TransactionRequest { gas: Some(estimate.saturating_add(estimate / GAS_MARGIN_DIVISOR)), ..tx };
        self.deadline(self.client.request("eth_sendTransaction", (tx,))).await.map(Some)
    }

    async fn balance(&self) -> anyhow::Result<U256> {
        self.deadline(self.client.request("eth_getBalance", (self.caller, BlockNumberOrTag::Latest))).await
    }
}

impl<C: StandbyChain, S: Sending> Caller<C, S> {
    /// Runs the caller until its task is dropped.
    /// Error cases:
    /// - none: failed reads and sends are retried on the next poll
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        let mut poll = interval(self.poll);
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let (mut read_failed, mut unfunded, mut failing) = (false, false, false);
        loop {
            poll.tick().await;
            let read: BoxFuture<'_, anyhow::Result<View>> =
                Box::pin(async { view(&self.chain.latest().await?, &self.domain).await });
            let view = match read.await {
                Ok(view) => {
                    if std::mem::take(&mut read_failed) {
                        info!(
                            alert_resolved = "liveness_caller_read_failed",
                            "the liveness caller reads the chain again"
                        );
                    }
                    view
                }
                Err(e) => {
                    if !std::mem::replace(&mut read_failed, true) {
                        warn!(
                            alert = "liveness_caller_read_failed",
                            "the liveness caller cannot read the chain: {e:#}"
                        );
                    }
                    continue;
                }
            };
            if let Some(head) = self.copy.signed().filter(|signed| signed.epoch == view.epoch) {
                self.copy.saw(head.epoch, head.head.head);
            }
            // The head on chain is checked too: a copy holding another link at its seq confirms
            // nothing under the epoch, whichever history the stream carries.
            if view.head.link != B256::ZERO {
                self.copy.saw(view.epoch, Head { seq: view.head.seq, link: view.head.link });
            }
            let now = u64::try_from(self.clock.now_ms() / 1000).unwrap_or(0);
            let signed = self.copy.signed();
            let decided = calls(&view, signed.clone(), now);
            debug!(
                block_time = view.time,
                now,
                epoch = view.epoch,
                marked_slot = view.marked_slot,
                due_at = view.due_at,
                head_seq = view.head.seq,
                posted_at = view.head.posted_at,
                signed = ?signed.as_ref().map(|signed| (signed.epoch, signed.head.head.seq, signed.head.signed_at)),
                ?decided,
                "the liveness caller read the chain"
            );
            for call in decided {
                self.send(&call, (&mut unfunded, &mut failing)).await;
            }
            self.check_balance(&mut unfunded, &mut failing).await;
        }
    }

    /// Sends `call`, logging what happened: a send that fails for funds pages `liveness_caller_unfunded`,
    /// and any other failure `liveness_caller_send_failed`, once per episode, which a call the endpoint
    /// estimates resolves.
    async fn send(&self, call: &Call, (unfunded, failing): (&mut bool, &mut bool)) {
        let (to, data) = match call {
            Call::Witness => (self.domain.standby.schedule, witnessCall {}.abi_encode()),
            Call::Promote => (self.domain.standby.schedule, promoteCall {}.abi_encode()),
            Call::Relay(signed) => match signed.head.calldata(&signed.signature) {
                Ok(data) => (self.domain.log_heads, data),
                Err(e) => {
                    warn!("a signed head from the stream does not encode: {e:#}");
                    return;
                }
            },
        };
        let sent = self.sending.send(to, data).await;
        if sent.is_ok() && std::mem::take(failing) {
            info!(alert_resolved = "liveness_caller_send_failed", "the liveness caller's endpoint answers again");
        }
        match sent {
            Ok(Some(hash)) => info!(?call, %hash, "liveness call sent"),
            Ok(None) => debug!(?call, "the chain would refuse the liveness call: not sent"),
            Err(e) => {
                let message = format!("{e:#}");
                if message.contains("insufficient funds") {
                    if !std::mem::replace(unfunded, true) {
                        warn!(
                            alert = "liveness_caller_unfunded",
                            "the liveness caller cannot pay for its calls: {message}"
                        );
                    }
                } else if !std::mem::replace(failing, true) {
                    warn!(
                        alert = "liveness_caller_send_failed",
                        "the liveness caller cannot estimate or send its calls ({call:?}): {message}"
                    );
                }
            }
        }
    }

    /// Warns while the caller's balance is below the minimum, once per episode; an unreadable balance is
    /// part of the `liveness_caller_send_failed` episode.
    async fn check_balance(&self, unfunded: &mut bool, failing: &mut bool) {
        match self.sending.balance().await {
            Ok(balance) if balance < self.min_balance => {
                if !std::mem::replace(unfunded, true) {
                    warn!(
                        alert = "liveness_caller_unfunded",
                        "the liveness caller holds {} ETH, below {}: witnesses, promotions and relays stop when it \
                         cannot pay for gas",
                        format_ether(balance),
                        format_ether(self.min_balance)
                    );
                }
            }
            Ok(balance) => {
                if std::mem::take(unfunded) {
                    info!(
                        alert_resolved = "liveness_caller_unfunded",
                        "the liveness caller holds {} ETH",
                        format_ether(balance)
                    );
                }
            }
            Err(e) => {
                if !std::mem::replace(failing, true) {
                    warn!(alert = "liveness_caller_send_failed", "the liveness caller's balance is unreadable: {e:#}");
                }
            }
        }
    }
}

/// Trusted time's lower bound, a time true time has certainly reached, as a clock's wall time: zero
/// before the first sample, so the caller judges by the block alone until one comes.
pub(crate) struct Reached(pub crate::trusted_time::TrustedClock);

impl crate::clock::Clock for Reached {
    fn now_ms(&self) -> engine_core::types::Millis {
        self.0.reached_ms().unwrap_or(0)
    }

    fn instant(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

/// The calls a read of `view` calls for at `now`, the caller's clock in Unix seconds, with `signed` the
/// newest signed head the operator's stream carried, judging time by the later of `now` and the block's:
/// none while no turn names the operator; a witness while the current slot is not the latest marked
/// one; a promotion once that time reaches `promotionDueAt`; a relay while the chain's head of the
/// epoch is older than two thirds of an interval and `signed` is newer. The latest mark counts only
/// when it names the current slot itself: a mark made under an earlier interval names a slot in that
/// interval's units, which no comparison of order can relate to this one.
pub(crate) fn calls(view: &View, signed: Option<SignedHead>, now: u64) -> Vec<Call> {
    if !view.turn_names_operator() || view.rule.interval == 0 {
        return Vec::new();
    }
    let time = view.time.max(now);
    let mut calls = Vec::new();
    if view.marked_slot != time / view.rule.interval {
        calls.push(Call::Witness);
    }
    if time >= view.due_at {
        calls.push(Call::Promote);
    }
    let stale = view.head.posted_at.saturating_add(view.rule.interval * 2 / 3) <= time;
    if let Some(signed) =
        signed.filter(|signed| stale && signed.epoch == view.epoch && signed.head.head.seq >= view.head.seq)
        && (signed.head.head.seq > view.head.seq || signed.head.signed_at > view.head.signed_at)
    {
        calls.push(Call::Relay(Box::new(signed)));
    }
    calls
}

#[cfg(test)]
pub(crate) mod tests;
