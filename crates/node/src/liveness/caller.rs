//! A member's liveness caller (#595): from a funded account of its own, apart from the log-head and
//! seed relayers, it reads the chain four times a heartbeat interval, or every `[liveness] read_interval`
//! while it knows no interval (`liveness::read_every`), and while a turn names the operator it
//! - witnesses the current slot once no witness has marked it (`OperatorSchedule.witness`);
//! - promotes once the block time reaches `promotionDueAt` (`OperatorSchedule.promote`), naming the
//!   next member in the turn's order;
//! - skips the turn in effect's current assignment once the block time reaches `skipDueAt`
//!   (`OperatorSchedule.skip`, #596), naming the next member in the turn's order;
//! - relays the operator's newest signed head from its stream when the chain's head is older than five
//!   sixths of an interval, so a head the operator's own relayer fails to post still counts for the
//!   enclave that signed it. A head the stream brings once the last read's head has aged that far is
//!   read for and relayed at once: the next read, a quarter of an interval on, could come after its
//!   signing time has left the head tolerance.
//!
//! The same account fixes turn fallbacks for the node's whole life (`liveness::fallback`), one call at a
//! time with these (`Sends`).
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
//! `liveness_caller_unfunded`, as does a send the endpoint refuses for funds; the fallback fixer reads
//! the balance too, for the node's whole life, in one episode with the caller's (`check_balance`).

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
use tokio::time::{MissedTickBehavior, interval, interval_at, timeout};
use tracing::{debug, info, warn};

use crate::config::{CheckpointSection, LivenessSection, redact};
use crate::liveness::chain::{LivenessDomain, View, promoteCall, skipCall, view, witnessCall};
use crate::liveness::fallback::{Account, Sends};
use crate::liveness::link::Copy;
use crate::liveness::read_every;
use crate::liveness::unowned::{KeyHolderRefused, head_release, names_unknown_key_holder};
use crate::log_heads::{Head, SignedHead};
use crate::standby_key::chain::StandbyChain;

/// How often the caller reads until a read shows the heartbeat interval: a failed first read must not
/// wait out a long configured interval.
const UNREAD_EVERY: Duration = Duration::from_secs(5);
/// What a call's gas limit adds to its estimate.
const GAS_MARGIN_DIVISOR: u64 = 4;

/// What the caller decides to send after a read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    Witness,
    Promote,
    Skip,
    /// A signed head, relayed.
    Relay(Box<SignedHead>),
}

/// How the caller sends: estimated first, then sent from its own account.
pub(crate) trait Sending: Send + Sync {
    /// Sends `data` to `to` once its estimate succeeds, returning the transaction hash, or `None` when
    /// the estimate shows the chain would refuse it.
    /// Error cases:
    /// - the send fails or outruns its bound
    /// - the estimate reverts with `KeyHolderUnknown` (`KeyHolderRefused`)
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
    /// Its read period while it knows no heartbeat interval: `[liveness] read_interval`.
    pub poll: Duration,
    /// The caller's own clock, which it judges slots by when it is ahead of the latest block: `Reached`
    /// over the member's trusted time.
    pub clock: std::sync::Arc<dyn crate::clock::Clock>,
    /// Held for each send, shared with the member's fallback fixer, which sends from the same account.
    pub sends: std::sync::Arc<Sends>,
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
            Err(e) if names_unknown_key_holder(&format!("{e:#}")) => return Err(KeyHolderRefused.into()),
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
        let mut poll = interval(self.poll.min(UNREAD_EVERY));
        poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let (mut read_failed, mut failing, mut unowned) = (false, false, false);
        // When the last read's head turns stale for a relay, and that read's block time: a head the stream
        // brings from then on is read for at once, by the later of that block time and the clock.
        let mut stale_at: Option<(u64, u64)> = None;
        loop {
            let woken = tokio::select! {
                _ = poll.tick() => false,
                () = self.copy.signed_arrived() => true,
            };
            let now = u64::try_from(self.clock.now_ms() / 1000).unwrap_or(0);
            if woken && !stale_at.is_some_and(|(at, time)| now.max(time) >= at) {
                continue;
            }
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
            stale_at = (view.turn_names_operator() && view.rule.interval != 0).then(|| (relay_from(&view), view.time));
            // Four reads a slot, however the rule's interval moves.
            let every = read_every(self.poll, view.rule.interval);
            if poll.period() != every {
                poll = interval_at(tokio::time::Instant::now() + every, every);
                poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
            }
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
                skip_due_at = view.skip_due_at,
                head_seq = view.head.seq,
                posted_at = view.head.posted_at,
                signed = ?signed.as_ref().map(|signed| (signed.epoch, signed.head.head.seq, signed.head.signed_at)),
                ?decided,
                "the liveness caller read the chain"
            );
            if unowned && !decided.iter().any(|call| matches!(call, Call::Skip | Call::Promote)) {
                unowned = false;
                info!(alert_resolved = "liveness_key_holder_unknown", "no skip or promotion is due");
            }
            for call in decided {
                let mut account = self.sends.lock().await;
                self.send(&call, view.epoch, (&mut account.unfunded, &mut failing, &mut unowned)).await;
            }
            // An unreadable balance is part of the `liveness_caller_send_failed` episode.
            if let Err(e) = check_balance(&self.sending, self.min_balance, &mut *self.sends.lock().await).await
                && !std::mem::replace(&mut failing, true)
            {
                warn!(alert = "liveness_caller_send_failed", "the liveness caller's balance is unreadable: {e:#}");
            }
        }
    }

    /// Sends `call`, logging what happened: a send that fails for funds pages `liveness_caller_unfunded`,
    /// and any other failure `liveness_caller_send_failed`, once per episode, which a call the endpoint
    /// estimates resolves. A skip or promotion refused with `KeyHolderUnknown` pages
    /// `liveness_key_holder_unknown` for `epoch`'s newest head's release, until one is not refused.
    async fn send(&self, call: &Call, epoch: u64, (unfunded, failing, unowned): (&mut bool, &mut bool, &mut bool)) {
        let (to, data) = match call {
            Call::Witness => (self.domain.standby.schedule, witnessCall {}.abi_encode()),
            Call::Promote => (self.domain.standby.schedule, promoteCall {}.abi_encode()),
            Call::Skip => (self.domain.standby.schedule, skipCall {}.abi_encode()),
            Call::Relay(signed) => match signed.head.calldata(&signed.signature) {
                Ok(data) => (self.domain.log_heads, data),
                Err(e) => {
                    warn!("a signed head from the stream does not encode: {e:#}");
                    return;
                }
            },
        };
        let sent = self.sending.send(to, data).await;
        let schedule_call = matches!(call, Call::Skip | Call::Promote);
        if sent.as_ref().is_err_and(|e| e.is::<KeyHolderRefused>()) {
            if schedule_call && !std::mem::replace(unowned, true) {
                let release = match self.chain.latest().await {
                    Ok(block) => head_release(&block, &self.domain, epoch).await,
                    Err(e) => Err(e),
                };
                let release = release.map_or_else(|e| format!("(unreadable: {e:#})"), |release| release.to_string());
                warn!(
                    alert = "liveness_key_holder_unknown",
                    "the schedule refuses {call:?}: the newest head's release {release} belongs to no candidate, so no \
                     skip or promotion can pass until a member takes the keys or the timelock binds the release"
                );
            }
            return;
        }
        if schedule_call && sent.is_ok() && std::mem::take(unowned) {
            info!(alert_resolved = "liveness_key_holder_unknown", "the schedule no longer refuses {call:?}");
        }
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
}

/// Reads the liveness caller's balance through `sending` and warns while it is below `min_balance`, once
/// per episode `account` holds, which the member's caller and fallback fixer share.
/// Error cases:
/// - the read fails or outruns its bound
pub(crate) async fn check_balance<S: Sending>(
    sending: &S,
    min_balance: U256,
    account: &mut Account,
) -> anyhow::Result<()> {
    let balance = sending.balance().await?;
    if balance < min_balance {
        if !std::mem::replace(&mut account.unfunded, true) {
            warn!(
                alert = "liveness_caller_unfunded",
                "the liveness caller holds {} ETH, below {}: witnesses, promotions, relays and fallback fixes \
                 stop when it cannot pay for gas",
                format_ether(balance),
                format_ether(min_balance)
            );
        }
    } else if std::mem::take(&mut account.unfunded) {
        info!(alert_resolved = "liveness_caller_unfunded", "the liveness caller holds {} ETH", format_ether(balance));
    }
    Ok(())
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
/// one; a promotion once that time reaches `promotionDueAt`; a skip once it reaches `skipDueAt`; a relay while the chain's head of the
/// epoch is older than five sixths of an interval, past the half interval at which the operator's own
/// heads are due, and `signed` is newer. The latest mark counts only
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
    if time >= view.skip_due_at {
        calls.push(Call::Skip);
    }
    let stale = relay_from(view) <= time;
    if let Some(signed) =
        signed.filter(|signed| stale && signed.epoch == view.epoch && signed.head.head.seq >= view.head.seq)
        && (signed.head.head.seq > view.head.seq || signed.head.signed_at > view.head.signed_at)
    {
        calls.push(Call::Relay(Box::new(signed)));
    }
    calls
}

/// When the chain's head of `view` turns stale enough to relay a newer one: five sixths of an interval
/// after it landed, past the half interval at which the operator's own heads are due.
fn relay_from(view: &View) -> u64 {
    view.head.posted_at.saturating_add(view.rule.interval * 5 / 6)
}

#[cfg(test)]
pub(crate) mod tests;
