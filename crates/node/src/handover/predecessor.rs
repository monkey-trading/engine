//! The predecessor's side: a listener that hands this release's keys, and the log segment the
//! final checkpoint is in, to the vault's current release, after stopping sequencing, and ends the
//! node once the successor confirms.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, anyhow, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use engine_verify::Tcb;
use ring::rand::{SecureRandom, SystemRandom};
use sha2::Digest as _;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::{Instrument, error, info, info_span, warn};

use crate::checkpoints::{CheckpointChain, CheckpointRpc};
use crate::config::{NodeConfig, TeeProvider};
use crate::handover::evidence::{self, Peer, Quoting};
use crate::handover::{
    Challenge, Confirm, MESSAGE_TIMEOUT, Reply, Request, confirm_digest, confirmed_digest, current_release,
    ensure_named, keys_report_data, receive, request_report_data, seal_keys, send, send_bytes, vault_reader,
};
use crate::log::{Checkpoint, CheckpointDomain, Manifest, SegmentInfo};
use crate::sequencer::{StreamBody, StreamMsg};
use crate::tee::keys::Head;
use crate::tee::sealing::{KeyStore, Platform, Release};
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
        }))
    }
}

impl<C: CheckpointChain> Predecessor<C> {
    /// Serves successors one connection at a time until one confirms a handover, which ends the
    /// node. Once sequencing has stopped it goes on serving them, under `handover_unconfirmed`: the
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
        let report_data = request_report_data(self.domain, &challenge, &public_key, &config_sha256);
        let release = match self.admit(&request, &report_data, config_sha256).await {
            Ok(release) => release,
            Err(e) => return Ok(refuse(&mut stream, e).await),
        };
        info!(successor_release = %hex::encode(release.id()), "the vault names the successor's release");
        // The segments the successor lacks must each fit the handover's bound, and the transfer
        // must fit memory: the writer rotates by command count, not bytes, so a runaway tail is
        // refused here, before the stop it could strand. A read error refuses this attempt and
        // keeps the node serving.
        match self.transfer_bounds(request.log_tip) {
            Ok(()) => {}
            Err(e) => return Ok(refuse(&mut stream, e).await),
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
        // The log past the successor's tip travels with the keys. A read failure must not end
        // this node: the keys go with no segments, the successor's strict checks refuse the
        // confirmation, and this release goes on serving.
        let tails = match self.tail_frames(&head, request.log_tip) {
            Ok(tails) => tails,
            Err(e) => {
                warn!("reading the log for the handover failed; sending the keys without it: {e:#}");
                Vec::new()
            }
        };
        match self.hand_over(&mut stream, &public_key, &report_data, head.clone(), tails).await {
            Ok(()) => {
                info!(alert_resolved = "handover_unconfirmed", "the successor holds the keys; this release exits");
                Ok(Served::Confirmed)
            }
            Err(e) => Ok(unconfirmed(&format!("{e:#}"))),
        }
    }

    /// Every segment past the successor's tip, with the sealed listing and bytes of each: any
    /// completed between the successor's archive fetch and the stop, and the just-sealed tail from
    /// the manifest the drain wrote. The archive serves only completed segments, and the API is
    /// already down when the successor could fetch; sending everything past its tip leaves no seq
    /// missing. Empty when the head already ends the successor's copy.
    /// Error cases:
    /// - the manifest or a segment cannot be read, or the bytes differ from its listing
    fn tail_frames(&self, head: &Head, tip: Option<u64>) -> anyhow::Result<Vec<(SegmentInfo, Vec<u8>)>> {
        let manifest = Manifest::load(&self.log_dir)?;
        let mut frames = Vec::new();
        for listed in manifest.segments.iter() {
            if tip.is_some_and(|tip| listed.last_seq <= tip) {
                continue;
            }
            let bytes =
                std::fs::read(self.log_dir.join(&listed.file)).with_context(|| format!("reading {}", listed.file))?;
            ensure!(
                bytes.len() as u64 == listed.bytes && hex::encode(sha2::Sha256::digest(&bytes)) == listed.sha256,
                "the log segment {} differs from its listing",
                listed.file
            );
            frames.push((listed.clone(), bytes));
        }
        ensure!(
            frames.last().is_none_or(|(listed, _)| listed.last_seq == head.seq),
            "the log's last segment ends at seq {}, not the final seq {}",
            frames.last().map_or(0, |(listed, _)| listed.last_seq),
            head.seq
        );
        Ok(frames)
    }

    /// Whether the segments past `tip` can be handed over: each within the frame bound, and the
    /// set within count and byte budgets the keys message and memory hold. The successor is
    /// expected to hold the rest from the archive; a transfer past these bounds is refused before
    /// the stop, with a message that says to sync the archive first.
    /// Error cases:
    /// - the manifest or the log directory cannot be read
    fn transfer_bounds(&self, tip: Option<u64>) -> anyhow::Result<()> {
        const MAX_TAILS: usize = 1024;
        const MAX_TRANSFER_BYTES: u64 = 8 << 30;
        let manifest = Manifest::load(&self.log_dir)?;
        let mut count = 0_usize;
        let mut bytes = 0_u64;
        for listed in manifest.segments.iter().filter(|listed| tip.is_none_or(|tip| listed.last_seq > tip)) {
            ensure!(
                listed.bytes <= crate::handover::MAX_SEGMENT_BYTES as u64,
                "the log segment {} is over the handover's 64 MiB frame bound: the successor cannot take it",
                listed.file
            );
            count += 1;
            bytes += listed.bytes;
        }
        ensure!(
            count <= MAX_TAILS && bytes <= MAX_TRANSFER_BYTES,
            "the log past seq {tip:?} is {count} segments, {bytes} bytes: beyond the handover's bounds, so sync              the successor's archive copy first"
        );
        let entries = match std::fs::read_dir(&self.log_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.log_dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if crate::log::segment_seq(&name).is_none() || manifest.segments.iter().any(|listed| listed.file == name) {
                continue;
            }
            ensure!(
                entry.metadata()?.len() <= crate::handover::MAX_SEGMENT_BYTES as u64,
                "the open log segment {name} is over the handover's 64 MiB frame bound: rotate it before the handover"
            );
        }
        Ok(())
    }

    /// The successor's release, the measurement its quote proves and the config hash it binds,
    /// once its quote binds `report_data`, its policy and TCB are no weaker than this release's, and
    /// the vault names that release at finality.
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
        ensure_named(current_release(&self.vault).await?, &release)?;
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
    /// behind them as one raw frame per listing, and waits for the successor to sign
    /// `report_data`'s confirmation with the signing key.
    /// Error cases:
    /// - encryption, quoting, or the exchange fails, or the confirmation is missing or not the key's
    async fn hand_over(
        &self,
        stream: &mut TcpStream,
        public_key: &[u8],
        report_data: &[u8; 64],
        head: Head,
        tails: Vec<(SegmentInfo, Vec<u8>)>,
    ) -> anyhow::Result<()> {
        let mut keys = self.store.keys();
        keys.predecessor_head = Some(head);
        keys.handover_tails = tails.iter().map(|(listed, _)| listed.clone()).collect();
        let (encapped_key, ciphertext) = seal_keys(self.domain, public_key, report_data, &keys)?;
        let bound = keys_report_data(self.domain, report_data, &encapped_key, &ciphertext);
        let quoting =
            Quoting { provider: self.provider, processor: self.processor.as_deref(), kds_base: &self.kds_base };
        let evidence = evidence::produce(&quoting, self.attestor.as_ref(), &self.store.platform(), bound).await?;
        let keys =
            Reply::Keys { encapped_key: hex::encode(encapped_key), ciphertext: hex::encode(ciphertext), evidence };
        send(stream, &keys).await?;
        for (listed, bytes) in &tails {
            send_bytes(stream, bytes).await?;
            info!(file = %listed.file, "sent a log segment with the keys");
        }
        info!("sent the keys to the successor");
        let confirm: Confirm = timeout(MESSAGE_TIMEOUT, receive(stream)).await.context("no confirmation in time")??;
        let signature: Signature = confirm.signature.parse().context("the confirmation is not a signature")?;
        let digest = B256::from(confirm_digest(self.domain, report_data));
        ensure!(eip712::recover(&digest, &signature) == Some(self.signer), "the confirmation is not the signing key's");
        let key = self.store.keys().enclave_key()?;
        send(stream, &Reply::Confirmed { signature: key.sign_digest(confirmed_digest(self.domain, report_data)) }).await
    }
}

/// Requires the successor to run under the same guest policy as this release, since the launch
/// measurement does not cover the policy and one that allowed debugging would expose the keys, and,
/// on hardware, at a TCB no lower than this platform's committed one.
/// Error cases:
/// - the policy differs, or the TCB is below the floor
fn no_weaker(own: &Platform, peer: &Peer) -> anyhow::Result<()> {
    ensure!(
        peer.policy == own.policy,
        "the successor's guest policy {:#x} differs from this release's {:#x}",
        peer.policy,
        own.policy
    );
    if let Some(tcb) = peer.tcb {
        let floor = Tcb::from_packed(own.tcb);
        ensure!(tcb.meets(floor), "the successor's TCB {tcb:?} is below this platform's committed {floor:?}");
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

/// Logs a connection that ended without a confirmed handover.
fn unconfirmed(why: &str) -> Served {
    warn!("the handover connection ended unconfirmed: {why}");
    Served::Unconfirmed
}

/// What the sequencer has published since the node started, read from its own stream: its last
/// command and its last checkpoint, which become a handover's final head once it drains. Held in
/// memory, so the host's disk plays no part; a receiver that falls behind skips the oldest entries
/// only, never the newest.
pub(crate) struct Published {
    receiver: broadcast::Receiver<Arc<StreamMsg>>,
    last_seq: Option<u64>,
    checkpoint: Option<Head>,
}

impl Published {
    /// Follows `receiver` on from the recovered log's last command and last checkpoint.
    pub(crate) fn new(
        receiver: broadcast::Receiver<Arc<StreamMsg>>,
        last_seq: Option<u64>,
        last_checkpoint: Option<&Checkpoint>,
    ) -> Self {
        let checkpoint = last_checkpoint.map(|cp| Head { seq: cp.seq, root: cp.root.clone() });
        Self { receiver, last_seq, checkpoint }
    }

    /// The final checkpoint once the sequencer has drained, which publishes everything it wrote
    /// before it returns: the checkpoint of its last command. A withheld final checkpoint is only a
    /// post-commit defense: the stop the node commits to already wrote one, so this error names the
    /// real recovery — new keys through the timelock — since a restart of this release refuses
    /// once the vault names the successor.
    /// Error cases:
    /// - no checkpoint was written, or the drain withheld the last command's (custody not yet
    ///   reconciled), so the keys stay with this release
    pub(crate) fn final_head(mut self) -> anyhow::Result<Head> {
        loop {
            match self.receiver.try_recv() {
                Ok(message) => match &message.body {
                    StreamBody::Command(command) => self.last_seq = Some(command.sequenced.seq),
                    StreamBody::Checkpoint(cp) => self.checkpoint = Some(Head { seq: cp.seq, root: cp.root.clone() }),
                    StreamBody::Withdrawal(_) => {}
                },
                Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let head = self.checkpoint.context("the log has no checkpoint to hand over")?;
        ensure!(
            Some(head.seq) == self.last_seq,
            "the final checkpoint was withheld after seq {}, so the keys stay with this release: the vault \
             already names the successor and a restart of this release refuses, so the successor needs new \
             keys, which the timelock registers",
            head.seq
        );
        Ok(head)
    }
}

#[cfg(test)]
mod tests;
