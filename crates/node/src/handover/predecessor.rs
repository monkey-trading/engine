//! The predecessor's side: a listener that hands this release's keys, and the log segment the
//! final checkpoint is in or, to a successor that follows the log, the entries past its tip, to the
//! vault's current release, after stopping sequencing, and ends once the successor confirms: the
//! node then exits, or, a rotation-set member's, follows its successor.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use engine_verify::Tcb;
use ring::rand::{SecureRandom, SystemRandom};
use sha2::Digest as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::{Instrument, error, info, info_span, warn};

use crate::checkpoints::{CheckpointChain, CheckpointRpc};
use crate::config::{NodeConfig, TeeProvider};
use crate::handover::evidence::{self, Peer, Quoting};
use crate::handover::tail::{TailMiss, read_tail, send_tail, tail_report_data};
use crate::handover::{
    Challenge, Confirm, MESSAGE_TIMEOUT, REPLY_TIMEOUT, Reply, Request, confirm_digest, confirmed_digest,
    keys_report_data, receive, request_report_data, seal_keys, send, send_bytes, vault_reader,
};
use crate::log::{CheckpointDomain, Manifest, SegmentInfo};
use crate::tee::keys::Head;
use crate::tee::sealing::{KeyStore, Platform, Release};
use crate::tee::secrets::SecretStore;
use crate::tee::{Attestor, EnclaveKey};
use crate::trusted_time::TrustedClock;

/// Asks the node to stop sequencing; it answers with the final checkpoint, or why there is none.
pub(crate) type Drain = mpsc::Sender<DrainReply>;

/// The node's answer to a stop request: the drained final checkpoint, or why there is none.
pub(crate) type DrainReply = oneshot::Sender<anyhow::Result<Head>>;

/// The handover listener and what it hands over, reading the vault's current release through `C`.
pub(crate) struct Predecessor<C = CheckpointRpc> {
    listener: TcpListener,
    domain: CheckpointDomain,
    provider: TeeProvider,
    processor: Option<String>,
    kds_base: String,
    store: Arc<KeyStore>,
    attestor: Box<dyn Attestor>,
    signer: AccountId,
    vault: C,
    /// Where the log the final segment is read from lives.
    log_dir: PathBuf,
    trusted: TrustedClock,
    drain: Drain,
    /// Whether the sequencer signed its last checkpoint, rather than withholding it.
    signing: watch::Receiver<bool>,
    /// The delivered secrets, which an approved upgrade takes first.
    secrets: Option<Arc<SecretStore>>,
    /// The sequencer, whose log must have accepted a daily turn before its member gets the keys. Weak,
    /// so the listener never holds the sequencer open past a shutdown's drain.
    engine: Option<mpsc::WeakSender<crate::sequencer::Request>>,
    /// The turn and key the log accepted when this release admitted that turn's member, which a
    /// retry after the stop, when the sequencer is gone, is admitted under.
    logged: std::sync::Mutex<Option<(u64, AccountId)>>,
    /// The log's daily turns as sequencing stopped, which admit a later turn's member once the
    /// sequencer is gone (`ensure_logged`).
    retained: std::sync::Mutex<Option<engine_core::turns::DailyTurns>>,
    /// The log's protocol version and the last tip found on it, which answer a tail request once the
    /// sequencer is gone (`tail_after`).
    tail_evidence: std::sync::Mutex<tail::Evidence>,
}

/// How one connection ended.
enum Served {
    /// The successor confirmed: the node ends.
    Confirmed,
    /// The keys stayed here, or went out unconfirmed.
    Unconfirmed,
}

impl Predecessor {
    /// The listener `[tee.sealing.handover]` asks for, bound; none without the section.
    /// Error cases:
    /// - the section is set without `[checkpoints]` or EVM custody, or the vault client or the
    ///   listener cannot be built
    pub(crate) async fn bind(
        cfg: &NodeConfig,
        store: &Arc<KeyStore>,
        attestor: Box<dyn Attestor>,
        key: &EnclaveKey,
        trusted: TrustedClock,
        drain: Drain,
        signing: watch::Receiver<bool>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(handover) = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref()) else {
            return Ok(None);
        };
        let vault = vault_reader(cfg)?.context("[tee.sealing.handover] needs [checkpoints]")?;
        let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
        let listener = TcpListener::bind(handover.listen)
            .await
            .with_context(|| format!("binding the handover listener {}", handover.listen))?;
        info!(listen = %listener.local_addr()?, "handover listener up");
        Ok(Some(Self {
            listener,
            domain,
            provider: cfg.tee.provider,
            processor: cfg.tee.processor.clone(),
            kds_base: cfg.tee.kds_base.clone(),
            store: store.clone(),
            attestor,
            signer: AccountId(key.address()),
            vault,
            log_dir: cfg.node.log_dir.clone(),
            trusted,
            drain,
            signing,
            secrets: None,
            engine: None,
            logged: std::sync::Mutex::new(None),
            retained: std::sync::Mutex::new(None),
            tail_evidence: Default::default(),
        }))
    }
}

impl<C: CheckpointChain> Predecessor<C> {
    /// Gives the listener the delivered secrets an approved upgrade takes.
    pub(crate) fn with_secrets(mut self, secrets: Option<Arc<SecretStore>>) -> Self {
        self.secrets = secrets;
        self
    }

    /// Gives the listener the sequencer, whose log a daily turn's successor needs (`ensure_logged`).
    pub(crate) fn with_engine(mut self, engine: &mpsc::Sender<crate::sequencer::Request>) -> Self {
        self.engine = Some(engine.downgrade());
        self
    }

    /// Serves successors one connection at a time until one confirms a handover, which ends the
    /// node's run. Once sequencing has stopped it goes on serving them, under `handover_unconfirmed`: the
    /// vault names the successor's release, so this release's own copy of the keys no longer opens,
    /// and only a confirmed handover or the operator ends it.
    /// Error cases:
    /// - the node cannot stop sequencing, or has no final checkpoint to hand over
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        let mut head = None;
        let mut connection = 0_u64;
        loop {
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    warn!("accepting a handover connection failed: {e}");
                    continue;
                }
            };
            connection += 1;
            let span = info_span!("handover", id = connection, peer = %peer);
            match self.serve(stream, &mut head).instrument(span).await? {
                Served::Confirmed => return Ok(()),
                Served::Unconfirmed if head.is_some() => error!(
                    alert = "handover_unconfirmed",
                    "sequencing has stopped and no successor holds the keys yet: this release goes on serving \
                     successors, and stopping it loses the keys, since its own copy no longer opens"
                ),
                Served::Unconfirmed => {}
            }
        }
    }

    /// One successor's request, which binds the challenge this sends first: refused with its
    /// reason, or answered with the keys and, on its confirmation, ended. A refusal or a broken
    /// connection leaves the node as it was.
    /// Error cases:
    /// - the node cannot stop sequencing or has no final checkpoint, which ends the node
    async fn serve(&self, mut stream: TcpStream, head: &mut Option<Head>) -> anyhow::Result<Served> {
        let mut challenge = [0u8; 32];
        if SystemRandom::new().fill(&mut challenge).is_err() {
            return Ok(unconfirmed("the RNG failed"));
        }
        if let Err(e) = send(&mut stream, &Challenge { nonce: hex::encode(challenge) }).await {
            return Ok(unconfirmed(&format!("sending the challenge failed: {e:#}")));
        }
        let request: Request = match timeout(MESSAGE_TIMEOUT, receive(&mut stream)).await {
            Ok(Ok(request)) => request,
            Ok(Err(e)) => return Ok(unconfirmed(&format!("an unreadable request: {e:#}"))),
            Err(_) => return Ok(unconfirmed("no request in time")),
        };
        let parsed = hex::decode(&request.public_key)
            .ok()
            .zip(hex::decode(&request.config_sha256).ok().and_then(|config| <[u8; 32]>::try_from(config).ok()));
        let Some((public_key, config_sha256)) = parsed else {
            return Ok(refuse(&mut stream, anyhow!("the public key or config hash is not hex of its length")).await);
        };
        if request.secrets_only {
            self.serve_secrets(&mut stream, &request, &challenge, &public_key, config_sha256).await;
            return Ok(Served::Unconfirmed);
        }
        let reads = &request.reads;
        let report_data = match &request.tail {
            Some(tip) => tail_report_data(self.domain, &challenge, &public_key, &config_sha256, reads, tip),
            None => request_report_data(self.domain, &challenge, &public_key, &config_sha256, reads),
        };
        let release = match self.admit(&request, &report_data, config_sha256).await {
            Ok(release) => release,
            Err(e) => return Ok(refuse(&mut stream, e).await),
        };
        info!(successor_release = %hex::encode(release.id()), "the vault names the successor's release");
        // A successor whose build cannot read the log's protocol version could not replay what it
        // takes: it is refused here, before the stop, tail or not.
        if let Err(e) = self.ensure_successor_reads(reads).await {
            return Ok(refuse(&mut stream, e).await);
        }
        // A following successor gets the entries past its tip, once the tip is on this log and the
        // tail fits its bound, judged before the stop with a segment frame's room for what is
        // sequenced first. Anything else is refused here, saying why: its copy's segment files are its
        // own writer's, so whole segments never go onto it.
        let room = crate::handover::tail::MAX_TAIL_BYTES - u64::from(crate::handover::MAX_SEGMENT_BYTES);
        let after = match &request.tail {
            Some(tip) => match self.tail_after(tip).await {
                Ok(after) if checks::tail_over(&self.log_dir, after, room) => {
                    return Ok(
                        miss(&mut stream, TailMiss::OverBound, "the tail past the tip may be over its bound").await
                    );
                }
                Ok(after) => Some(after),
                Err(missed) => return Ok(miss(&mut stream, missed, "the successor's tip is not one it can tail").await),
            },
            None => None,
        };
        // The segments the successor lacks must each fit the handover's bound, and the transfer
        // must fit memory: the writer rotates by command count, not bytes, so a runaway tail is
        // refused here, before the stop it could strand. A read error refuses this attempt and
        // keeps the node serving.
        if after.is_none()
            && let Err(e) = self.transfer_bounds(request.log_tip)
        {
            return Ok(refuse(&mut stream, e).await);
        }
        let head = match head {
            Some(head) => head.clone(),
            None => {
                // Once stopped, this release cannot sequence again, so it stops only when it can
                // sign the final checkpoint it hands over.
                if !*self.signing.borrow() {
                    let withheld = anyhow!(
                        "checkpoint signing is withheld, so this release could not sign a final checkpoint: it \
                         goes on sequencing until the checkpoint_withheld alert clears"
                    );
                    return Ok(refuse(&mut stream, withheld).await);
                }
                self.retain_turns().await;
                self.retain_version().await;
                let stopped = self.stop().await;
                match &stopped {
                    // The checkpoint gate refused the final checkpoint: the node goes on sequencing,
                    // so this release refuses the successor and keeps serving.
                    Err(e) if e.downcast_ref::<crate::sequencer::HandoverRefused>().is_some() => {
                        return Ok(refuse(&mut stream, anyhow!("{e:#}")).await);
                    }
                    Err(e) => {
                        error!(
                            alert = "handover_failed",
                            "sequencing stopped with no final checkpoint to hand over, and the vault names the successor, \
                             so this release's keys no longer open: the successor needs new keys, which the timelock \
                             registers: {e:#}"
                        );
                        refuse(&mut stream, anyhow!("{e:#}")).await;
                    }
                    Ok(_) => {}
                }
                head.insert(stopped?).clone()
            }
        };
        // The log past the successor's tip travels with the keys: the listings are bound inside
        // the ciphertext, and each segment's bytes are read, verified, and sent one at a time, so
        // the transfer holds at most one segment in memory. A listing-read failure must not end
        // this node: the keys go with no segments, the successor's strict checks refuse the
        // confirmation, and this release goes on serving.
        let tails = match self.tail_listings(&head, request.log_tip) {
            Ok(tails) => tails,
            Err(e) => {
                warn!("reading the log for the handover failed; sending the keys without it: {e:#}");
                Vec::new()
            }
        };
        match self.hand_over(&mut stream, &public_key, &report_data, head.clone(), tails, after).await {
            Ok(()) => {
                info!(alert_resolved = "handover_unconfirmed", "the successor holds the keys; this release exits");
                Ok(Served::Confirmed)
            }
            Err(e) => Ok(unconfirmed(&format!("{e:#}"))),
        }
    }

    /// Every segment past the successor's tip, newest last: the sealed listings the drain wrote.
    /// Empty when the head already ends the successor's copy.
    /// Error cases:
    /// - the manifest cannot be read
    fn tail_listings(&self, head: &Head, tip: Option<u64>) -> anyhow::Result<Vec<SegmentInfo>> {
        let manifest = Manifest::load(&self.log_dir)?;
        let tails: Vec<_> =
            manifest.segments.iter().filter(|listed| tip.is_none_or(|tip| listed.last_seq > tip)).cloned().collect();
        ensure!(
            tails.last().is_none_or(|listed| listed.last_seq == head.seq),
            "the log's last segment ends at seq {}, not the final seq {}",
            tails.last().map_or(0, |listed| listed.last_seq),
            head.seq
        );
        Ok(tails)
    }

    /// Reads one segment and verifies it against its sealed listing.
    /// Error cases:
    /// - the segment cannot be read, or the bytes differ from the listing
    fn segment_bytes(&self, listed: &SegmentInfo) -> anyhow::Result<Vec<u8>> {
        let bytes =
            std::fs::read(self.log_dir.join(&listed.file)).with_context(|| format!("reading {}", listed.file))?;
        ensure!(
            bytes.len() as u64 == listed.bytes && hex::encode(sha2::Sha256::digest(&bytes)) == listed.sha256,
            "the log segment {} differs from its listing",
            listed.file
        );
        Ok(bytes)
    }

    /// The successor's release, the measurement its quote proves and the config hash it binds,
    /// once its quote binds `report_data`, its policy and TCB are no weaker than this release's, and
    /// the vault names that release at finality, or the finalised schedule names it for the turn
    /// trusted time has reached (`turns`) and this release's log accepted it.
    /// Error cases:
    /// - any of those checks fails, or the vault cannot be read
    async fn admit(
        &self,
        request: &Request,
        report_data: &[u8; 64],
        config_sha256: [u8; 32],
    ) -> anyhow::Result<Release> {
        // Sampling stops with sequencing, so a retry after the stop reads the last sample moved on.
        let now = self.trusted.since_sample_ms().map(|ms| ms / 1000);
        let peer = evidence::verify(self.provider, &request.evidence, report_data, now, None)?;
        no_weaker(&self.store.platform(), &peer)?;
        let release = Release { launch_digest: peer.launch_digest, config_sha256 };
        let at = self.vault.observe(crate::checkpoints::View::Finalized, &[]).await?;
        let (_, turn) =
            crate::handover::turns::named_generation(&self.vault, &at, &release, self.trusted.reached_ms()).await?;
        if let Some(turn) = turn {
            self.ensure_logged(&turn).await?;
        }
        Ok(release)
    }

    /// Stops sequencing and returns the final checkpoint.
    /// Error cases:
    /// - the node is already stopping, or has no final checkpoint (see `final_head`)
    /// - the checkpoint gate refused the final checkpoint (`HandoverRefused`): the node goes on
    ///   sequencing
    async fn stop(&self) -> anyhow::Result<Head> {
        info!("stopping sequencing to hand the keys over");
        let (tx, rx) = oneshot::channel();
        self.drain.send(tx).await.map_err(|_| anyhow!("the node is already stopping"))?;
        let head = rx.await.map_err(|_| anyhow!("the node stopped without a final checkpoint"))??;
        info!(seq = head.seq, root = %head.root, "sequencing stopped at the final checkpoint");
        Ok(head)
    }

    /// Sends the keys and `head` encrypted to `public_key`, with this enclave's quote over them,
    /// the segments past the successor's tip named inside the ciphertext, and each segment's bytes
    /// behind them as one raw frame per listing — or, with `after`, the entries the log holds past
    /// that seq, listed inside the ciphertext and sent as raw frames — and waits for the successor to
    /// sign `report_data`'s confirmation with the signing key.
    /// Error cases:
    /// - the tail cannot be read (see `tail::read_tail`)
    /// - encryption, quoting, or the exchange fails, or the confirmation is missing or not the key's
    async fn hand_over(
        &self,
        stream: &mut TcpStream,
        public_key: &[u8],
        report_data: &[u8; 64],
        head: Head,
        tails: Vec<SegmentInfo>,
        after: Option<u64>,
    ) -> anyhow::Result<()> {
        let tail = after.map(|after| read_tail(&self.log_dir, after, &head)).transpose()?;
        let tails = if tail.is_some() { Vec::new() } else { tails };
        let mut keys = self.store.keys();
        keys.predecessor_head = Some(head);
        keys.handover_tails = tails.clone();
        keys.handover_tail = tail.as_ref().map(|(listing, _)| listing.clone());
        // Certificates stay with the member whose hostname they name (`tee::certificates`), so one an
        // earlier release sealed with the keys does not leave with them.
        keys.certificate = None;
        let (encapped_key, ciphertext) = seal_keys(self.domain, public_key, report_data, &keys)?;
        let bound = keys_report_data(self.domain, report_data, &encapped_key, &ciphertext);
        let quoting =
            Quoting { provider: self.provider, processor: self.processor.as_deref(), kds_base: &self.kds_base };
        let evidence = evidence::produce(&quoting, self.attestor.as_ref(), &self.store.platform(), bound).await?;
        let keys =
            Reply::Keys { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
        send(stream, &keys).await?;
        for listed in &tails {
            let bytes = self.segment_bytes(listed)?;
            send_bytes(stream, &bytes).await?;
            info!(file = %listed.file, "sent a log segment with the keys");
        }
        if let Some((listing, bytes)) = &tail {
            send_tail(stream, bytes).await?;
            info!(
                entries = listing.entries,
                bytes = listing.bytes,
                after = listing.after,
                "sent the tail past the successor's tip"
            );
        }
        info!("sent the keys to the successor");
        // The successor replays the whole restored log before it seals and confirms, so the wait
        // covers a replay, not one message.
        let confirm: Confirm = timeout(REPLY_TIMEOUT, receive(stream)).await.context("no confirmation in time")??;
        let signature: Signature = confirm.signature.parse().context("the confirmation is not a signature")?;
        let digest = B256::from(confirm_digest(self.domain, report_data));
        ensure!(eip712::recover(&digest, &signature) == Some(self.signer), "the confirmation is not the signing key's");
        let key = self.store.keys().enclave_key()?;
        send(stream, &Reply::Confirmed { signature: key.sign_digest(confirmed_digest(self.domain, report_data)) }).await
    }
}

/// Requires a peer, a successor or a member verifying the log, to run under the same guest policy as
/// this release, since the launch measurement does not cover the policy and one that allowed
/// debugging would expose the keys or its replay, and, on hardware, at a TCB no lower than this
/// platform's committed one.
/// Error cases:
/// - the policy differs, or the TCB is below the floor
pub(crate) fn no_weaker(own: &Platform, peer: &Peer) -> anyhow::Result<()> {
    ensure!(
        peer.policy == own.policy,
        "the peer's guest policy {:#x} differs from this release's {:#x}",
        peer.policy,
        own.policy
    );
    if let Some(tcb) = peer.tcb {
        let floor = Tcb::from_packed(own.tcb);
        ensure!(tcb.meets(floor), "the peer's TCB {tcb:?} is below this platform's committed {floor:?}");
    }
    Ok(())
}

/// Refuses the request with `reason`, which the successor reports.
async fn refuse(stream: &mut TcpStream, reason: anyhow::Error) -> Served {
    warn!("refusing the handover: {reason:#}");
    if let Err(e) = send(stream, &Reply::Refused { reason: format!("{reason:#}") }).await {
        warn!("sending the refusal failed: {e:#}");
    }
    Served::Unconfirmed
}

/// Refuses a tail request before the stop, saying why: the node keeps the keys and sequences on.
async fn miss(stream: &mut TcpStream, miss: TailMiss, reason: &str) -> Served {
    warn!(?miss, "refusing the tail handover: {reason}");
    if let Err(e) = send(stream, &Reply::Missed { reason: reason.into(), miss }).await {
        warn!("sending the refusal failed: {e:#}");
    }
    Served::Unconfirmed
}

/// Logs a connection that ended without a confirmed handover.
fn unconfirmed(why: &str) -> Served {
    warn!("the handover connection ended unconfirmed: {why}");
    Served::Unconfirmed
}

mod checks;
mod secrets;
mod tail;

#[cfg(test)]
mod tests;
